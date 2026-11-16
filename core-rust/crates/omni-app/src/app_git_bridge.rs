//! Git over the mesh — the transport of `docs/decisions/omni-workspace-git.md` (phase 1).
//!
//! Serving: an inbound bulk stream that starts with [`GIT_BRIDGE_MAGIC`] names a workspace
//! repository (`<group_id>:<repo>`); a node of a member of that group gets `git upload-pack` on our
//! published bare repository, its stdin and stdout piped through the stream.
//!
//! Fetching ([`fetch_member_branches`]): git speaks its own pack protocol end to end, we only move
//! bytes. The core opens a one-shot entry on `127.0.0.1` guarded by a token and runs
//! `git fetch "ext::<omni-git-helper> --relay <entry> <token> %S"`; the helper joins git's pipes to
//! the entry, the core joins the entry to a bulk stream to the member's node.
//!
//! Fetch only: nobody writes into another node's repository (`receive-pack` is never served).
//!
//! A files project is not git: its content goes on its own streams (`app_workspace_content`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use omni_mesh_runtime::{BulkStream, MeshNodeAddress};
use omni_types::{OmniError, OmniResult};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Refs only the account's own devices are served («на мои устройства»): the branches sent to them, and
/// the copies of the other devices' ones. Anyone else's `upload-pack` neither lists nor gives them.
pub(crate) const OWN_DEVICES_ONLY_REFS: [&str; 2] = ["refs/omni/mine", "refs/omni/devices"];

/// First bytes of a bulk stream that asks for a repository.
pub(crate) const GIT_BRIDGE_MAGIC: [u8; 4] = *b"OGIT";
/// `git upload-pack --timeout`: a peer that stops talking frees the process.
const GIT_UPLOAD_PACK_TIMEOUT_SECS: u32 = 120;
/// How long git gets to reach the one-shot entry after it was started.
const ENTRY_WAIT: Duration = Duration::from_secs(30);
const ENTRY_POLL: Duration = Duration::from_millis(20);
/// The token line the helper sends first (64 hex digits and a newline; anything longer is not it).
const TOKEN_LINE_MAX: usize = 128;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

fn runtime() -> OmniResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| OmniError::Transport(format!("git bridge IO runtime: {e}")))
}

/// `git` with no console window flashing up under the GUI.
pub(crate) fn git_command() -> Command {
    let mut command = Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// The request after the magic: one length byte, then `<group_id>:<repo>`.
fn encode_request(group_id: &str, repo: &str) -> Vec<u8> {
    let what = format!("{group_id}:{repo}");
    let mut request = Vec::with_capacity(5 + what.len());
    request.extend_from_slice(&GIT_BRIDGE_MAGIC);
    request.push(what.len() as u8); // a group id (32 hex) and a repository name (≤ 64) fit
    request.extend_from_slice(what.as_bytes());
    request
}

/// The rest of an inbound stream that began with [`GIT_BRIDGE_MAGIC`]: read which repository, and if
/// `peer` is a member's node of that workspace group and we publish the repository there, run
/// `git upload-pack` on the stream. A refusal just closes the stream; the asker's git reports the
/// early end.
pub(crate) fn serve_git_stream(db_path: &str, peer: MeshNodeAddress, bulk: BulkStream) {
    let outcome = (|| -> OmniResult<String> {
        let BulkStream { send, mut recv } = bulk;
        let rt = runtime()?;
        let mut len = [0_u8; 1];
        rt.block_on(recv.read_exact(&mut len)).map_err(|e| OmniError::Transport(format!("git request: {e}")))?;
        let mut what = vec![0_u8; usize::from(len[0])];
        rt.block_on(recv.read_exact(&mut what)).map_err(|e| OmniError::Transport(format!("git request: {e}")))?;
        let what = String::from_utf8(what).map_err(|_| OmniError::InvalidInput("git request is not UTF-8".into()))?;
        let (group_id, repo) = what.split_once(':').ok_or_else(|| OmniError::InvalidInput("git request is malformed".into()))?;
        omni_protocol::validate_workspace_repo_name(repo)?;
        let (path, own_device) = crate::app_workspace::published_repo_for_member_node(db_path, group_id, repo, &peer)?;
        // Asked again at the level it was opened at: a stream that shows our own devices' refs
        // goes on only while the peer is still one of our live devices — membership alone let a
        // revoked device of ours keep reading them (blind audit 2026-11-12, pass 17).
        let mut right = crate::app_serve_right::ServeRight::new(|| {
            let (_, still_own) = crate::app_workspace::published_repo_for_member_node(db_path, group_id, repo, &peer)?;
            if own_device && !still_own {
                return Err(OmniError::InvalidInput("no longer one of our devices".into()));
            }
            Ok(())
        });
        let sent = serve_upload_pack(&path, BulkStream { send, recv }, !own_device, &mut right)?;
        Ok(format!("group={group_id} repo={repo} own_device={own_device} sent={sent}"))
    })();
    match outcome {
        Ok(served) => crate::omni_app_trace_log("git/serve", &format!("peer={peer:?} {served}")),
        Err(error) => crate::omni_app_trace_log("git/serve", &format!("peer={peer:?} refused reason={error}")),
    }
}

/// `git upload-pack` on `path` with the stream as its stdin and stdout; the bytes sent back. With
/// `hide_own`, the refs of [`OWN_DEVICES_ONLY_REFS`] are neither listed nor given (git refuses a want
/// it did not advertise). The asker's right is asked again as the pack goes out (`right`); gone, the
/// upload-pack is killed and the stream closed.
pub(crate) fn serve_upload_pack(path: &Path, bulk: BulkStream, hide_own: bool, right: &mut crate::app_serve_right::ServeRight<'_>) -> OmniResult<u64> {
    let BulkStream { mut send, mut recv } = bulk;
    let rt = runtime()?;
    let mut command = git_command();
    if hide_own {
        for prefix in OWN_DEVICES_ONLY_REFS {
            command.arg("-c").arg(format!("uploadpack.hideRefs={prefix}"));
        }
    }
    let mut child = command
        .arg("upload-pack")
        .arg("--strict")
        .arg(format!("--timeout={GIT_UPLOAD_PACK_TIMEOUT_SECS}"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| OmniError::Transport(format!("cannot run git upload-pack: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    // Peer → upload-pack, on its own thread; the end of the peer's stream closes stdin.
    let inbound = std::thread::Builder::new()
        .name("omni-git-in".to_string())
        .spawn(move || {
            let Ok(rt) = runtime() else { return };
            let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
            while let Ok(n) = rt.block_on(recv.read(&mut buf)) {
                if n == 0 || stdin.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| OmniError::Transport(format!("git bridge thread: {e}")))?;
    // upload-pack → peer, here; its exit ends our half of the stream.
    let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
    let mut sent: u64 = 0;
    let mut right_ended = None;
    loop {
        let n = stdout.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        if let Err(error) = right.before_sending(n as u64) {
            right_ended = Some(error);
            break;
        }
        if rt.block_on(send.write_all(&buf[..n])).is_err() {
            break;
        }
        sent += n as u64;
    }
    let _ = rt.block_on(send.shutdown());
    let _ = child.kill();
    let _ = child.wait();
    let _ = inbound.join();
    match right_ended {
        Some(error) => Err(error),
        None => Ok(sent),
    }
}

/// Quote one argument of an `ext::` command line: git splits it at spaces, `% ` is a space kept and
/// `%%` a percent sign.
fn ext_arg(arg: &str) -> String {
    arg.replace('%', "%%").replace(' ', "% ")
}

/// Fetch `refspec` of the repository `repo` of `group_id` from a member's node into the published bare
/// repository `published` (what the member no longer has is pruned): a member's branches into its
/// space, or a device of ours' branches sent to our devices. `open` gives a bulk stream to a node; the
/// first that opens carries the fetch. The bytes that came back.
pub(crate) fn fetch_member_branches(
    published: &Path,
    group_id: &str,
    repo: &str,
    refspec: &str,
    nodes: &[MeshNodeAddress],
    open: &dyn Fn(MeshNodeAddress) -> OmniResult<BulkStream>,
) -> OmniResult<u64> {
    let launch = crate::helper_launch("git")?;
    let entry = TcpListener::bind("127.0.0.1:0").map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
    entry.set_nonblocking(true).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
    let addr = entry.local_addr().map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
    let token = hex::encode(omni_crypto::generate_group_key());
    let mut ext = String::from("ext::");
    ext.push_str(&ext_arg(&launch.program.to_string_lossy()));
    for arg in &launch.prefix_args {
        ext.push(' ');
        ext.push_str(&ext_arg(arg));
    }
    ext.push_str(&format!(" --relay {addr} {token} %S"));
    let mut child = git_command()
        .arg("-c")
        .arg("protocol.ext.allow=always")
        .arg("-C")
        .arg(published)
        .args(["fetch", "--quiet", "--prune", "--no-tags", "--no-write-fetch-head"])
        .arg(&ext)
        .arg(refspec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| OmniError::Transport(format!("cannot run git fetch: {e}")))?;
    let mut stderr = child.stderr.take().expect("piped");
    let said = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stderr.read_to_string(&mut said);
        said
    });
    let relayed = (|| -> OmniResult<u64> {
        let socket = accept_entry(&entry, &mut child)?;
        check_token(&socket, &token)?;
        let mut last_error = OmniError::Transport("no linked node of the member".into());
        for node in nodes {
            match open(*node) {
                Ok(bulk) => return relay_entry(socket, bulk, &encode_request(group_id, repo)),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    })();
    if relayed.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| OmniError::Transport(format!("git fetch: {e}")))?;
    let said = said.join().unwrap_or_default();
    let received = relayed?;
    if !status.success() {
        return Err(OmniError::Transport(format!("git fetch failed ({status}): {}", said.trim())));
    }
    Ok(received)
}

/// The one connection git's helper makes, or an error when git ends or takes too long first.
fn accept_entry(entry: &TcpListener, child: &mut std::process::Child) -> OmniResult<TcpStream> {
    let deadline = Instant::now() + ENTRY_WAIT;
    loop {
        match entry.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
                return Ok(socket);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.try_wait().ok().flatten().is_some() {
                    return Err(OmniError::Transport("git ended before its helper connected".into()));
                }
                if Instant::now() >= deadline {
                    return Err(OmniError::Transport("git's helper did not connect in time".into()));
                }
                std::thread::sleep(ENTRY_POLL);
            }
            Err(error) => return Err(OmniError::Transport(format!("git entry: {error}"))),
        }
    }
}

/// The helper's first line is the token; another local process that found the port gets nothing.
fn check_token(mut socket: &TcpStream, token: &str) -> OmniResult<()> {
    socket.set_read_timeout(Some(ENTRY_WAIT)).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
    let mut line = Vec::with_capacity(TOKEN_LINE_MAX);
    let mut byte = [0_u8; 1];
    while line.len() < TOKEN_LINE_MAX {
        if socket.read(&mut byte).map_err(|e| OmniError::Transport(format!("git entry: {e}")))? == 0 {
            break;
        }
        if byte[0] == b'\n' {
            socket.set_read_timeout(None).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
            return if line == token.as_bytes() { Ok(()) } else { Err(OmniError::InvalidInput("git entry: wrong token".into())) };
        }
        line.push(byte[0]);
    }
    Err(OmniError::InvalidInput("git entry: no token".into()))
}

/// Entry ↔ stream until both ends are done: git's requests out (then the end of them), the pack
/// back (its end ends the helper, and git with it).
fn relay_entry(socket: TcpStream, bulk: BulkStream, request: &[u8]) -> OmniResult<u64> {
    socket.set_nonblocking(true).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
    runtime()?.block_on(async move {
        let socket = tokio::net::TcpStream::from_std(socket).map_err(|e| OmniError::Transport(format!("git entry: {e}")))?;
        let _ = socket.set_nodelay(true);
        let (mut from_git, mut to_git) = socket.into_split();
        let BulkStream { mut send, mut recv } = bulk;
        send.write_all(request).await.map_err(|e| OmniError::Transport(format!("git request: {e}")))?;
        let out = async {
            let copied = tokio::io::copy(&mut from_git, &mut send).await;
            let _ = send.shutdown().await;
            copied
        };
        let back = async {
            let copied = tokio::io::copy(&mut recv, &mut to_git).await;
            let _ = to_git.shutdown().await;
            copied
        };
        let (_, back) = tokio::join!(out, back);
        back.map_err(|e| OmniError::Transport(format!("git stream: {e}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk_pair() -> (BulkStream, BulkStream) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_recv, a_send) = tokio::io::split(a);
        let (b_recv, b_send) = tokio::io::split(b);
        (
            BulkStream { send: Box::new(a_send), recv: Box::new(a_recv) },
            BulkStream { send: Box::new(b_send), recv: Box::new(b_recv) },
        )
    }

    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn built_git_helper() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        exe.parent().and_then(Path::parent).expect("target dir").join(format!("omni-git-helper{}", std::env::consts::EXE_SUFFIX))
    }

    #[test]
    fn an_ext_argument_keeps_its_spaces_and_percent_signs() {
        assert_eq!(ext_arg(r"C:\Program Files\Omni\omni-git-helper.exe"), r"C:\Program% Files\Omni\omni-git-helper.exe");
        assert_eq!(ext_arg("100%"), "100%%");
    }

    /// The whole way of a fetch but the mesh: git → helper → entry → stream → `upload-pack` on the
    /// member's published repository, and the member's branch lands in its space here.
    #[test]
    fn a_members_branches_are_fetched_over_a_stream_into_its_space() {
        if !built_git_helper().exists() {
            eprintln!("SKIP: omni-git-helper not built at {} (scripts/test-core.ps1 builds the workspace)", built_git_helper().display());
            return;
        }
        let root = std::env::temp_dir().join(format!("omni-git-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (theirs, ours) = (root.join("theirs.git"), root.join("ours.git"));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&root, &["init", "--quiet", "--bare", theirs.to_str().unwrap()]);
        git(&root, &["init", "--quiet", "--bare", ours.to_str().unwrap()]);
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(work.join("a.txt"), "a").unwrap();
        git(&work, &["add", "a.txt"]);
        git(&work, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "a"]);
        git(&work, &["push", "--quiet", theirs.to_str().unwrap(), "main", "main:feat/x"]);
        let head = git(&work, &["rev-parse", "HEAD"]);

        git(&work, &["push", "--quiet", theirs.to_str().unwrap(), "main:refs/omni/mine/heads/secret"]);
        let members = "+refs/heads/*:refs/omni/members/acct/heads/*";
        let served = theirs.clone();
        let hidden = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hide_switch = std::sync::Arc::clone(&hidden);
        let open = move |_node: MeshNodeAddress| -> OmniResult<BulkStream> {
            let hide = hide_switch.load(std::sync::atomic::Ordering::SeqCst);
            let (asking, serving) = bulk_pair();
            let served = served.clone();
            std::thread::spawn(move || {
                let BulkStream { send, mut recv } = serving;
                let rt = runtime().unwrap();
                let mut head = [0_u8; 5];
                rt.block_on(recv.read_exact(&mut head)).unwrap();
                assert_eq!(&head[..4], &GIT_BRIDGE_MAGIC);
                let mut what = vec![0_u8; usize::from(head[4])];
                rt.block_on(recv.read_exact(&mut what)).unwrap();
                assert_eq!(what, b"g1:api");
                serve_upload_pack(&served, BulkStream { send, recv }, hide, &mut crate::app_serve_right::ServeRight::new(|| Ok(()))).unwrap();
            });
            Ok(asking)
        };
        let received = fetch_member_branches(&ours, "g1", "api", members, &[MeshNodeAddress::from_bytes([1; 20])], &open).unwrap();
        assert!(received > 0);
        assert_eq!(git(&ours, &["rev-parse", "refs/omni/members/acct/heads/main"]), head);
        assert_eq!(git(&ours, &["rev-parse", "refs/omni/members/acct/heads/feat/x"]), head);

        // A branch the member dropped is pruned from its space on the next fetch.
        git(&work, &["push", "--quiet", theirs.to_str().unwrap(), ":feat/x"]);
        fetch_member_branches(&ours, "g1", "api", members, &[MeshNodeAddress::from_bytes([1; 20])], &open).unwrap();
        assert!(git(&ours, &["for-each-ref", "refs/omni/members/acct/"]).lines().count() == 1);

        // «На мои устройства»: someone else's fetch neither sees nor gets the branch sent to the own
        // devices; an own device does.
        let mine = "+refs/omni/mine/heads/*:refs/omni/devices/d1/heads/*";
        fetch_member_branches(&ours, "g1", "api", mine, &[MeshNodeAddress::from_bytes([1; 20])], &open).unwrap();
        assert!(git(&ours, &["for-each-ref", "refs/omni/devices/"]).is_empty(), "hidden from a member");
        hidden.store(false, std::sync::atomic::Ordering::SeqCst);
        fetch_member_branches(&ours, "g1", "api", mine, &[MeshNodeAddress::from_bytes([1; 20])], &open).unwrap();
        assert_eq!(git(&ours, &["rev-parse", "refs/omni/devices/d1/heads/secret"]), head, "given to an own device");
        let _ = std::fs::remove_dir_all(&root);
    }
}
