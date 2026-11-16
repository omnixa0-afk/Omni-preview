//! `omni-mcp`: an agent's Omni (docs/decisions/omni-workspace-git.md section 5).
//!
//! `omni-mcp --dir <agent folder> --name <name> --owner <owner's account>` runs the agent's own
//! profile in `<dir>` (made on the first run) and speaks MCP — JSON-RPC 2.0, one message per line —
//! on stdin/stdout. The first run also writes the agent's code into `<dir>\agent-code.txt`: the owner's
//! Omni reads it (or the owner pastes it) to pair with the agent and ask it into a workspace.
//!
//! What the agent may do is the core's business, not this server's: an agent is never an admin or a
//! custodian, its branches are `agent/*`, its lines stop at the limits and when the workspace's admin
//! stops all agents. Messages of the thread come to the agent marked: a command only from its owner
//! naming it, everything else is data — never instructions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use omni_app::{AppInitRequest, OmniApp, WorkspaceFilesCommand};
use serde_json::{json, Value};

mod tools;

const PROTOCOL_VERSION: &str = "2025-06-18";
/// How long `omni_wait` waits when the agent does not say: under the usual 60 s tool timeout.
const WAIT_DEFAULT_S: u64 = 50;
const WAIT_MAX_S: u64 = 600;
/// How often the server takes its owner's contact and invitations, and looks for new lines.
const HOUSEKEEPING: Duration = Duration::from_secs(3);
/// How long omni_code_take waits for the project's `main` to come over the mesh.
const TAKE_WAIT: Duration = Duration::from_secs(90);
/// How long a tool call waits for the agent's Omni to open (the first run makes the profile and starts
/// the network).
const OPEN_WAIT: Duration = Duration::from_secs(90);

/// The agent's Omni, opened in the background: the application that runs the server waits only a few
/// seconds for its first answer, and on a first run the profile and the network take longer — the
/// tools then showed up and vanished.
type Opened = Arc<(Mutex<Option<Result<Arc<Mutex<OmniApp>>, String>>>, Condvar)>;

fn opened(slot: &Opened) -> Result<Arc<Mutex<OmniApp>>, String> {
    let (lock, ready) = &**slot;
    let guard = lock.lock().map_err(|_| "the agent's Omni stopped".to_string())?;
    let (guard, _) = ready
        .wait_timeout_while(guard, OPEN_WAIT, |state| state.is_none())
        .map_err(|_| "the agent's Omni stopped".to_string())?;
    guard.clone().unwrap_or_else(|| Err("the agent's Omni is still starting — try again in a minute".into()))
}

struct Args {
    dir: PathBuf,
    name: String,
    owner: String,
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let (mut dir, mut name, mut owner) = (None, None, None);
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => dir = it.next(),
            "--name" => name = it.next(),
            "--owner" => owner = it.next(),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    match (dir, name, owner) {
        (Some(dir), Some(name), Some(owner)) if !name.trim().is_empty() && !owner.trim().is_empty() => {
            Ok(Args { dir: PathBuf::from(dir), name: name.trim().to_string(), owner: owner.trim().to_string() })
        }
        _ => Err("usage: omni-mcp --dir <agent folder> --name <name> --owner <owner's account>".into()),
    }
}

/// The server: its exit code.
pub fn run(args: Vec<String>) -> i32 {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let slot: Opened = Arc::new((Mutex::new(None), Condvar::new()));
    let opening = Arc::clone(&slot);
    std::thread::spawn(move || {
        let result = match open(&args) {
            Ok(app) => {
                let app = Arc::new(Mutex::new(app));
                spawn_housekeeping(Arc::clone(&app));
                Ok(app)
            }
            Err(error) => {
                // Said by every tool call: the agent tells its person (another chat runs this agent).
                eprintln!("omni-mcp: {error}");
                Err(error)
            }
        };
        let (lock, ready) = &*opening;
        if let Ok(mut state) = lock.lock() {
            *state = Some(result);
        }
        ready.notify_all();
    });
    let state = Arc::new(Mutex::new(tools::Cursor::new()));
    serve(&slot, &state);
    if let Some(Ok(app)) = slot.0.lock().ok().and_then(|s| s.clone()) {
        if let Ok(app) = app.lock() {
            tools::stops_listening(&app);
            app.stop_mesh();
        }
    }
    0
}

/// The agent's profile, made on the first run; the mesh started; its code written for the owner.
fn open(args: &Args) -> Result<OmniApp, String> {
    std::fs::create_dir_all(&args.dir).map_err(|e| format!("cannot create {}: {e}", args.dir.display()))?;
    let db_path = args.dir.join("omni.db").to_string_lossy().into_owned();
    let app = OmniApp::init(AppInitRequest { db_path }).map_err(|e| format!("cannot open the profile: {e}"))?;
    let profiles = app.list_local_profiles().map_err(|e| e.to_string())?;
    let profile = match profiles.profiles.first() {
        Some(profile) => profile.profile_id.clone(),
        None => {
            let account = app.create_account(&args.name, "agent").map_err(|e| format!("cannot create the agent's profile: {e}"))?;
            account.local_profile_summary.ok_or("the new profile has no summary")?.profile_id
        }
    };
    app.select_active_profile(&profile).map_err(|e| format!("cannot open the profile: {e}"))?;
    let same = app.agent_self().is_some_and(|me| me.owner == args.owner && me.name == args.name);
    if !same {
        app.agent_init(&args.owner, &args.name).map_err(|e| e.to_string())?;
    }
    app.start_mesh_networked().map_err(|e| format!("cannot start the network: {e}"))?;
    write_code(&app, &args.dir);
    Ok(app)
}

fn write_code(app: &OmniApp, dir: &Path) {
    match app.agent_code() {
        Ok(code) => {
            if let Err(error) = std::fs::write(dir.join(omni_app::AGENT_CODE_FILE), code) {
                eprintln!("omni-mcp: cannot write the agent's code: {error}");
            }
        }
        Err(error) => eprintln!("omni-mcp: no agent code: {error}"),
    }
}

fn spawn_housekeeping(app: Arc<Mutex<OmniApp>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HOUSEKEEPING);
        let Ok(app) = app.lock() else { return };
        let _guard = app.serializer.lock();
        if let Err(error) = app.agent_housekeeping() {
            eprintln!("omni-mcp: {error}");
        }
        // Nobody watches the core's events here: the thread is read from the store.
        let _ = app.drain_events();
    });
}

/// The longest request line (F08, audit 2026-11-09): the tools take text — a task, a comment, a
/// message — far below it; a line without an end is not read into memory without a bound.
const MAX_REQUEST_BYTES: usize = 4 << 20;

/// The next line of `input` (without its end) up to `MAX_REQUEST_BYTES`; `Err(())` for a longer one,
/// which is read to its end and dropped; `None` at the end of the input.
fn next_request(input: &mut impl BufRead) -> Option<Result<String, ()>> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if available.is_empty() {
            return (!line.is_empty() || too_long).then(|| if too_long { Err(()) } else { String::from_utf8(line).map_err(|_| ()) });
        }
        let (part, end) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (&available[..at], Some(at + 1)),
            None => (available, None),
        };
        if !too_long {
            if line.len() + part.len() > MAX_REQUEST_BYTES {
                too_long = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(part);
            }
        }
        let used = end.unwrap_or(available.len());
        input.consume(used);
        if end.is_some() {
            return Some(if too_long { Err(()) } else { String::from_utf8(line).map_err(|_| ()) });
        }
    }
}

/// Calls under way at once at most. Each runs on a thread of its own, so the server keeps reading —
/// a ping, another call, a cancellation — while `omni_wait` waits up to [`WAIT_MAX_S`] (audit
/// 2026-11-09: one wait held the whole server, and the client's cancellation was not even read).
const MAX_CALLS_AT_ONCE: usize = 8;

/// A tool call: its result, given the call's `params` and a flag raised when the client cancels it.
type Call = dyn Fn(&Value, &AtomicBool) -> Value + Send + Sync;

fn serve(app: &Opened, cursor: &Arc<Mutex<tools::Cursor>>) {
    let (app, cursor) = (Arc::clone(app), Arc::clone(cursor));
    let call: Arc<Call> = Arc::new(move |params: &Value, cancel: &AtomicBool| call(&app, &cursor, params, cancel));
    serve_requests(std::io::stdin().lock(), Arc::new(Mutex::new(std::io::stdout())), call);
}

/// Answer the requests of `input` on `out` until the input ends; `tools/call` on threads of their own
/// ([`MAX_CALLS_AT_ONCE`]). A cancelled call is not answered (MCP). At the end of the input the calls
/// still under way are cancelled and waited for: nobody reads their answers any more.
fn serve_requests<W: Write + Send + 'static>(mut input: impl BufRead, out: Arc<Mutex<W>>, call: Arc<Call>) {
    let running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    let send = |message: &Value| {
        if let Ok(mut out) = out.lock() {
            reply(&mut *out, message);
        }
    };
    while let Some(line) = next_request(&mut input) {
        let Ok(line) = line else {
            send(&json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":format!("the request is not UTF-8 or longer than {MAX_REQUEST_BYTES} bytes")}}));
            continue;
        };
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            send(&json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}));
            continue;
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "omni", "version": env!("CARGO_PKG_VERSION")},
                "instructions": tools::INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::list()})),
            "tools/call" => {
                let Some(id) = id else { continue };
                let key = id.to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                let refused = match running.lock() {
                    Ok(now) if now.contains_key(&key) => Some("a call with this id is under way".to_string()),
                    Ok(now) if now.len() >= MAX_CALLS_AT_ONCE => {
                        Some(format!("{MAX_CALLS_AT_ONCE} calls are under way: try again when one ends"))
                    }
                    Ok(mut now) => {
                        now.insert(key.clone(), Arc::clone(&cancel));
                        None
                    }
                    Err(_) => Some("the server stopped".to_string()),
                };
                if let Some(message) = refused {
                    send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}));
                    continue;
                }
                workers.retain(|worker| !worker.is_finished());
                let (out, running, call) = (Arc::clone(&out), Arc::clone(&running), Arc::clone(&call));
                workers.push(std::thread::spawn(move || {
                    let result = call(&params, &cancel);
                    if let Ok(mut now) = running.lock() {
                        now.remove(&key);
                    }
                    if !cancel.load(Ordering::SeqCst) {
                        if let Ok(mut out) = out.lock() {
                            reply(&mut *out, &json!({"jsonrpc":"2.0","id":id,"result":result}));
                        }
                    }
                }));
                continue;
            }
            "notifications/cancelled" => {
                let asked = params.get("requestId").map(Value::to_string);
                if let (Some(asked), Ok(now)) = (asked, running.lock()) {
                    if let Some(flag) = now.get(&asked) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                continue;
            }
            _ if id.is_none() => continue, // a notification (initialized, …): nothing to answer
            other => Err(json!({"code": -32601, "message": format!("unknown method {other}")})),
        };
        let Some(id) = id else { continue };
        send(&match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        });
    }
    if let Ok(now) = running.lock() {
        for flag in now.values() {
            flag.store(true, Ordering::SeqCst);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}

fn reply(out: &mut impl Write, message: &Value) {
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

/// One tool call: its text, or the error as the tool's text with `isError`.
fn call(slot: &Opened, cursor: &Arc<Mutex<tools::Cursor>>, params: &Value, cancel: &AtomicBool) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let app = match opened(slot) {
        Ok(app) => app,
        Err(error) => return json!({"content":[{"type":"text","text": error}], "isError": true}),
    };
    let app = &app;
    let outcome = match name {
        "omni_wait" => wait(app, cursor, &args, cancel),
        "omni_code_take" => take(app, &args, cancel),
        _ => once(app, name, &args),
    };
    match outcome {
        Ok(value) => json!({"content":[{"type":"text","text": text(&value)}], "isError": false}),
        Err(error) => json!({"content":[{"type":"text","text": error}], "isError": true}),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn once(app: &Arc<Mutex<OmniApp>>, name: &str, args: &Value) -> Result<Value, String> {
    let app = app.lock().map_err(|_| "the agent's Omni stopped".to_string())?;
    let _guard = app.serializer.lock();
    tools::call(&app, name, args)
}

/// A code project taken into the agent's folder, and its shared `main` waited for (it comes over the
/// mesh after the take) and checked out — not an empty folder handed back. The profile is not held
/// between looks.
fn take(app: &Arc<Mutex<OmniApp>>, args: &Value, cancel: &AtomicBool) -> Result<Value, String> {
    let mut taken = once(app, "omni_code_take", args)?;
    let until = Instant::now() + TAKE_WAIT;
    while taken["main"] == json!("no_main") && Instant::now() < until && !cancel.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(2));
        taken["main"] = once(app, "omni_code_pull", args)?;
    }
    taken["note"] = json!(match taken["main"].as_str() {
        Some("no_main") => "The code is still coming: call omni_code_pull in a minute.",
        // A folder taken before is left as it is: fetched only, nothing of the agent's is touched.
        Some("not_on_main") => "Fetched; the folder stays on its current branch.",
        Some("dirty") => "Fetched; the folder has uncommitted changes, so main was not moved.",
        Some("diverged") => "Fetched; the folder's main went another way than the shared one: merge it yourself.",
        _ => "main is checked out.",
    });
    Ok(taken)
}

/// New lines of the thread, waited for without holding the profile between looks; a cancelled wait
/// ends at its next look.
fn wait(app: &Arc<Mutex<OmniApp>>, cursor: &Arc<Mutex<tools::Cursor>>, args: &Value, cancel: &AtomicBool) -> Result<Value, String> {
    let seconds = args.get("timeout_s").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT_S).min(WAIT_MAX_S);
    let until = Instant::now() + Duration::from_secs(seconds);
    {
        let app = app.lock().map_err(|_| "the agent's Omni stopped".to_string())?;
        let _guard = app.serializer.lock();
        tools::listening(&app, args, seconds);
    }
    loop {
        let items = {
            let app = app.lock().map_err(|_| "the agent's Omni stopped".to_string())?;
            let _guard = app.serializer.lock();
            tools::fresh(&app, &mut cursor.lock().unwrap_or_else(|p| p.into_inner()), args)?
        };
        if !items.is_empty() || Instant::now() >= until || cancel.load(Ordering::SeqCst) {
            return Ok(json!({"messages": items, "note": tools::DATA_NOTE}));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A files-project command as the shell sends it.
pub(crate) fn files_command(app: &OmniApp, json: Value) -> Result<Value, String> {
    let command: WorkspaceFilesCommand = serde_json::from_value(json).map_err(|e| e.to_string())?;
    let details = app.prepare_workspace_files_command(command).and_then(|job| job.run()).map_err(|e| e.to_string())?;
    serde_json::to_value(details).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call that waits until cancelled (or 10 s) when its tool is `omni_wait`, else answers at once.
    fn fake_calls() -> Arc<Call> {
        Arc::new(|params: &Value, cancel: &AtomicBool| {
            if params["name"] == "omni_wait" {
                let until = Instant::now() + Duration::from_secs(10);
                while !cancel.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            json!({"tool": params["name"]})
        })
    }

    fn served(lines: &[Value]) -> (Vec<Value>, Duration) {
        let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let out = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        serve_requests(input.as_bytes(), Arc::clone(&out), fake_calls());
        let elapsed = started.elapsed();
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        (text.lines().map(|line| serde_json::from_str(line).unwrap()).collect(), elapsed)
    }

    /// Audit 2026-11-09: a long call (`omni_wait`) does not hold the server. A ping sent after it is
    /// answered at once, and the client's cancellation is read and ends the call, unanswered (MCP).
    #[test]
    fn a_long_call_does_not_hold_the_server_and_a_cancelled_one_ends() {
        // The client's end stays open between its lines, as a real one does.
        struct Feed(std::sync::mpsc::Receiver<Vec<u8>>, Vec<u8>);
        impl std::io::Read for Feed {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1.is_empty() {
                    match self.0.recv() {
                        Ok(bytes) => self.1 = bytes,
                        Err(_) => return Ok(0),
                    }
                }
                let n = buf.len().min(self.1.len());
                buf[..n].copy_from_slice(&self.1[..n]);
                self.1.drain(..n);
                Ok(n)
            }
        }
        let (client, feed) = std::sync::mpsc::channel::<Vec<u8>>();
        let out = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let out = Arc::clone(&out);
            std::thread::spawn(move || serve_requests(std::io::BufReader::new(Feed(feed, Vec::new())), out, fake_calls()))
        };
        let say = |line: Value| client.send(format!("{line}\n").into_bytes()).unwrap();
        let answers = || -> Vec<Value> {
            String::from_utf8(out.lock().unwrap().clone()).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
        };
        let answered = |id: Value| {
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                if answers().iter().any(|a| a["id"] == id) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };

        say(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"omni_wait"}}));
        say(json!({"jsonrpc":"2.0","id":2,"method":"ping"}));
        assert!(answered(json!(2)), "the ping is answered while the wait goes on");
        say(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"omni_tasks"}}));
        assert!(answered(json!(3)), "another call is answered while the wait goes on");
        let started = Instant::now();
        say(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}));
        say(json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
        assert!(answered(json!(4)));
        drop(client);
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "the wait was cancelled, not sat out");
        assert!(answers().iter().all(|a| a["id"] != 1), "a cancelled call is not answered: {:?}", answers());
    }

    /// At most MAX_CALLS_AT_ONCE calls run at once — one more is refused with a reason — and the end
    /// of the input ends the calls still under way: the server does not sit out their waits.
    #[test]
    fn calls_past_the_cap_are_refused_and_the_end_of_input_ends_the_rest() {
        let mut lines: Vec<Value> = (1..=MAX_CALLS_AT_ONCE)
            .map(|id| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"omni_wait"}}))
            .collect();
        lines.push(json!({"jsonrpc":"2.0","id":"one-more","method":"tools/call","params":{"name":"omni_wait"}}));
        let (answers, elapsed) = served(&lines);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert_eq!(answers[0]["id"], "one-more");
        assert!(answers[0]["error"]["message"].as_str().unwrap().contains("under way"));
    }

    /// F08 (audit 2026-11-09): a request line past the cap is dropped whole (an error answers it),
    /// and the next request is read as it was sent.
    #[test]
    fn a_request_past_the_cap_is_dropped_and_the_next_one_read() {
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"a\":1}\r\n");
        input.extend(std::iter::repeat_n(b'x', MAX_REQUEST_BYTES + 5));
        input.extend_from_slice(b"\n{\"b\":2}");
        let mut reader = std::io::BufReader::with_capacity(4096, input.as_slice());
        assert_eq!(next_request(&mut reader), Some(Ok("{\"a\":1}\r".to_string())));
        assert_eq!(next_request(&mut reader), Some(Err(())));
        assert_eq!(next_request(&mut reader), Some(Ok("{\"b\":2}".to_string())), "the last line needs no end");
        assert_eq!(next_request(&mut reader), None);
    }

    #[test]
    fn the_server_needs_its_folder_name_and_owner() {
        assert!(parse_args(vec!["--dir".into(), "x".into(), "--name".into(), "Robo".into()]).is_err());
        let args = parse_args(["--dir", "x", "--name", " Robo ", "--owner", "acct-1"].map(String::from).to_vec()).unwrap();
        assert_eq!((args.name.as_str(), args.owner.as_str()), ("Robo", "acct-1"));
    }
}
