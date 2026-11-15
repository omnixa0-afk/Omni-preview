//! Content of a files project over the mesh (docs/decisions/workspace-files-storage.md): version
//! records and file contents, each by its SHA-256.
//!
//! An inbound bulk stream that starts with [`CONTENT_MAGIC`] asks for one object of a project from an
//! offset; a member's node gets it when the gate of `custody::object_to_serve` lets it, and the asker
//! checks the hash before keeping it. Content of 1 MiB and more goes by blocks from several holders at
//! once ([`blocks`], [`blocks::BLOCKS_MAGIC`]). The store the objects are kept in is [`store`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use omni_mesh_runtime::{BulkStream, MeshNodeAddress};
use omni_types::{OmniError, OmniResult};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "app_workspace_content_store.rs"]
mod store;
pub(crate) use store::{configure_store, existing_object_path, object_path, remove_object, store_path, store_roots};
#[path = "app_workspace_blocks.rs"]
pub(crate) mod blocks;

/// Opens a bulk stream to a member's node; called from several threads when content comes by blocks.
pub(crate) type OpenStream<'a> = dyn Fn(MeshNodeAddress) -> OmniResult<BulkStream> + Sync + 'a;

/// First bytes of a bulk stream that asks for one object of a files project, whole.
pub(crate) const CONTENT_MAGIC: [u8; 4] = *b"OCNT";
const COPY_BUFFER_BYTES: usize = 64 * 1024;

fn runtime() -> OmniResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| OmniError::Transport(format!("content IO runtime: {e}")))
}

/// `magic`, one length byte, then `<group_id>:<repo>`.
fn encode_request(magic: [u8; 4], group_id: &str, repo: &str) -> Vec<u8> {
    let what = format!("{group_id}:{repo}");
    let mut request = Vec::with_capacity(5 + what.len());
    request.extend_from_slice(&magic);
    request.push(what.len() as u8); // a group id (32 hex) and a project name (≤ 64) fit
    request.extend_from_slice(what.as_bytes());
    request
}

/// An object id: 64 lowercase hex digits (SHA-256).
pub(crate) fn is_content_id(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// After the magic: one length byte, `<group_id>:<repo>`, the object id (64 bytes) and the offset to
/// start from (u64 LE). The answer: one byte (0 — here, 1 — not here), then the object's size (u64 LE)
/// and its bytes from the offset.
fn encode_content_request(group_id: &str, repo: &str, oid: &str, offset: u64) -> Vec<u8> {
    let mut request = encode_request(CONTENT_MAGIC, group_id, repo);
    request.extend_from_slice(oid.as_bytes());
    request.extend_from_slice(&offset.to_le_bytes());
    request
}

/// The rest of an inbound stream that began with [`CONTENT_MAGIC`]: a member's node gets the object
/// of that project from us, if we have it and it may.
pub(crate) fn serve_content_stream(db_path: &str, peer: MeshNodeAddress, bulk: BulkStream) {
    let outcome = (|| -> OmniResult<String> {
        let BulkStream { send, mut recv } = bulk;
        let rt = runtime()?;
        let mut len = [0_u8; 1];
        rt.block_on(recv.read_exact(&mut len)).map_err(|e| OmniError::Transport(format!("content request: {e}")))?;
        let mut what = vec![0_u8; usize::from(len[0]) + 64 + 8];
        rt.block_on(recv.read_exact(&mut what)).map_err(|e| OmniError::Transport(format!("content request: {e}")))?;
        let (name, rest) = what.split_at(usize::from(len[0]));
        let name = std::str::from_utf8(name).map_err(|_| OmniError::InvalidInput("content request is not UTF-8".into()))?;
        let (group_id, repo) = name.split_once(':').ok_or_else(|| OmniError::InvalidInput("content request is malformed".into()))?;
        let oid = std::str::from_utf8(&rest[..64]).unwrap_or_default().to_string();
        let offset = u64::from_le_bytes(rest[64..72].try_into().expect("8 bytes"));
        omni_protocol::validate_workspace_repo_name(repo)?;
        if !is_content_id(&oid) {
            return Err(OmniError::InvalidInput("content request names no object".into()));
        }
        let (path, _) = crate::app_workspace::published_repo_for_member_node(db_path, group_id, repo, &peer)?;
        let object = match crate::app_workspace::custody::object_to_serve(db_path, group_id, repo, &peer, &oid, &path) {
            Ok(object) => object,
            Err(error) if error.to_string().contains(crate::app_workspace::custody::CONTENT_NOT_HERE) => {
                let mut send = send;
                let _ = rt.block_on(async {
                    send.write_all(&[1]).await?;
                    send.shutdown().await
                });
                return Ok(format!("group={group_id} repo={repo} oid={oid}: not here"));
            }
            Err(error) => return Err(error),
        };
        // The right asked again is the object's own: membership alone let a reviewer whose role
        // was taken keep reading content nobody accepted (blind audit 2026-11-12, pass 16).
        let mut right = crate::app_serve_right::ServeRight::new(|| {
            crate::app_workspace::published_repo_for_member_node(db_path, group_id, repo, &peer)?;
            crate::app_workspace::custody::object_to_serve(db_path, group_id, repo, &peer, &oid, &path).map(|_| ())
        });
        let sent = serve_object(&object, offset, BulkStream { send, recv }, &mut right)?;
        Ok(format!("group={group_id} repo={repo} oid={oid} offset={offset} sent={sent}"))
    })();
    match outcome {
        Ok(served) => crate::omni_app_trace_log("workspace/serve", &format!("peer={peer:?} {served}")),
        Err(error) => crate::omni_app_trace_log("workspace/serve", &format!("peer={peer:?} refused reason={error}")),
    }
}

/// Answer one content request with the object at `path` from `offset` (or "not here"); the bytes sent.
/// The asker's right is asked again as the bytes go out (`right`).
pub(crate) fn serve_object(path: &Path, offset: u64, bulk: BulkStream, right: &mut crate::app_serve_right::ServeRight<'_>) -> OmniResult<u64> {
    use std::io::Seek as _;
    let BulkStream { mut send, recv: _recv } = bulk;
    let rt = runtime()?;
    let Ok(mut file) = std::fs::File::open(path) else {
        let _ = rt.block_on(async {
            send.write_all(&[1]).await?;
            send.shutdown().await
        });
        return Ok(0);
    };
    let size = file.metadata().map_err(|e| OmniError::Storage(format!("content: {e}")))?.len();
    file.seek(std::io::SeekFrom::Start(offset.min(size))).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let mut head = vec![0_u8];
    head.extend_from_slice(&size.to_le_bytes());
    rt.block_on(send.write_all(&head)).map_err(|e| OmniError::Transport(format!("content stream: {e}")))?;
    let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
    let mut sent = 0_u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
        if n == 0 {
            break;
        }
        right.before_sending(n as u64)?;
        rt.block_on(send.write_all(&buf[..n])).map_err(|e| OmniError::Transport(format!("content stream: {e}")))?;
        sent += n as u64;
    }
    let _ = rt.block_on(send.shutdown());
    Ok(sent)
}

/// The rest of an inbound stream that began with [`blocks::BLOCKS_MAGIC`]: a block list or a run of
/// blocks of an object, under the same gate as a whole one.
pub(crate) fn serve_blocks_stream(db_path: &str, peer: MeshNodeAddress, bulk: BulkStream) {
    let outcome = (|| -> OmniResult<String> {
        let BulkStream { send, mut recv } = bulk;
        let rt = runtime()?;
        let request = blocks::read_blocks_request(&mut *recv, &rt)?;
        let (path, _) = crate::app_workspace::published_repo_for_member_node(db_path, &request.group_id, &request.repo, &peer)?;
        let object = crate::app_workspace::custody::object_to_serve(db_path, &request.group_id, &request.repo, &peer, &request.oid, &path)?;
        let mut right = crate::app_serve_right::ServeRight::new(|| {
            crate::app_workspace::published_repo_for_member_node(db_path, &request.group_id, &request.repo, &peer)?;
            crate::app_workspace::custody::object_to_serve(db_path, &request.group_id, &request.repo, &peer, &request.oid, &path).map(|_| ())
        });
        let sent = blocks::serve_blocks_request(&path, &request, &object, send, &rt, &mut right)?;
        Ok(format!("{request} sent={sent}"))
    })();
    if let Err(error) = outcome {
        crate::omni_app_trace_log("workspace/serve", &format!("peer={peer:?} blocks refused reason={error}"));
    }
}

/// Fetch content `oid` of `size` bytes to `target`: by blocks when it is big enough (with `seeds`,
/// older files of the same path, giving the blocks they share), else — or from older nodes — whole.
/// The bytes that came over the network.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_content(
    target: &Path,
    temp_dir: &Path,
    group_id: &str,
    repo: &str,
    oid: &str,
    size: u64,
    seeds: &[PathBuf],
    nodes: &[MeshNodeAddress],
    open: &OpenStream,
) -> OmniResult<u64> {
    ensure_room_for(temp_dir, size)?;
    if size >= blocks::BLOCKS_FROM_BYTES {
        if let Some(fetched) = blocks::fetch_by_blocks(target, temp_dir, group_id, repo, oid, size, seeds, nodes, open)? {
            return Ok(fetched.fetched_bytes);
        }
    }
    fetch_object(target, temp_dir, group_id, repo, oid, size, nodes, open)
}

/// Fetch content `oid` of `size` bytes of the project at `dir` into its store ([`fetch_content`]); one
/// an earlier store holds is moved over instead.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_to_store(
    dir: &Path,
    group_id: &str,
    repo: &str,
    oid: &str,
    size: u64,
    seeds: &[PathBuf],
    nodes: &[MeshNodeAddress],
    open: &OpenStream,
) -> OmniResult<u64> {
    if !is_content_id(oid) {
        return Err(OmniError::InvalidInput("not a content id".into()));
    }
    let target = object_path(dir, oid);
    if target.metadata().is_ok_and(|m| m.is_file() && m.len() == size) {
        return Ok(0);
    }
    // Cut short or damaged on disk: fetched again (a whole one is checked against its hash below).
    let _ = std::fs::remove_file(&target);
    if store::migrate_cached_object(dir, oid, size)? {
        return Ok(0);
    }
    fetch_content(&target, &store_path(dir).join("tmp"), group_id, repo, oid, size, seeds, nodes, open)
}

/// Fetch object `oid` of `size` bytes of a project from the first of `nodes` that has it, to `target`:
/// resumed from what an earlier try left in `temp_dir`, kept only when its SHA-256 is `oid`. The bytes
/// that came now.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_object(
    target: &Path,
    temp_dir: &Path,
    group_id: &str,
    repo: &str,
    oid: &str,
    size: u64,
    nodes: &[MeshNodeAddress],
    open: &OpenStream,
) -> OmniResult<u64> {
    use sha2::Digest as _;
    if !is_content_id(oid) {
        return Err(OmniError::InvalidInput("not a content id".into()));
    }
    std::fs::create_dir_all(temp_dir).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    sweep_abandoned_parts(temp_dir);
    let part = temp_dir.join(format!("{oid}.omni-part"));
    ensure_room_for(temp_dir, size.saturating_sub(std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)))?;
    let rt = runtime()?;
    let mut last_error = OmniError::Transport("no linked member has the content".into());
    for node in nodes {
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let offset = if have > size { 0 } else { have };
        let bulk = match open(*node) {
            Ok(bulk) => bulk,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let BulkStream { mut send, mut recv } = bulk;
        let received = rt.block_on(async {
            send.write_all(&encode_content_request(group_id, repo, oid, offset)).await?;
            let _ = send.shutdown().await;
            let mut head = [0_u8; 1];
            recv.read_exact(&mut head).await?;
            if head[0] != 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let mut len = [0_u8; 8];
            recv.read_exact(&mut len).await?;
            if u64::from_le_bytes(len) != size {
                return Err(std::io::Error::other("the member's object has another size"));
            }
            let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&part)?;
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset))?;
            let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
            let mut got = 0_u64;
            // Never more than the object's size: the hash is checked only at the end, and a member
            // that kept sending past it would fill the disk first.
            let left = size - offset;
            loop {
                let n = match tokio::time::timeout(CONTENT_IDLE_LIMIT, recv.read(&mut buf)).await {
                    Ok(read) => read?,
                    Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the member stopped sending")),
                };
                if n == 0 {
                    break;
                }
                if got + n as u64 > left {
                    return Err(std::io::Error::other(OVERSIZED_CONTENT));
                }
                file.write_all(&buf[..n])?;
                got += n as u64;
            }
            Ok(Some(got))
        });
        match received {
            Ok(None) => last_error = OmniError::Transport("the member does not have the content".into()),
            Ok(Some(got)) => {
                if std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) < size {
                    last_error = OmniError::Transport("the content stream ended early".into());
                    continue;
                }
                let mut hasher = sha2::Sha256::new();
                let mut file = std::fs::File::open(&part).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
                std::io::copy(&mut file, &mut hasher).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
                drop(file);
                if hex::encode(hasher.finalize()) != oid {
                    // A member whose copy is damaged (or that lies) does not stop the fetch: the next
                    // member is asked, from the start (audit 2026-11-09).
                    let _ = std::fs::remove_file(&part);
                    last_error = OmniError::InvalidInput(format!("the content of {oid} did not match its id"));
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
                }
                std::fs::rename(&part, target).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
                return Ok(got);
            }
            Err(error) => {
                // What was sent past the object's size is no part of it: the file goes, not resumed.
                if error.to_string() == OVERSIZED_CONTENT {
                    let _ = std::fs::remove_file(&part);
                }
                last_error = OmniError::Transport(format!("content stream: {error}"));
            }
        }
    }
    Err(last_error)
}

/// How long a half-fetched object is kept for a resume nobody comes back for.
const PART_KEPT_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Remove the half-fetched objects of `temp_dir` untouched for [`PART_KEPT_FOR`]. A part is kept to be
/// resumed, and one of every object a member's versions named and a holder broke off stayed for good
/// (blind audit 2026-11-12, pass 16).
pub(crate) fn sweep_abandoned_parts(temp_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(temp_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if ![".omni-part", ".omni-bpart", ".omni-blocks"].iter().any(|ext| name.ends_with(ext)) {
            continue;
        }
        let stale = entry.metadata().and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age > PART_KEPT_FOR);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Room on the disk for `bytes` more of a project's content, leaving [`CONTENT_KEEP_FREE_BYTES`]:
/// what a version names is fetched whole, and a member's version may name objects bigger than the
/// disk holds — it must not fill it.
fn ensure_room_for(dir: &Path, bytes: u64) -> OmniResult<()> {
    let probe = std::iter::successors(Some(dir), |d| d.parent()).find(|d| d.exists()).unwrap_or(dir);
    let Some(available) = omni_storage::available_space_bytes(probe)? else {
        return Ok(());
    };
    if available < bytes.saturating_add(CONTENT_KEEP_FREE_BYTES) {
        return Err(OmniError::Storage(format!(
            "not enough free disk space for the project's file: {bytes} bytes needed, {available} free"
        )));
    }
    Ok(())
}

/// What a project's content leaves free on the disk.
const CONTENT_KEEP_FREE_BYTES: u64 = 256 * 1024 * 1024;

/// A content stream that sends nothing for this long is given up (the next member is asked).
const CONTENT_IDLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// What a stream that went past the object's size is refused with.
const OVERSIZED_CONTENT: &str = "the member sent more than the object's size";

#[cfg(test)]
mod content_stream_tests {
    use super::*;
    use sha2::Digest as _;

    /// A member that answers any request with `size` as the object's size and then `body`.
    fn member(size: u64, body: Vec<u8>) -> impl Fn(MeshNodeAddress) -> OmniResult<BulkStream> + Sync {
        move |_| {
            let (a, b) = tokio::io::duplex(1 << 16);
            let (a_recv, a_send) = tokio::io::split(a);
            let (mut b_recv, mut b_send) = tokio::io::split(b);
            let body = body.clone();
            std::thread::spawn(move || {
                let rt = runtime().unwrap();
                rt.block_on(async {
                    let mut request = Vec::new();
                    let _ = b_recv.read_to_end(&mut request).await;
                    let _ = b_send.write_all(&[0]).await;
                    let _ = b_send.write_all(&size.to_le_bytes()).await;
                    let _ = b_send.write_all(&body).await;
                    let _ = b_send.shutdown().await;
                });
            });
            Ok(BulkStream { send: Box::new(a_send), recv: Box::new(a_recv) })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omni-content-{name}-{}", hex::encode(&omni_crypto::generate_group_key()[..6])));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Blind audit 2026-11-12, pass 16: a half-fetched object nobody came back for stayed for good.
    /// Untouched for a week it goes; a fresh one and other files stay.
    #[test]
    fn a_part_nobody_came_back_for_goes_after_a_week() {
        let dir = temp_dir("sweep");
        let (old, fresh, other) = (dir.join("a.omni-part"), dir.join("b.omni-bpart"), dir.join("keep.txt"));
        for path in [&old, &fresh, &other] {
            std::fs::write(path, b"x").unwrap();
        }
        let week_and_a_day = std::time::Duration::from_secs(8 * 24 * 3600);
        for path in [&old, &other] {
            std::fs::File::options().write(true).open(path).unwrap().set_modified(std::time::SystemTime::now() - week_and_a_day).unwrap();
        }
        sweep_abandoned_parts(&dir);
        assert!(!old.exists(), "an abandoned part goes");
        assert!(fresh.exists(), "a fresh one stays");
        assert!(other.exists(), "and what is no part stays");
    }

    /// Class A (audit 2026-11-09 #6): the asker's right is asked again as an object goes out; once it
    /// is gone (a member removed from the group) the rest does not go — at most a mebibyte more.
    #[test]
    fn an_object_stops_going_out_once_the_askers_right_is_gone() {
        let dir = temp_dir("right-gone");
        let file = dir.join("object");
        std::fs::write(&file, vec![7_u8; 3 * 1024 * 1024]).unwrap();
        let (a, b) = tokio::io::duplex(1 << 16);
        let (a_recv, a_send) = tokio::io::split(a);
        let (mut b_recv, _b_send) = tokio::io::split(b);
        let reader = std::thread::spawn(move || {
            let rt = runtime().unwrap();
            let mut got = Vec::new();
            let _ = rt.block_on(b_recv.read_to_end(&mut got));
            got.len()
        });
        let checks = std::cell::Cell::new(0);
        let mut right = crate::app_serve_right::ServeRight::new(|| {
            checks.set(checks.get() + 1);
            if checks.get() == 1 { Ok(()) } else { Err(OmniError::InvalidInput("the node is not a member's".into())) }
        });
        let served = serve_object(&file, 0, BulkStream { send: Box::new(a_send), recv: Box::new(a_recv) }, &mut right);
        assert!(served.is_err(), "the stream ends when the right is gone");
        let got = reader.join().unwrap();
        assert!(got <= 9 + 1024 * 1024, "no more than a mebibyte after the last yes: {got}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A member that sends past the size it named is cut off at the first byte too many: the disk
    /// holds no more than the object's size, and the partial file goes.
    #[test]
    fn a_content_stream_longer_than_its_object_is_refused_at_the_first_byte_over() {
        let body = b"sixteen bytes!!!".to_vec();
        let oid = hex::encode(sha2::Sha256::digest(&body));
        let dir = temp_dir("over");
        let nodes = [MeshNodeAddress::from_bytes([1; 20])];
        let flood = member(16, vec![7u8; 200_000]);
        let refused = fetch_object(&dir.join("got"), &dir.join("tmp"), "g", "files", &oid, 16, &nodes, &flood);
        assert!(refused.is_err());
        assert!(!dir.join("tmp").join(format!("{oid}.omni-part")).exists(), "the oversized part is gone");
        assert!(!dir.join("got").exists());

        // An object bigger than the disk holds is not even asked for (Windows tells the free space).
        #[cfg(windows)]
        {
            let asked = std::sync::atomic::AtomicUsize::new(0);
            let counting = |node| {
                asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                member(16, body.clone())(node)
            };
            assert!(fetch_object(&dir.join("huge"), &dir.join("tmp"), "g", "files", &oid, u64::MAX / 2, &nodes, &counting).is_err());
            assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 0, "no stream for what does not fit");
        }

        let honest = member(16, body.clone());
        assert_eq!(fetch_object(&dir.join("got"), &dir.join("tmp"), "g", "files", &oid, 16, &nodes, &honest).unwrap(), 16);
        assert_eq!(std::fs::read(dir.join("got")).unwrap(), body);
        let _ = std::fs::remove_dir_all(dir);
    }
}
