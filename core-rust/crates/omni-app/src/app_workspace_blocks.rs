//! Content of a files project by blocks (docs/decisions/workspace-files-storage.md, stage 4).
//!
//! The asker gets the object's block list from a holder ([`BLOCKS_MAGIC`], op 0), takes from its own
//! files the blocks it already has (the old version of the file in the folder: an edit in place keeps
//! most of them) and fetches the rest from several holders at once (op 1, a run of blocks per stream),
//! each block checked against the list as it comes. The list is a holder's word: the whole is checked
//! against the object's id before it is kept, as a whole-file fetch is.
//!
//! Blocks are of a fixed size chosen by the file's size, as Syncthing's: content-defined chunks were
//! measured on NX saves (2026-11-01) and did not pay — those files are compressed inside and change
//! almost wholly per save. What blocks give every format is several sources and a resume exact to a
//! block.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use omni_mesh_runtime::{BulkStream, MeshNodeAddress};
use omni_types::{OmniError, OmniResult};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{encode_request, is_content_id, runtime, OpenStream};

/// First bytes of a bulk stream that asks for an object's block list or a run of its blocks.
pub(crate) const BLOCKS_MAGIC: [u8; 4] = *b"OBLK";
/// Objects smaller than this come whole, in one request.
pub(crate) const BLOCKS_FROM_BYTES: u64 = 1 << 20;
const BLOCK_MIN: u64 = 128 * 1024;
const BLOCK_MAX: u64 = 16 << 20;
const BLOCKS_TARGET: u64 = 2048;
/// One stream asks for this much in a row.
const RUN_BYTES: u64 = 8 << 20;
/// Holders asked at once.
const SOURCES_MAX: usize = 4;
const OP_LIST: u8 = 0;
const OP_RANGE: u8 = 1;

/// The block size of an object of `size` bytes: the smallest from 128 KiB up that keeps the list at
/// about 2048 blocks; 16 MiB at most.
pub(crate) fn block_size(size: u64) -> u64 {
    let mut block = BLOCK_MIN;
    while block < BLOCK_MAX && size.div_ceil(block) > BLOCKS_TARGET {
        block *= 2;
    }
    block
}

/// The SHA-256 of each block of an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockList {
    pub size: u64,
    pub hashes: Vec<[u8; 32]>,
}

impl BlockList {
    pub fn block(&self) -> u64 {
        block_size(self.size)
    }

    /// Where block `i` is and how long it is.
    pub fn span(&self, i: usize) -> (u64, u64) {
        let at = i as u64 * self.block();
        (at, self.block().min(self.size - at))
    }

    /// The list of the file at `path`, and the SHA-256 of the whole file.
    pub fn of_file(path: &Path) -> OmniResult<(Self, String)> {
        let mut file = std::fs::File::open(path).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
        let size = file.metadata().map_err(|e| OmniError::Storage(format!("content: {e}")))?.len();
        let mut whole = sha2::Sha256::new();
        let mut hashes = Vec::with_capacity(size.div_ceil(block_size(size)) as usize);
        let mut buf = vec![0_u8; block_size(size) as usize];
        let mut left = size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..n]).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
            whole.update(&buf[..n]);
            hashes.push(sha2::Sha256::digest(&buf[..n]).into());
            left -= n as u64;
        }
        Ok((Self { size, hashes }, hex::encode(whole.finalize())))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 32 * self.hashes.len());
        out.extend_from_slice(&self.size.to_le_bytes());
        for hash in &self.hashes {
            out.extend_from_slice(hash);
        }
        out
    }

    /// A list of an object of `size` bytes: one hash per block of [`block_size`].
    pub fn decode(bytes: &[u8], size: u64) -> OmniResult<Self> {
        let bad = || OmniError::InvalidInput("the block list does not fit the object".into());
        let head: [u8; 8] = bytes.get(..8).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
        let rest = &bytes[8..];
        if u64::from_le_bytes(head) != size || rest.len() as u64 != 32 * size.div_ceil(block_size(size)) {
            return Err(bad());
        }
        Ok(Self { size, hashes: rest.as_chunks::<32>().0.to_vec() })
    }
}

/// Where a holder keeps the lists it worked out: by object id, beside the project's data.
fn list_path(published: &Path, oid: &str) -> PathBuf {
    published.join("blocks").join(oid)
}

/// The block list of object `oid` at `path`: worked out once (with the whole checked against `oid`,
/// so a changed file never leaves a wrong list) and kept by id.
fn list_of(published: &Path, oid: &str, path: &Path) -> OmniResult<BlockList> {
    let size = path.metadata().map_err(|e| OmniError::Storage(format!("content: {e}")))?.len();
    let kept = list_path(published, oid);
    if let Ok(list) = std::fs::read(&kept).map_err(|_| ()).and_then(|b| BlockList::decode(&b, size).map_err(|_| ())) {
        return Ok(list);
    }
    let (list, whole) = BlockList::of_file(path)?;
    if whole != oid {
        return Err(OmniError::InvalidInput("the content changed on disk".into()));
    }
    write_atomically(&kept, &list.encode())?;
    Ok(list)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> OmniResult<()> {
    let dir = path.parent().expect("a list has a folder");
    std::fs::create_dir_all(dir).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let temp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("list")));
    std::fs::write(&temp, bytes).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    std::fs::rename(&temp, path).map_err(|e| OmniError::Storage(format!("content: {e}")))
}

/// Forget the list of a removed object.
pub(crate) fn remove_list(published: &Path, oid: &str) {
    let _ = std::fs::remove_file(list_path(published, oid));
}

/// After the magic: one length byte, `<group_id>:<repo>`, the object id (64 bytes), the op, and for a
/// run its offset and length (u64 LE each). The answer: one byte (0 — here, 1 — not here); for the
/// list its length (u32 LE) and bytes, for a run exactly its bytes.
fn encode_blocks_request(group_id: &str, repo: &str, oid: &str, op: u8, offset: u64, len: u64) -> Vec<u8> {
    let mut request = encode_request(BLOCKS_MAGIC, group_id, repo);
    request.extend_from_slice(oid.as_bytes());
    request.push(op);
    request.extend_from_slice(&offset.to_le_bytes());
    request.extend_from_slice(&len.to_le_bytes());
    request
}

/// A request read past its magic.
pub(crate) struct BlocksRequest {
    pub group_id: String,
    pub repo: String,
    pub oid: String,
    op: u8,
    offset: u64,
    len: u64,
}

impl std::fmt::Display for BlocksRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.op {
            OP_LIST => write!(f, "group={} repo={} oid={} list", self.group_id, self.repo, self.oid),
            _ => write!(f, "group={} repo={} oid={} offset={} len={}", self.group_id, self.repo, self.oid, self.offset, self.len),
        }
    }
}

pub(crate) fn read_blocks_request(recv: &mut (dyn tokio::io::AsyncRead + Send + Unpin), rt: &tokio::runtime::Runtime) -> OmniResult<BlocksRequest> {
    let failed = |e: std::io::Error| OmniError::Transport(format!("blocks request: {e}"));
    let mut len = [0_u8; 1];
    rt.block_on(recv.read_exact(&mut len)).map_err(failed)?;
    let mut what = vec![0_u8; usize::from(len[0]) + 64 + 1 + 16];
    rt.block_on(recv.read_exact(&mut what)).map_err(failed)?;
    let (name, rest) = what.split_at(usize::from(len[0]));
    let name = std::str::from_utf8(name).map_err(|_| OmniError::InvalidInput("blocks request is not UTF-8".into()))?;
    let (group_id, repo) = name.split_once(':').ok_or_else(|| OmniError::InvalidInput("blocks request is malformed".into()))?;
    omni_protocol::validate_workspace_repo_name(repo)?;
    let oid = std::str::from_utf8(&rest[..64]).unwrap_or_default().to_string();
    if !is_content_id(&oid) || !matches!(rest[64], OP_LIST | OP_RANGE) {
        return Err(OmniError::InvalidInput("blocks request names no object".into()));
    }
    Ok(BlocksRequest {
        group_id: group_id.to_string(),
        repo: repo.to_string(),
        oid,
        op: rest[64],
        offset: u64::from_le_bytes(rest[65..73].try_into().expect("8 bytes")),
        len: u64::from_le_bytes(rest[73..81].try_into().expect("8 bytes")),
    })
}

/// Answer `request` with object `path` of the project `published` (or "not here"); the bytes sent.
/// The asker's right is asked again as the blocks go out (`right`).
pub(crate) fn serve_blocks_request(
    published: &Path,
    request: &BlocksRequest,
    path: &Path,
    mut send: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    rt: &tokio::runtime::Runtime,
    right: &mut crate::app_serve_right::ServeRight<'_>,
) -> OmniResult<u64> {
    let failed = |e: std::io::Error| OmniError::Transport(format!("blocks stream: {e}"));
    let not_here = |mut send: Box<dyn tokio::io::AsyncWrite + Send + Unpin>| {
        let _ = rt.block_on(async {
            send.write_all(&[1]).await?;
            send.shutdown().await
        });
        Ok(0)
    };
    let Ok(size) = path.metadata().map(|m| m.len()) else { return not_here(send) };
    if request.op == OP_LIST {
        let Ok(list) = list_of(published, &request.oid, path) else { return not_here(send) };
        let bytes = list.encode();
        let mut out = vec![0_u8];
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
        rt.block_on(send.write_all(&out)).map_err(failed)?;
        let _ = rt.block_on(send.shutdown());
        return Ok(out.len() as u64);
    }
    if request.len == 0 || request.offset.checked_add(request.len).is_none_or(|end| end > size) {
        return not_here(send);
    }
    let Ok(mut file) = std::fs::File::open(path) else { return not_here(send) };
    file.seek(SeekFrom::Start(request.offset)).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    rt.block_on(send.write_all(&[0])).map_err(failed)?;
    let mut buf = vec![0_u8; 64 * 1024];
    let mut left = request.len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..n]).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
        right.before_sending(n as u64)?;
        rt.block_on(send.write_all(&buf[..n])).map_err(failed)?;
        left -= n as u64;
    }
    let _ = rt.block_on(send.shutdown());
    Ok(request.len)
}

/// What a fetch by blocks did: blocks taken from files here and bytes fetched, by how many holders.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlocksFetched {
    pub local_bytes: u64,
    pub fetched_bytes: u64,
    pub sources: usize,
}

/// Fetch object `oid` of `size` bytes to `target` by blocks: the blocks `seeds` (older files of the
/// same path) already hold are copied, the rest come from up to four of `nodes` at once; what an
/// earlier try left in `temp_dir` is kept. `None` when no node gives a block list (an older node):
/// the caller fetches the object whole.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_by_blocks(
    target: &Path,
    temp_dir: &Path,
    group_id: &str,
    repo: &str,
    oid: &str,
    size: u64,
    seeds: &[PathBuf],
    nodes: &[MeshNodeAddress],
    open: &OpenStream,
) -> OmniResult<Option<BlocksFetched>> {
    std::fs::create_dir_all(temp_dir).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let (kept_list, part) = (temp_dir.join(format!("{oid}.omni-blocks")), temp_dir.join(format!("{oid}.omni-bpart")));
    // Holders asked for the list who have no object (or speak no blocks) are not asked for blocks.
    let mut passed = Vec::new();
    let list = match std::fs::read(&kept_list).ok().and_then(|b| BlockList::decode(&b, size).ok()) {
        Some(list) => list,
        None => {
            let _ = std::fs::remove_file(&part);
            let asked = nodes.iter().find_map(|node| {
                let list = ask_list(group_id, repo, oid, size, *node, open);
                if list.is_none() {
                    passed.push(*node);
                }
                list
            });
            let Some(list) = asked else { return Ok(None) };
            write_atomically(&kept_list, &list.encode())?;
            list
        }
    };
    let resumed = part.metadata().is_ok_and(|m| m.len() == size);
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&part).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    file.set_len(size).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let mut done = vec![false; list.hashes.len()];
    if resumed {
        check_blocks(&part, &list, &mut done)?;
    }
    let mut outcome = BlocksFetched { local_bytes: copy_from_seeds(&part, &list, &mut done, seeds), ..Default::default() };

    // The missing blocks in runs; each holder takes the next run until none is left or it fails. A
    // run is at most 8 MiB, and small enough that four holders share even a mid-sized file.
    let per_run = ((RUN_BYTES / list.block()) as usize).min((done.len() / (SOURCES_MAX * 4)).max(1));
    let mut runs = VecDeque::new();
    let mut i = 0;
    while i < done.len() {
        if done[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < done.len() && !done[i] && i - start < per_run {
            i += 1;
        }
        runs.push_back((start, i));
    }
    let queue = Mutex::new(runs);
    let fetched = std::sync::atomic::AtomicU64::new(0);
    let sources = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for node in nodes.iter().filter(|n| !passed.contains(n)).take(SOURCES_MAX) {
            let (queue, fetched, sources, list, part) = (&queue, &fetched, &sources, &list, &part);
            scope.spawn(move || {
                let mut served = false;
                while let Some(run) = queue.lock().ok().and_then(|mut q| q.pop_front()) {
                    match fetch_run(group_id, repo, oid, list, part, run, *node, open) {
                        Ok(bytes) => {
                            fetched.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                            served = true;
                        }
                        Err((left, error)) => {
                            if let (Some(left), Ok(mut q)) = (left, queue.lock()) {
                                q.push_front(left);
                            }
                            crate::omni_app_trace_log("workspace/blocks", &format!("group={group_id} repo={repo} {oid} from {node:?}: {error}"));
                            break;
                        }
                    }
                }
                if served {
                    sources.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
    });
    outcome.fetched_bytes = fetched.into_inner();
    outcome.sources = sources.into_inner();
    if queue.into_inner().is_ok_and(|q| !q.is_empty()) {
        return Err(OmniError::Transport("the holders gave only part of the content; the rest comes next time".into()));
    }
    let mut whole = sha2::Sha256::new();
    let mut file = std::fs::File::open(&part).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    std::io::copy(&mut file, &mut whole).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    drop(file);
    if hex::encode(whole.finalize()) != oid {
        // A holder's list did not describe the object: start over, with a list from the next asked.
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&kept_list);
        return Err(OmniError::InvalidInput(format!("the content of {oid} did not match its id")));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    }
    std::fs::rename(&part, target).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let _ = std::fs::remove_file(&kept_list);
    crate::omni_app_trace_log(
        "workspace/blocks",
        &format!("group={group_id} repo={repo} {oid} size={size} local={} fetched={} sources={}", outcome.local_bytes, outcome.fetched_bytes, outcome.sources),
    );
    Ok(Some(outcome))
}

/// The block list of `oid` from `node`, if it has the object and speaks blocks.
fn ask_list(group_id: &str, repo: &str, oid: &str, size: u64, node: MeshNodeAddress, open: &OpenStream) -> Option<BlockList> {
    let rt = runtime().ok()?;
    let BulkStream { mut send, mut recv } = open(node).ok()?;
    let bytes = rt
        .block_on(async {
            send.write_all(&encode_blocks_request(group_id, repo, oid, OP_LIST, 0, 0)).await?;
            let _ = send.shutdown().await;
            let mut head = [0_u8; 1];
            recv.read_exact(&mut head).await?;
            if head[0] != 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let mut len = [0_u8; 4];
            recv.read_exact(&mut len).await?;
            let len = u32::from_le_bytes(len) as usize;
            if len > 8 + 32 * (BLOCKS_TARGET as usize + 1) * 2 {
                return Err(std::io::Error::other("block list too long"));
            }
            let mut bytes = vec![0_u8; len];
            recv.read_exact(&mut bytes).await?;
            Ok(Some(bytes))
        })
        .ok()??;
    BlockList::decode(&bytes, size).ok()
}

/// Mark the blocks of `part` (left by an earlier try) that match the list.
fn check_blocks(part: &Path, list: &BlockList, done: &mut [bool]) -> OmniResult<()> {
    let mut file = std::fs::File::open(part).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
    let mut buf = vec![0_u8; list.block() as usize];
    for (i, done) in done.iter_mut().enumerate() {
        let (_, len) = list.span(i);
        file.read_exact(&mut buf[..len as usize]).map_err(|e| OmniError::Storage(format!("content: {e}")))?;
        *done = sha2::Sha256::digest(&buf[..len as usize]).as_slice() == list.hashes[i];
    }
    Ok(())
}

/// Copy into `part` the missing blocks found at the same places of `seeds`; the bytes copied. A seed
/// that cannot be read gives nothing.
fn copy_from_seeds(part: &Path, list: &BlockList, done: &mut [bool], seeds: &[PathBuf]) -> u64 {
    let mut wanted: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (i, hash) in list.hashes.iter().enumerate().filter(|(i, _)| !done[*i]) {
        wanted.entry(*hash).or_default().push(i);
    }
    let Ok(mut out) = std::fs::OpenOptions::new().write(true).open(part) else { return 0 };
    let mut copied = 0_u64;
    let mut buf = vec![0_u8; list.block() as usize];
    for seed in seeds {
        if wanted.is_empty() {
            break;
        }
        let Ok(mut file) = std::fs::File::open(seed) else { continue };
        let Ok(seed_size) = file.metadata().map(|m| m.len()) else { continue };
        let mut at = 0_u64;
        while at < seed_size && !wanted.is_empty() {
            let n = list.block().min(seed_size - at) as usize;
            if file.read_exact(&mut buf[..n]).is_err() {
                break;
            }
            at += n as u64;
            let hash: [u8; 32] = sha2::Sha256::digest(&buf[..n]).into();
            let Some(places) = wanted.remove(&hash) else { continue };
            for i in places {
                let (offset, len) = list.span(i);
                if len as usize == n && out.seek(SeekFrom::Start(offset)).is_ok() && out.write_all(&buf[..n]).is_ok() {
                    done[i] = true;
                    copied += len;
                }
            }
        }
    }
    copied
}

/// Fetch blocks `run` (start..end) from `node` into `part`, each checked as it comes. On failure,
/// the blocks not written yet (to give to another holder) and why.
#[allow(clippy::too_many_arguments)]
fn fetch_run(
    group_id: &str,
    repo: &str,
    oid: &str,
    list: &BlockList,
    part: &Path,
    (start, end): (usize, usize),
    node: MeshNodeAddress,
    open: &OpenStream,
) -> Result<u64, (Option<(usize, usize)>, OmniError)> {
    let whole = Some((start, end));
    let rt = runtime().map_err(|e| (whole, e))?;
    let BulkStream { mut send, mut recv } = open(node).map_err(|e| (whole, e))?;
    let (offset, _) = list.span(start);
    let (last, last_len) = list.span(end - 1);
    let len = last + last_len - offset;
    let mut out = std::fs::OpenOptions::new().write(true).open(part).map_err(|e| (whole, OmniError::Storage(format!("content: {e}"))))?;
    let mut buf = vec![0_u8; list.block() as usize];
    rt.block_on(async {
        let failed = |at: usize, e: std::io::Error| (Some((at, end)), OmniError::Transport(format!("blocks stream: {e}")));
        send.write_all(&encode_blocks_request(group_id, repo, oid, OP_RANGE, offset, len)).await.map_err(|e| failed(start, e))?;
        let _ = send.shutdown().await;
        let mut head = [0_u8; 1];
        recv.read_exact(&mut head).await.map_err(|e| failed(start, e))?;
        if head[0] != 0 {
            return Err((whole, OmniError::Transport("the member does not have the content".into())));
        }
        for i in start..end {
            let (at, n) = list.span(i);
            let n = n as usize;
            recv.read_exact(&mut buf[..n]).await.map_err(|e| failed(i, e))?;
            if sha2::Sha256::digest(&buf[..n]).as_slice() != list.hashes[i] {
                return Err((Some((i, end)), OmniError::InvalidInput("a block did not match the list".into())));
            }
            out.seek(SeekFrom::Start(at)).and_then(|_| out.write_all(&buf[..n])).map_err(|e| (Some((i, end)), OmniError::Storage(format!("content: {e}"))))?;
        }
        Ok(len)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_size_keeps_the_list_near_two_thousand_blocks() {
        assert_eq!(block_size(0), BLOCK_MIN);
        assert_eq!(block_size(BLOCK_MIN * BLOCKS_TARGET), BLOCK_MIN);
        assert_eq!(block_size(BLOCK_MIN * BLOCKS_TARGET + 1), 2 * BLOCK_MIN);
        assert_eq!(block_size(u64::MAX / 2), BLOCK_MAX);
    }

    /// Holders by node: each serves the objects named by id in its folder, as `serve_blocks_stream`
    /// does past its gate, and counts the runs it gave.
    fn holders(dirs: Vec<(MeshNodeAddress, PathBuf, std::sync::Arc<std::sync::atomic::AtomicUsize>)>) -> impl Fn(MeshNodeAddress) -> OmniResult<BulkStream> + Sync {
        move |node| {
            let (_, dir, runs) = dirs.iter().find(|(n, _, _)| *n == node).cloned().expect("a known node");
            let (a, b) = tokio::io::duplex(1 << 20);
            let (a_recv, a_send) = tokio::io::split(a);
            let (mut b_recv, b_send) = tokio::io::split(b);
            std::thread::spawn(move || {
                let rt = runtime().unwrap();
                let mut magic = [0_u8; 4];
                rt.block_on(b_recv.read_exact(&mut magic)).unwrap();
                assert_eq!(magic, BLOCKS_MAGIC);
                let request = read_blocks_request(&mut b_recv, &rt).unwrap();
                if request.op == OP_RANGE {
                    runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = serve_blocks_request(&dir, &request, &dir.join(&request.oid), Box::new(b_send), &rt, &mut crate::app_serve_right::ServeRight::new(|| Ok(())));
            });
            Ok(BulkStream { send: Box::new(a_send), recv: Box::new(a_recv) })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omni-blocks-{name}-{}", hex::encode(&omni_crypto::generate_group_key()[..6])));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn content_comes_by_blocks_from_several_holders_and_takes_what_the_old_file_shares() {
        // 3 MiB of 128 KiB blocks, each block different; the new file differs from the old in one block.
        let old: Vec<u8> = (0..3u32 << 20).map(|i| ((i / 7) ^ (i >> 17)) as u8).collect();
        let mut new = old.clone();
        new[(5 << 17) + 9] ^= 0xff;
        let oid = hex::encode(sha2::Sha256::digest(&new));
        let (full_a, full_b, empty, here) = (temp_dir("a"), temp_dir("b"), temp_dir("empty"), temp_dir("here"));
        std::fs::write(full_a.join(&oid), &new).unwrap();
        std::fs::write(full_b.join(&oid), &new).unwrap();
        std::fs::write(here.join("part.prt"), &old).unwrap();
        let nodes: Vec<MeshNodeAddress> = (1..=3).map(|n| MeshNodeAddress::from_bytes([n; 20])).collect();
        let counts: Vec<_> = (0..3).map(|_| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))).collect();
        let open = holders(vec![(nodes[0], empty, counts[0].clone()), (nodes[1], full_a, counts[1].clone()), (nodes[2], full_b, counts[2].clone())]);
        let size = new.len() as u64;

        // The old file in the folder gives all but the changed block; a holder without it is passed over.
        let target = here.join("got-1");
        let got = fetch_by_blocks(&target, &here.join("tmp"), "g", "files", &oid, size, &[here.join("part.prt")], &nodes, &open).unwrap().unwrap();
        assert_eq!((got.local_bytes, got.fetched_bytes), (size - (128 << 10), 128 << 10));
        assert_eq!(std::fs::read(&target).unwrap(), new);

        // Without it, both holders that have the content share the fetch.
        let target = here.join("got-2");
        let got = fetch_by_blocks(&target, &here.join("tmp"), "g", "files", &oid, size, &[], &nodes, &open).unwrap().unwrap();
        assert_eq!((got.local_bytes, got.fetched_bytes, got.sources), (0, size, 2));
        assert!(counts[1].load(std::sync::atomic::Ordering::Relaxed) > 1 && counts[2].load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert_eq!(counts[0].load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(std::fs::read(&target).unwrap(), new);

        // A try cut short is resumed from the blocks it kept; no holder speaking blocks: fetched whole.
        let target = here.join("got-3");
        std::fs::write(here.join("tmp").join(format!("{oid}.omni-blocks")), BlockList::of_file(&here.join("got-2")).unwrap().0.encode()).unwrap();
        let mut half = new.clone();
        half[size as usize / 2..].fill(0);
        std::fs::write(here.join("tmp").join(format!("{oid}.omni-bpart")), &half).unwrap();
        let got = fetch_by_blocks(&target, &here.join("tmp"), "g", "files", &oid, size, &[], &nodes, &open).unwrap().unwrap();
        assert_eq!(got.fetched_bytes, size / 2);
        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert!(fetch_by_blocks(&here.join("got-4"), &here.join("tmp"), "g", "files", &oid, size, &[], &nodes[..1], &open).unwrap().is_none());
    }

    #[test]
    fn a_block_list_reads_back_only_for_its_size() {
        let list = BlockList { size: BLOCK_MIN + 1, hashes: vec![[1; 32], [2; 32]] };
        assert_eq!(BlockList::decode(&list.encode(), BLOCK_MIN + 1).unwrap(), list);
        assert!(BlockList::decode(&list.encode(), BLOCK_MIN * 3).is_err());
        assert_eq!(list.span(1), (BLOCK_MIN, 1));
    }
}
