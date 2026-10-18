// Serverless-rendezvous helper management (step 2 of the rendezvous integration —
// docs/decisions/serverless-bootstrap-rendezvous.md §Integration). The network-facing
// rendezvous work (publishing/fetching blinded beacons to public providers, STUN
// reflexive discovery) runs in a SEPARATE, untrusted-by-design process —
// `omni-rendezvous-helper` — so it is sandboxable + per-component proxiable and a
// compromise there can only DoS, never deceive (the core does the seal/topic with the
// per-contact pair_secret and hands the helper only opaque blobs). This module owns the
// helper's lifecycle from the app side and drives it over its line IPC.

/// Locate a helper binary (`stem` without extension). Order:
///   1. `$OMNI_HELPER_DIR` if set — an escape hatch for a host that keeps the helpers somewhere
///      other than next to itself.
///   2. The running exe's own directory, then its parent — the normal multi-file layout (also
///      resolves under `target/<triple>/debug/deps/` during `cargo test`, where bins live one up).
///
/// A single-file host has NO sibling `.exe` and no extraction step, so this fails there by design
/// and `helper_launch` falls through to self-exec.
fn helper_binary_path(stem: &str) -> OmniResult<std::path::PathBuf> {
    let name = format!("{stem}{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = std::env::var_os("OMNI_HELPER_DIR") {
        let candidate = std::path::Path::new(&dir).join(&name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let exe = std::env::current_exe()
        .map_err(|error| OmniError::Internal(format!("current_exe failed: {error}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| OmniError::Internal("current_exe has no parent dir".to_string()))?;
    let candidates = [dir.join(&name), dir.join("..").join(&name)];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .ok_or_else(|| {
            OmniError::Internal(format!(
                "helper '{name}' not found (OMNI_HELPER_DIR unset/empty, and not next to the executable {})",
                dir.display()
            ))
        })
}

/// How to START a helper child: the program to run, plus the args that must precede `--listen …`.
/// Two shapes, both ending in a separate PROCESS (the isolation the helpers exist for):
///   * a sibling `omni-<kind>-helper.exe`, no prefix args — the normal multi-file layout;
///   * self-exec — `<current_exe> --run-helper <kind>` — for a single-file host, which has no
///     sibling on disk (Windows cannot start a process from inside the bundle, and we refuse to
///     extract one: "one file" must stay literally one file). Its startup intercepts the flag and
///     re-enters the same courier code through the FFI (`omni_run_helper`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct HelperLaunch {
    program: std::path::PathBuf,
    prefix_args: Vec<String>,
}

impl HelperLaunch {
    fn binary(program: impl Into<std::path::PathBuf>) -> Self {
        Self {
            program: program.into(),
            prefix_args: Vec::new(),
        }
    }
}

/// Set by a host that can serve `--run-helper <kind>` (the WinUI shell does, at startup). Gated
/// rather than unconditional because self-exec is a CONTRACT: a host without that interception
/// would re-launch its whole self instead of a courier. Sibling binaries still win when present, so
/// a dev multi-file build behaves exactly as before with this set.
const HELPER_SELF_EXEC_ENV: &str = "OMNI_HELPER_SELF_EXEC";

/// Resolve how to launch helper `kind` ("rendezvous" | "dht"): sibling binary first, self-exec as
/// the fallback when the host opted in.
fn helper_launch(kind: &str) -> OmniResult<HelperLaunch> {
    let sibling = helper_binary_path(&format!("omni-{kind}-helper"));
    let sibling_error = match sibling {
        Ok(path) => return Ok(HelperLaunch::binary(path)),
        Err(error) => error,
    };
    if std::env::var_os(HELPER_SELF_EXEC_ENV).is_none_or(|value| value != "1") {
        return Err(sibling_error);
    }
    let exe = std::env::current_exe()
        .map_err(|error| OmniError::Internal(format!("current_exe failed: {error}")))?;
    Ok(HelperLaunch {
        program: exe,
        prefix_args: vec!["--run-helper".to_string(), kind.to_string()],
    })
}

fn rendezvous_helper_launch() -> OmniResult<HelperLaunch> {
    helper_launch("rendezvous")
}

fn dht_helper_launch() -> OmniResult<HelperLaunch> {
    helper_launch("dht")
}

/// Spawn a listener-mode helper child (`--listen 127.0.0.1:0`), windowless on Windows, and read the
/// resolved ephemeral endpoint from its first stderr line (`<listening_prefix><addr>`). Shared by the
/// rendezvous and DHT helpers — identical lifecycle (only the binary name, env, and the line prefix
/// differ). `envs` are extra child env vars (e.g. provider set, DHT endpoint). On a non-listening
/// early line (`cannot listen …`) or EOF before binding, the child is killed and an error returned.
fn spawn_listener_child(
    launch: &HelperLaunch,
    envs: &[(&str, String)],
    listening_prefix: &str,
    what: &str,
) -> OmniResult<(std::process::Child, String, Vec<String>)> {
    use std::io::BufRead;
    let mut command = std::process::Command::new(&launch.program);
    command
        .args(&launch.prefix_args)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    // Never flash a console window (the helpers also FreeConsole defensively).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| OmniError::Internal(format!("spawn {what}: {error}")))?;
    // Страховка на случай, когда до `Drop` дело не дойдёт (статики при выходе процесса не
    // дропаются, плюс у оболочки есть жёсткий backstop). См. app_helper_job.rs.
    crate::app_helper_job::adopt_helper_child(&child, what);

    // The helper prints "<name> listening on <addr>" once bound; read the resolved ephemeral
    // address from that first stderr line.
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| OmniError::Internal(format!("{what} stderr not captured")))?;
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    // Pre-listen lines (e.g. the DHT helper's "UPnP public endpoint …") are handed back so the caller
    // can parse them; the loop ends at the listening line.
    let mut preamble: Vec<String> = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| OmniError::Internal(format!("read {what} stderr: {error}")))?;
        if read == 0 {
            let _ = child.kill();
            return Err(OmniError::Internal(format!(
                "{what} exited before it bound a port"
            )));
        }
        if let Some(addr) = line.trim().strip_prefix(listening_prefix) {
            return Ok((child, addr.to_string(), preamble));
        }
        // Any other early line (e.g. "cannot listen on …") means it failed to start.
        if line.contains("cannot listen") {
            let _ = child.kill();
            return Err(OmniError::Internal(format!(
                "{what} failed to bind: {}",
                line.trim()
            )));
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            preamble.push(trimmed.to_string());
        }
    }
}

/// A spawned `omni-rendezvous-helper` child + a line-IPC client to drive it. The helper
/// binds an ephemeral loopback port; we read the resolved address from its stderr.
///
/// The child is killed on drop — but `Drop` alone is NOT what keeps the helper from outliving the
/// app: this value lives in a static registry, and statics are not dropped at process exit (11
/// orphaned couriers were found this way). The guarantee comes from the job object in
/// `app_helper_job.rs`; `Drop` handles the ordinary cases. См. docs/bugs/rendezvous-helper-outlives-the-app.md.
pub struct RendezvousHelper {
    child: std::process::Child,
    endpoint: String,
    /// What this profile last posted under each pair topic — see [`publish_own_beacon_via`].
    beacon_publishes: BeaconPublishLog,
    /// When this profile last asked the DHT under each pair topic — see [`dht_read_due`].
    dht_reads: DhtReadLog,
    /// What each courier did last (by its label, `omni_rendezvous::courier_label`), from the helper's
    /// per-courier report — what the courier list in Settings shows.
    courier_status: Mutex<HashMap<String, CourierStatus>>,
    /// Where and how often each publication goes — the profile's plans (`app_publish_plans`).
    plans: Mutex<crate::PublishPlansV1>,
    /// (publication key, courier kind) → when a courier of that kind last took that publication:
    /// what a block's frequency counts from. In memory: a restarted profile posts at once.
    taken: Mutex<HashMap<(String, String), u64>>,
}

/// The last thing a courier did, as the helper reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourierStatus {
    /// `ok` (answered — with or without anything for us), `quota` (its allowance is spent), `cooling`
    /// (skipped while it recovers from a failure or a spent allowance), `fail`.
    pub state: String,
    /// The courier's own words for a refusal or a failure.
    pub detail: String,
    pub at_unix_ms: u64,
}

/// The courier states in a helper report: `label=ok+412ms label=quota+30ms:reason label=skip …`.
fn courier_statuses(attribution: &str, now_ms: u64) -> Vec<(String, CourierStatus)> {
    attribution
        .split_whitespace()
        .filter_map(|token| {
            let (label, rest) = token.split_once('=')?;
            let (result, detail) = match rest.split_once(':') {
                Some((result, detail)) => (result, detail.replace('_', " ")),
                None => (rest, String::new()),
            };
            let result = result.split('+').next().unwrap_or(result);
            let state = match result {
                "ok" | "empty" => "ok",
                "quota" => "quota",
                "skip" => "cooling",
                _ => "fail",
            };
            Some((label.to_string(), CourierStatus { state: state.to_string(), detail, at_unix_ms: now_ms }))
        })
        .collect()
}

impl RendezvousHelper {
    /// Spawn the helper bound to an ephemeral loopback port, windowless on Windows.
    pub fn spawn() -> OmniResult<Self> {
        Self::spawn_launch(rendezvous_helper_launch()?, &[], None)
    }

    /// Spawn the helper with an explicit rendezvous-provider set (the app's resolved
    /// `configured_rendezvous_providers()`), handed to the child via `OMNI_RENDEZVOUS_PROVIDERS` so the
    /// helper fans out across the SAME set the app manages (per-profile storage / invite-merged / default)
    /// instead of its own compiled fallback. An empty slice leaves the env untouched (helper uses its own
    /// default). `dht_endpoint` (R7 Phase 2) is the loopback address of this profile's `omni-dht-helper`,
    /// handed to the child via `OMNI_DHT_HELPER_ENDPOINT` so its BT-DHT courier delegates raw put/get
    /// there instead of running the Mainline engine in-process; `None` leaves the env untouched.
    pub fn spawn_with_providers(
        providers: &[String],
        dht_endpoint: Option<&str>,
    ) -> OmniResult<Self> {
        Self::spawn_launch(rendezvous_helper_launch()?, providers, dht_endpoint)
    }

    /// Spawn a specific helper binary (used by tests that locate the cargo-built bin). No provider
    /// override / no DHT endpoint — the helper reads its own `OMNI_RENDEZVOUS_PROVIDERS`/default.
    pub fn spawn_binary(binary: &std::path::Path) -> OmniResult<Self> {
        Self::spawn_binary_with_providers(binary, &[], None)
    }

    /// Spawn a specific helper binary, optionally overriding its provider set via the child's
    /// `OMNI_RENDEZVOUS_PROVIDERS` env (comma-joined) and pointing its BT-DHT courier at an
    /// `omni-dht-helper` via `OMNI_DHT_HELPER_ENDPOINT`. An empty `providers` / `None` endpoint
    /// leaves the respective env untouched.
    pub fn spawn_binary_with_providers(
        binary: &std::path::Path,
        providers: &[String],
        dht_endpoint: Option<&str>,
    ) -> OmniResult<Self> {
        Self::spawn_launch(HelperLaunch::binary(binary), providers, dht_endpoint)
    }

    /// The one spawn body: `launch` says WHICH process shape starts the courier (sibling binary or
    /// self-exec); everything after is identical.
    fn spawn_launch(
        launch: HelperLaunch,
        providers: &[String],
        dht_endpoint: Option<&str>,
    ) -> OmniResult<Self> {
        let mut envs: Vec<(&str, String)> = Vec::new();
        if !providers.is_empty() {
            envs.push(("OMNI_RENDEZVOUS_PROVIDERS", providers.join(",")));
        }
        if let Some(endpoint) = dht_endpoint {
            envs.push(("OMNI_DHT_HELPER_ENDPOINT", endpoint.to_string()));
        }
        let (child, endpoint, _preamble) = spawn_listener_child(
            &launch,
            &envs,
            "omni-rendezvous-helper listening on ",
            "rendezvous helper",
        )?;
        Ok(Self {
            child,
            endpoint,
            beacon_publishes: BeaconPublishLog::default(),
            dht_reads: DhtReadLog::default(),
            courier_status: Mutex::new(HashMap::new()),
            plans: Mutex::new(crate::PublishPlansV1::default()),
            taken: Mutex::new(HashMap::new()),
        })
    }

    /// The loopback endpoint the helper is listening on (host:port).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Publish along these plans (`app_publish_plans`).
    pub fn set_publish_plans(&self, plans: crate::PublishPlansV1) {
        *self.plans.lock().unwrap_or_else(|poison| poison.into_inner()) = plans;
    }

    /// The steps of `publication`'s plan due now for `key` (all of them when `force`) — empty: nothing to post.
    pub(crate) fn due_steps_for(&self, publication: crate::Publication, key: &str, force: bool) -> Vec<Vec<String>> {
        let last: HashMap<String, u64> = self
            .taken
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|((k, _), _)| k == key)
            .map(|((_, kind), at)| (kind.clone(), *at))
            .collect();
        self.plans
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(publication)
            .due_steps(&last, current_unix_ms() as u64, force)
    }

    /// Note which kinds took `key`'s post, from the helper's per-courier report (`nostr:nos.lol=ok+554ms …`).
    fn note_taken(&self, key: &str, attribution: &str) {
        let now = current_unix_ms() as u64;
        let mut taken = self.taken.lock().unwrap_or_else(|poison| poison.into_inner());
        for entry in attribution.split_whitespace() {
            if let Some((label, outcome)) = entry.split_once('=') {
                if outcome.starts_with("ok") {
                    let kind = label.split(':').next().unwrap_or(label);
                    taken.insert((key.to_string(), kind.to_string()), now);
                }
            }
        }
    }

    /// Where a post that is not one of the planned publications goes. For a contact (a revocation
    /// notice): every courier. In the background (a mailbox note): along the contacts' steps, the first
    /// that takes it — whoever looks reads every courier.
    fn spread_for(&self, priority: PublishPriority) -> String {
        match priority {
            PublishPriority::Contact => "all".to_string(),
            PublishPriority::Background => {
                let plans = self.plans.lock().unwrap_or_else(|poison| poison.into_inner());
                let steps: Vec<Vec<String>> = plans
                    .contacts
                    .steps
                    .iter()
                    .map(|step| step.iter().map(|block| block.kind.clone()).collect())
                    .collect();
                crate::app_publish_plans::spread_token(&steps, 1)
            }
        }
    }

    /// A planned publication along its `route` (see [`PublishRoute`]); notes which kinds took it.
    #[allow(clippy::too_many_arguments)] // the IPC command's arguments, one each
    pub(crate) fn publish_routed_among(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        slim_hex: Option<&str>,
        ttl_secs: u32,
        route: &PublishRoute,
        among: Option<&[String]>,
    ) -> OmniResult<()> {
        let spread = route.spread();
        let reply = self.publish_line(topic_hex, dhtkey_hex, blob_hex, ttl_secs, route.priority, among, slim_hex, &spread, true)?;
        let (head, attr) = split_courier_attribution(&reply);
        if let Some(attr) = attr {
            self.note_taken(&route.key, attr);
        }
        publish_reply_accepted(head).map_err(|reason| OmniError::Transport(format!("rendezvous publish: {reason}")))
    }

    /// Publish an opaque sealed `blob` under a blinded `topic` (both hex), with the per-bucket
    /// `dhtkey_hex` seed (the BT-DHT leg keys its slots by it; web couriers ignore it), to the
    /// providers. The core does the seal/topic/key; the helper only carries opaque bytes. `ttl_secs`:
    /// how long the blob is worth keeping (a Nostr relay is told so by NIP-40). `slim_hex`: the same
    /// beacon without its relay bundle, for a courier the full one does not fit (a BT-DHT record).
    #[allow(clippy::too_many_arguments)] // the IPC command's arguments, one each
    pub fn publish(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
        among: Option<&[String]>,
        slim_hex: Option<&str>,
    ) -> OmniResult<()> {
        self.publish_reaching(topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority, among, slim_hex, "all", false)
    }

    /// [`publish`](Self::publish) along `spread` (the helper's token: `all`, or `<reach>:<steps>` — see
    /// `app_publish_plans::spread_token`). See docs/decisions/courier-metadata-and-quota.md, step 7.
    #[allow(clippy::too_many_arguments)] // the IPC command's arguments, one each
    pub fn publish_reaching(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
        among: Option<&[String]>,
        slim_hex: Option<&str>,
        spread: &str,
        replace: bool,
    ) -> OmniResult<()> {
        let reply = self.publish_line(topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority, among, slim_hex, spread, replace)?;
        let (head, _) = split_courier_attribution(&reply);
        publish_reply_accepted(head)
            .map_err(|reason| OmniError::Transport(format!("rendezvous publish: {reason}")))
    }

    /// The `publish` command itself; returns the helper's reply (traced with its per-courier report).
    #[allow(clippy::too_many_arguments)] // the IPC command's arguments, one each
    fn publish_line(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
        among: Option<&[String]>,
        slim_hex: Option<&str>,
        spread: &str,
        replace: bool,
    ) -> OmniResult<String> {
        let reply = self.command(&format!(
            "publish {topic_hex} {dhtkey_hex} {blob_hex} {ttl_secs} {} {} {} {spread} {}",
            priority.as_str(),
            among_token(among),
            slim_hex.unwrap_or("-"),
            if replace { "replace" } else { "keep" }
        ))?;
        // Per-courier visibility for the PUBLISHER (the peer being found by ID): log WHICH courier
        // accepted/dropped the beacon, not just a count. See `attribution` in the helper.
        if let (_, Some(attr)) = split_courier_attribution(&reply) {
            omni_app_trace_log(
                "rendezvous/publish",
                &format!("topic={} couriers: {attr}", topic_prefix(topic_hex)),
            );
        }
        Ok(reply)
    }

    /// Every blob under any of `topic_hexes` (a set read as one — the relay directory's shelves), each
    /// courier asked once for all of its topics (see the helper's `fetch-many`), plus whether any
    /// courier answered.
    pub fn fetch_many_with_reach(&self, topic_hexes: &[String], legs: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
        let reply = self.command(&format!("fetch-many {} {}", legs.as_str(), topic_hexes.join(",")))?;
        let (head, attr) = split_courier_attribution(&reply);
        let mut parts = head.split_whitespace();
        match parts.next() {
            Some("OK") => {
                let blobs: Vec<String> = parts.map(str::to_string).collect();
                let reached = match attr {
                    Some(attr) => {
                        omni_app_trace_log(
                            "rendezvous/fetch-many",
                            &format!("topics={} got={} couriers: {attr}", topic_hexes.len(), blobs.len()),
                        );
                        any_courier_answered(attr)
                    }
                    None => true,
                };
                Ok((blobs, reached))
            }
            _ => Err(OmniError::Transport(format!("rendezvous fetch-many: {reply}"))),
        }
    }

    /// Fetch the opaque blobs (hex) published under `topic` (with the per-bucket `dhtkey_hex` seed for
    /// the BT-DHT leg) across all providers. The core decrypts/validates them — the helper is content-blind.
    pub fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>> {
        self.fetch_with_reach(topic_hex, dhtkey_hex, legs, None)
            .map(|(blobs, _reached)| blobs)
    }

    /// Like [`fetch`], plus whether ANY courier actually answered.
    ///
    /// "No blobs" and "nobody could be asked" are completely different answers, and the
    /// caller that reports to a human must not conflate them: with ntfy failing on every
    /// call (seen live, `ntfy:ntfy.sh=fail`) a lookup that finds nothing would otherwise be
    /// reported as "this person is not findable" when the truth is "we could not look".
    pub fn fetch_with_reach(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        legs: CourierLegs,
        among: Option<&[String]>,
    ) -> OmniResult<(Vec<String>, bool)> {
        let reply = self.command(&format!(
            "fetch {topic_hex} {dhtkey_hex} {} {}",
            legs.as_str(),
            among_token(among)
        ))?;
        // Reply: "OK[ <blob_hex> …]\t<per-courier attribution>". Split OFF the attribution first (the
        // helper appends it after a TAB), then every whitespace token after OK in the head is a blob.
        let (head, attr) = split_courier_attribution(&reply);
        let mut parts = head.split_whitespace();
        match parts.next() {
            Some("OK") => {
                let blobs: Vec<String> = parts.map(str::to_string).collect();
                // Per-courier visibility for the SEARCHER (the one looking up by ID): log WHICH courier
                // was queried, which had the beacon (ok), which came back empty, which was skipped.
                let reached = match attr {
                    Some(attr) => {
                        omni_app_trace_log(
                            "rendezvous/fetch",
                            &format!(
                                "topic={} got={} couriers: {attr}",
                                topic_prefix(topic_hex),
                                blobs.len()
                            ),
                        );
                        any_courier_answered(attr)
                    }
                    // No attribution at all (older helper): assume the lookup was real rather
                    // than inventing an outage.
                    None => true,
                };
                Ok((blobs, reached))
            }
            _ => Err(OmniError::Transport(format!("rendezvous fetch: {reply}"))),
        }
    }


    /// Hold the couriers' subscriptions on `topic_hexes` for up to `wait` and return the blob(s)
    /// published there after the call began — on the first arrival, empty on timeout (`watch` in
    /// the helper). Push instead of poll: see `spawn_beacon_watch_group`.
    pub fn watch(&self, topic_hexes: &[String], wait: std::time::Duration) -> OmniResult<Vec<String>> {
        let line = format!("watch {} {}", wait.as_secs().max(1), topic_hexes.join(" "));
        // The helper answers by `wait` + its own 10 s grace; a little more here so a slow answer is
        // still read rather than cut off.
        let reply = self.command_within(&line, wait + std::time::Duration::from_secs(20))?;
        let (head, attr) = split_courier_attribution(&reply);
        let mut parts = head.split_whitespace();
        match parts.next() {
            Some("OK") => {
                let blobs: Vec<String> = parts.map(str::to_string).collect();
                if !blobs.is_empty() {
                    omni_app_trace_log(
                        "rendezvous/watch",
                        &format!("got={} couriers: {}", blobs.len(), attr.unwrap_or("")),
                    );
                }
                Ok(blobs)
            }
            _ => Err(OmniError::Transport(format!("rendezvous watch: {reply}"))),
        }
    }

    /// One request line → one reply line, over a fresh loopback connection. Bounded by a
    /// read/write timeout so a stuck helper can't wedge a caller.
    fn command(&self, line: &str) -> OmniResult<String> {
        self.command_within(line, std::time::Duration::from_secs(30))
    }

    fn command_within(&self, line: &str, timeout: std::time::Duration) -> OmniResult<String> {
        use std::io::{BufRead, Write};
        let mut stream = std::net::TcpStream::connect(&self.endpoint)
            .map_err(|error| OmniError::Transport(format!("connect helper: {error}")))?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        writeln!(stream, "{line}")
            .map_err(|error| OmniError::Transport(format!("write helper: {error}")))?;
        let mut reader = std::io::BufReader::new(stream);
        let mut reply = String::new();
        reader
            .read_line(&mut reply)
            .map_err(|error| OmniError::Transport(format!("read helper: {error}")))?;
        let reply = reply.trim().to_string();
        if let (_, Some(attribution)) = split_courier_attribution(&reply) {
            let mut status = self.courier_status.lock().unwrap_or_else(|poison| poison.into_inner());
            for (label, latest) in courier_statuses(attribution, current_unix_ms() as u64) {
                // A courier not tried this round keeps what it last did.
                if latest.state == "cooling" && status.contains_key(&label) {
                    continue;
                }
                status.insert(label, latest);
            }
        }
        Ok(reply)
    }

    /// What the courier labelled `label` did last, if this helper has used it.
    pub fn courier_status(&self, label: &str) -> Option<CourierStatus> {
        self.courier_status.lock().unwrap_or_else(|poison| poison.into_inner()).get(label).cloned()
    }
}

/// The couriers a command allows for its topic, as the helper reads them: comma-separated URLs, or
/// `-` for our own set.
fn among_token(among: Option<&[String]>) -> String {
    match among {
        Some(urls) if !urls.is_empty() => urls.join(","),
        _ => "-".to_string(),
    }
}

/// Split a helper reply into its payload head and the OPTIONAL per-courier attribution the helper
/// appends after a TAB (`OK …\t<attr>`). A reply without a TAB (or with an empty tail) → `(reply,
/// None)`, so nothing breaks if the helper ever omits the detail.
fn split_courier_attribution(reply: &str) -> (&str, Option<&str>) {
    match reply.split_once('\t') {
        Some((head, attr)) if !attr.is_empty() => (head, Some(attr)),
        Some((head, _)) => (head, None),
        None => (reply, None),
    }
}

/// Did ANY courier take the beacon? The helper answers `OK <accepted_count>` and says `OK 0` when
/// every courier refused. Reading only the `OK` counted that as a publication: live 2026-10-03 the
/// PC traced `topics=2 published=2` while ntfy answered 429 (quota) and the Nostr relay "pubkey is
/// not in our web of trust" — its fresh beacon reached nobody for four minutes, and the trace said
/// the opposite. The helper is this same exe re-run (`--run-helper`), so the count is always there.
fn publish_reply_accepted(head: &str) -> Result<(), String> {
    let Some(rest) = head.strip_prefix("OK") else {
        return Err(head.to_string());
    };
    match rest.trim().parse::<usize>() {
        Ok(0) => Err("no courier accepted the beacon".to_string()),
        Ok(_) => Ok(()),
        Err(_) => Err(format!("unexpected publish reply {head:?}")),
    }
}

/// First 8 hex chars of an opaque topic, to correlate a publish with its later fetch in the trace
/// without logging the whole topic.
fn topic_prefix(topic_hex: &str) -> &str {
    &topic_hex[..topic_hex.len().min(8)]
}

impl Drop for RendezvousHelper {
    fn drop(&mut self) {
        // The helper must never outlive the app.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A spawned `omni-dht-helper` child (R7 Phase 2): the separate, untrusted-by-design process that
/// owns the Mainline DHT engine. The core does NOT talk to it directly — it is the rendezvous
/// helper's BT-DHT courier that connects over the loopback `endpoint` (handed to that child via
/// `OMNI_DHT_HELPER_ENDPOINT`). The core just owns its lifecycle: spawned once per profile alongside
/// the rendezvous helper and killed on drop, so the DHT engine never outlives the profile. See
/// docs/decisions/omni-dht-helper.md.
pub struct DhtHelper {
    child: std::process::Child,
    endpoint: String,
    /// The node's PUBLIC DHT endpoint (`ip:port`) when running as an ENTRY node with UPnP — captured
    /// from the helper's startup output so the shell can show/share it. `None` if not an entry node, or
    /// UPnP didn't yield a public mapping (no IGD gateway / CGNAT).
    public_endpoint: Option<String>,
}

/// The helper prints this once it maps its DHT port via UPnP (entry-node mode). See omni-dht-helper.
const DHT_UPNP_LINE_PREFIX: &str = "omni-dht-helper UPnP public endpoint ";

impl DhtHelper {
    /// Spawn the DHT helper bound to an ephemeral loopback port, windowless on Windows. `bootstrap`
    /// (local DHT nodes for an isolated network segment; empty → the global Mainline routers) is
    /// handed via `OMNI_DHT_BOOTSTRAP`. `entry_node` = run as a reachable in-segment ENTRY node
    /// (server_mode + fixed port + self-UPnP) so others can bootstrap FROM us.
    pub fn spawn(bootstrap: &[String], extra_nodes: &[String], entry_node: bool) -> OmniResult<Self> {
        Self::spawn_launch(dht_helper_launch()?, bootstrap, extra_nodes, entry_node)
    }

    /// Spawn a specific DHT-helper binary (used by tests that locate the cargo-built bin). An empty
    /// `bootstrap` leaves the env untouched (the helper uses the global routers).
    pub fn spawn_binary(
        binary: &std::path::Path,
        bootstrap: &[String],
        extra_nodes: &[String],
        entry_node: bool,
    ) -> OmniResult<Self> {
        Self::spawn_launch(HelperLaunch::binary(binary), bootstrap, extra_nodes, entry_node)
    }

    /// The one spawn body — see `RendezvousHelper::spawn_launch`.
    fn spawn_launch(
        launch: HelperLaunch,
        bootstrap: &[String],
        extra_nodes: &[String],
        entry_node: bool,
    ) -> OmniResult<Self> {
        let mut envs: Vec<(&str, String)> = Vec::new();
        if !bootstrap.is_empty() {
            envs.push(("OMNI_DHT_BOOTSTRAP", bootstrap.join(",")));
        }
        // ДОПОЛНЕНИЕ к точкам входа по умолчанию, не замена: сюда идёт авто-собранный кэш, который
        // протухает. Подменять им роутеры нельзя — узел, вернувшийся через неделю, остался бы со
        // списком мёртвых адресов и без запасного пути. См. app_dht_rendezvous::dht_startup_extra_nodes.
        if !extra_nodes.is_empty() {
            envs.push(("OMNI_DHT_EXTRA_BOOTSTRAP", extra_nodes.join(",")));
        }
        if entry_node {
            // Serve queries, on a fixed port, self-mapped via UPnP → a reachable bootstrap target.
            envs.push(("OMNI_DHT_SERVER_MODE", "1".to_string()));
            envs.push(("OMNI_DHT_PORT", DHT_ENTRY_NODE_PORT.to_string()));
            envs.push(("OMNI_DHT_UPNP", "1".to_string()));
        }
        let (child, endpoint, preamble) =
            spawn_listener_child(&launch, &envs, "omni-dht-helper listening on ", "DHT helper")?;
        // Capture the UPnP public endpoint (entry-node mode) from the startup preamble, if present.
        let public_endpoint = preamble.iter().find_map(|line| {
            line.strip_prefix(DHT_UPNP_LINE_PREFIX).map(|addr| addr.trim().to_string())
        });
        Ok(Self { child, endpoint, public_endpoint })
    }

    /// The loopback endpoint the DHT helper is listening on (host:port) — passed to the rendezvous
    /// child so its BT-DHT courier delegates raw BEP44 put/get here.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The node's public DHT entry endpoint (`ip:port`) if running as a UPnP-mapped entry node.
    pub fn public_endpoint(&self) -> Option<&str> {
        self.public_endpoint.as_deref()
    }
}

impl Drop for DhtHelper {
    fn drop(&mut self) {
        // The DHT engine must never outlive the profile.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One request line → one reply line to an `omni-dht-helper` over loopback (the same line protocol
/// `BtDhtChannel` speaks). Bounded by a timeout so a stuck helper can't wedge the caller. `None` on any
/// connect / IO error. Used by the DHT-over-peer responder (`dht_proxy_serve`) — R7 Phase 2 mode b.
fn dht_helper_request(endpoint: &str, line: &str) -> Option<String> {
    dht_helper_request_timeout(endpoint, line, std::time::Duration::from_secs(30))
}

/// `dht_helper_request` with an explicit read/write timeout. The DHT-over-peer path wants the long
/// default (a real BEP44 put/get is a network round-trip); the status poll wants a SHORT one — a
/// `nodes` read is a local, instant routing-table dump, and a wedged helper must never freeze the
/// UI-driven poll.
fn dht_helper_request_timeout(
    endpoint: &str,
    line: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::{BufRead, Write};
    let mut stream = std::net::TcpStream::connect(endpoint).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    writeln!(stream, "{line}").ok()?;
    let _ = stream.flush();
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply).ok()?;
    Some(reply.trim().to_string())
}

/// Size of the DHT helper's live Mainline routing table (`nodes` → comma-list of `ip:port`). `Some(0)`
/// = joined-but-empty / still bootstrapping; `None` = IPC failure. Short timeout: a status poll must
/// never block on a wedged helper. This is the honest "connected to the swarm" signal for the UI.
fn dht_helper_nodes_count(endpoint: &str) -> Option<usize> {
    let reply = dht_helper_request_timeout(endpoint, "nodes", std::time::Duration::from_secs(2))?;
    let rest = reply.strip_prefix("OK")?.trim();
    if rest.is_empty() {
        return Some(0);
    }
    Some(rest.split(',').filter(|entry| !entry.trim().is_empty()).count())
}

/// The loopback endpoint of the profile's DHT helper, while it runs.
fn dht_helper_endpoint(db_path: &str) -> Option<String> {
    let registry = rendezvous_runtime_registry().lock().ok()?;
    registry.get(db_path)?.dht_helper.as_ref().map(|helper| helper.endpoint().to_string())
}

/// `announce <info_hash> <port>` on the helper (BEP 5): our IP as the swarm sees it, `port` given.
pub(crate) fn dht_helper_announce(endpoint: &str, info_hash: &[u8; 20], port: u16) -> bool {
    let line = format!("announce {} {port}", to_hex(info_hash));
    matches!(dht_helper_request(endpoint, &line), Some(reply) if reply.starts_with("OK"))
}

/// `peers <info_hash>` on the helper (BEP 5): the addresses announced under it; `None` — IPC failure.
pub(crate) fn dht_helper_peers(endpoint: &str, info_hash: &[u8; 20]) -> Option<Vec<std::net::SocketAddr>> {
    let reply = dht_helper_request(endpoint, &format!("peers {}", to_hex(info_hash)))?;
    let rest = reply.strip_prefix("OK")?.trim();
    Some(rest.split(',').filter_map(|peer| peer.trim().parse().ok()).collect())
}

/// `put key salt value` on the helper — `true` if it accepted (`OK`).
fn dht_helper_put(endpoint: &str, key: &[u8; 32], salt: &[u8], value: &[u8]) -> bool {
    let line = format!("put {} {} {}", to_hex(key), to_hex(salt), to_hex(value));
    matches!(dht_helper_request(endpoint, &line), Some(reply) if reply.starts_with("OK"))
}

/// `get key salt` on the helper. `Ok(Some)` = value, `Ok(None)` = empty slot (`OK` with no value),
/// `Err(())` = IPC failure / malformed reply.
fn dht_helper_get(endpoint: &str, key: &[u8; 32], salt: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    let line = format!("get {} {}", to_hex(key), to_hex(salt));
    match dht_helper_request(endpoint, &line) {
        Some(reply) => {
            let mut parts = reply.split_whitespace();
            match parts.next() {
                Some("OK") => Ok(parts.next().and_then(from_hex)),
                _ => Err(()),
            }
        }
        None => Err(()),
    }
}

/// Per-origin anti-abuse cap on the DHT-proxy responder (masterplan §4 — minimal harm /
/// no-amplification): serve at most [`DHT_PROXY_MAX_SERVES_PER_WINDOW`] ops per origin per
/// [`DHT_PROXY_WINDOW_MS`]. Over the cap we simply don't serve (the requester falls back / times out,
/// N-of-many). The log is NODE-global on purpose — NOT per-profile, so a flooder can't reset its cap by
/// switching profiles — and self-bounded by the sliding window (+ a lazy GC when distinct origins grow
/// large). `now_ms` injected for testability.
const DHT_PROXY_WINDOW_MS: u64 = 60_000;
const DHT_PROXY_MAX_SERVES_PER_WINDOW: usize = 240; // generous for a few requesters' sweeps/min; a flood exceeds it
static DHT_PROXY_SERVE_LOG: std::sync::OnceLock<Mutex<HashMap<String, Vec<u64>>>> =
    std::sync::OnceLock::new();

/// `true` if this origin may be served now (and records the serve); `false` if it is over its
/// per-window budget. See [`DHT_PROXY_MAX_SERVES_PER_WINDOW`].
pub(crate) fn dht_proxy_admit_origin(origin: &str, now_ms: u64) -> bool {
    let log = DHT_PROXY_SERVE_LOG.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = log.lock().unwrap_or_else(|p| p.into_inner());
    let window_start = now_ms.saturating_sub(DHT_PROXY_WINDOW_MS);
    // Lazy global GC only when the map grows large (keeps the common path O(1) on one origin's window).
    if map.len() > 4096 {
        map.retain(|_, hits| {
            hits.retain(|&t| t >= window_start);
            !hits.is_empty()
        });
    }
    let hits = map.entry(origin.to_string()).or_default();
    hits.retain(|&t| t >= window_start);
    if hits.len() >= DHT_PROXY_MAX_SERVES_PER_WINDOW {
        return false;
    }
    hits.push(now_ms);
    true
}

/// Serve a DHT-proxy request (R7 Phase 2 mode b) on `db_path`'s local `omni-dht-helper`: run the BEP44
/// op and return `(ok, value)`. Returns `(false, [])` if this profile has no running dht-helper
/// (rendezvous not active / the DHT leg is disabled) — the requester then times out and falls back
/// (N-of-many). Content-blind: we shuttle the requester's sealed `value` + seed, never open them. See
/// docs/decisions/dht-over-peer.md.
pub(crate) fn dht_proxy_serve(
    db_path: &str,
    op: omni_mesh_runtime::MeshDhtProxyOp,
    key: &[u8; 32],
    salt: &[u8],
    value: &[u8],
) -> (bool, Vec<u8>) {
    let endpoint = {
        let Ok(registry) = rendezvous_runtime_registry().lock() else {
            return (false, Vec::new());
        };
        match registry.get(db_path).and_then(|rt| rt.dht_helper.as_ref()) {
            Some(helper) => helper.endpoint().to_string(),
            None => return (false, Vec::new()),
        }
    };
    match op {
        omni_mesh_runtime::MeshDhtProxyOp::Put => (dht_helper_put(&endpoint, key, salt, value), Vec::new()),
        omni_mesh_runtime::MeshDhtProxyOp::Get => match dht_helper_get(&endpoint, key, salt) {
            Ok(Some(value)) => (true, value),
            Ok(None) => (true, Vec::new()), // empty slot is a successful get
            Err(()) => (false, Vec::new()),
        },
        omni_mesh_runtime::MeshDhtProxyOp::Peers => {
            let info_hash: [u8; 20] = key[..20].try_into().expect("20 of 32");
            match dht_helper_peers(&endpoint, &info_hash) {
                Some(peers) => {
                    let v4: Vec<std::net::SocketAddrV4> = peers
                        .into_iter()
                        .filter_map(|peer| match peer {
                            std::net::SocketAddr::V4(v4) => Some(v4),
                            std::net::SocketAddr::V6(_) => None,
                        })
                        .collect();
                    (true, omni_mesh_runtime::pack_ipv4_peers(&v4))
                }
                None => (false, Vec::new()),
            }
        }
    }
}

/// Lowercase-hex encode (DHT-helper IPC). Local mirror of the helper's own encoder.
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for [hi, lo] in bytes.as_chunks::<2>().0 {
        let hi = (*hi as char).to_digit(16)?;
        let lo = (*lo as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// The app's view of the rendezvous courier: publish/fetch opaque blobs under opaque
/// topics. `RendezvousHelper` is the production impl (the separated process, real
/// providers); a test impl is in-memory — which makes the publish→fetch→open beacon
/// flow locally testable without the network. The core always does the seal/topic.
/// `Sync` because topics, and my publish against the peer's fetch, run in parallel threads.
pub trait RendezvousCourier: Sync {
    fn publish(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
    ) -> OmniResult<()>;
    fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>>;

    /// Publish one of the planned publications (a beacon) along `route` — the steps of its plan due
    /// now — with its slim copy (the same beacon without the relay bundle) for a courier the full one
    /// does not fit. Default: the full one, as [`publish`] — a test double has no plans.
    #[allow(clippy::too_many_arguments)]
    fn publish_routed(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        slim_hex: Option<&str>,
        ttl_secs: u32,
        route: &PublishRoute,
    ) -> OmniResult<()> {
        let _ = slim_hex;
        self.publish(topic_hex, dhtkey_hex, blob_hex, ttl_secs, route.priority)
    }

    /// The steps of `publication`'s plan due now for `key`: all when `force` (the content or the topic
    /// window changed), else the blocks whose frequency came up. Empty — nothing to post. Default (a
    /// test double without plans): go when forced, else nothing — post on a change only.
    fn due_steps(&self, publication: crate::Publication, key: &str, force: bool) -> Vec<Vec<String>> {
        let _ = (publication, key);
        if force { vec![Vec::new()] } else { Vec::new() }
    }

    /// Publish to couriers of one kind only (`nostr`, `ntfy`, `dht`), replacing this writer's earlier
    /// value where the courier can — for an entry whose topic is that kind's own (the relay directory
    /// keeps a different number of shelves per kind). Default: as [`publish`] — a test double has no kinds.
    fn publish_on_kind(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, ttl_secs: u32, kind: &str) -> OmniResult<()> {
        let _ = kind;
        self.publish(topic_hex, dhtkey_hex, blob_hex, ttl_secs, PublishPriority::Background)
    }

    /// Like [`fetch`], plus whether any provider actually answered. Defaults to "yes" so a
    /// test double only has to implement `fetch`; the real helper overrides it.
    fn fetch_with_reach(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        legs: CourierLegs,
    ) -> OmniResult<(Vec<String>, bool)> {
        self.fetch(topic_hex, dhtkey_hex, legs).map(|blobs| (blobs, true))
    }

    /// [`fetch_with_reach`](Self::fetch_with_reach) under several topics, every blob together (no DHT
    /// key — a set read as one). Default: one topic after another.
    fn fetch_many_with_reach(&self, topic_hexes: &[String], legs: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
        let mut blobs = Vec::new();
        let mut reached = false;
        for topic_hex in topic_hexes {
            let (got, answered) = self.fetch_with_reach(topic_hex, "-", legs)?;
            blobs.extend(got);
            reached |= answered;
        }
        Ok((blobs, reached))
    }

    /// When this courier last asked the DHT under each pair topic (see [`dht_read_due`]). Default:
    /// nowhere — a test double asks every time it is asked.
    fn dht_read_log(&self) -> Option<&DhtReadLog> {
        None
    }

    /// Push model — blobs published under `topic_hexes` within `wait` (see `RendezvousHelper::watch`).
    /// Default: nothing ever arrives (a test double without subscriptions).
    fn watch(&self, topic_hexes: &[String], wait: std::time::Duration) -> OmniResult<Vec<String>> {
        let _ = (topic_hexes, wait);
        Ok(Vec::new())
    }

    /// Where this courier remembers what was already posted (see [`publish_own_beacon_via`]).
    /// Default: nowhere — a test double posts every time, as asked.
    fn beacon_publish_log(&self) -> Option<&BeaconPublishLog> {
        None
    }
}

impl RendezvousCourier for RendezvousHelper {
    fn publish(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
    ) -> OmniResult<()> {
        let spread = self.spread_for(priority);
        RendezvousHelper::publish_reaching(self, topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority, None, None, &spread, false)
    }
    fn publish_routed(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        slim_hex: Option<&str>,
        ttl_secs: u32,
        route: &PublishRoute,
    ) -> OmniResult<()> {
        self.publish_routed_among(topic_hex, dhtkey_hex, blob_hex, slim_hex, ttl_secs, route, None)
    }
    fn due_steps(&self, publication: crate::Publication, key: &str, force: bool) -> Vec<Vec<String>> {
        self.due_steps_for(publication, key, force)
    }
    fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>> {
        RendezvousHelper::fetch(self, topic_hex, dhtkey_hex, legs)
    }
    fn publish_on_kind(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, ttl_secs: u32, kind: &str) -> OmniResult<()> {
        // `1:<kind>,-<every other>`: that kind and nothing else, not even as a fallback.
        let others: Vec<String> =
            ["nostr", "ntfy", "dht"].iter().filter(|other| **other != kind).map(|other| format!("-{other}")).collect();
        let reply = self.command(&format!(
            "publish {topic_hex} {dhtkey_hex} {blob_hex} {ttl_secs} {} - - 1:{kind},{} replace",
            PublishPriority::Background.as_str(),
            others.join(",")
        ))?;
        let (head, attr) = split_courier_attribution(&reply);
        if let Some(attr) = attr {
            omni_app_trace_log("rendezvous/publish", &format!("topic={} couriers: {attr}", topic_prefix(topic_hex)));
        }
        publish_reply_accepted(head).map_err(|reason| OmniError::Transport(format!("rendezvous publish: {reason}")))
    }
    fn fetch_with_reach(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        legs: CourierLegs,
    ) -> OmniResult<(Vec<String>, bool)> {
        RendezvousHelper::fetch_with_reach(self, topic_hex, dhtkey_hex, legs, None)
    }
    fn fetch_many_with_reach(&self, topic_hexes: &[String], legs: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
        RendezvousHelper::fetch_many_with_reach(self, topic_hexes, legs)
    }
    fn dht_read_log(&self) -> Option<&DhtReadLog> {
        Some(&self.dht_reads)
    }
    fn watch(&self, topic_hexes: &[String], wait: std::time::Duration) -> OmniResult<Vec<String>> {
        RendezvousHelper::watch(self, topic_hexes, wait)
    }
    fn beacon_publish_log(&self) -> Option<&BeaconPublishLog> {
        Some(&self.beacon_publishes)
    }
}

/// One owned, long-lived rendezvous runtime per active profile (keyed by db_path): the
/// app-owned helper process plus the cross-call `ReplayGuard`. The helper is spawned once
/// on first use and reused for every later dial — a fresh process per dial would re-pay the
/// spawn + STUN warmup — and is killed when the profile's runtime context stops
/// (`stop_runtime_context` → `clear_rendezvous_runtime_for_profile`). Both are held behind
/// `Arc`s so a caller clones the handle out under a brief registry lock and then runs the
/// SLOW network I/O (STUN consensus, provider fetch, dial) WITHOUT holding the global lock.
#[derive(Clone)]
struct RendezvousRuntime {
    helper: std::sync::Arc<RendezvousHelper>,
    /// The owned `omni-dht-helper` process (R7 Phase 2). Held only for its lifecycle: the rendezvous
    /// child talks to it directly (via `OMNI_DHT_HELPER_ENDPOINT`), so the core never reads this field
    /// — keeping the `Arc` alive keeps the DHT engine alive, and dropping the runtime kills it. `None`
    /// in unit tests that don't exercise the DHT leg.
    #[allow(dead_code)]
    dht_helper: Option<std::sync::Arc<DhtHelper>>,
}

static RENDEZVOUS_RUNTIME_REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, RendezvousRuntime>>> =
    std::sync::OnceLock::new();

fn rendezvous_runtime_registry() -> &'static Mutex<HashMap<String, RendezvousRuntime>> {
    RENDEZVOUS_RUNTIME_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get-or-spawn the owned rendezvous runtime for `db_path`, reusing an already-running
/// helper. `spawn` (which starts the windowless helper children — DHT helper + rendezvous
/// helper) runs only on the first call for this profile, and only under the registry lock for
/// the one-time startup-line reads — it performs NO network I/O, so the brief lock never covers
/// a slow operation.
fn ensure_rendezvous_runtime_with(
    db_path: &str,
    spawn: impl FnOnce() -> OmniResult<RendezvousRuntime>,
) -> OmniResult<RendezvousRuntime> {
    let mut registry = rendezvous_runtime_registry()
        .lock()
        .map_err(|_| OmniError::Internal("rendezvous runtime registry poisoned".to_string()))?;
    if let Some(existing) = registry.get(db_path) {
        return Ok(existing.clone());
    }
    let runtime = spawn()?;
    registry.insert(db_path.to_string(), runtime.clone());
    Ok(runtime)
}

/// The profile's rendezvous runtime, started with the profile's current courier set if it is not
/// running. The one way to get it: a change to the set restarts the runtime
/// ([`clear_rendezvous_runtime_for_profile`]), and every user takes up the new one on its next call.
pub(crate) fn profile_rendezvous_runtime(db_path: &str) -> OmniResult<RendezvousRuntime> {
    ensure_rendezvous_runtime_with(db_path, || {
        let couriers = resolve_rendezvous_providers_for_db(db_path);
        let dht = couriers_include_dht(&couriers);
        let runtime = spawn_rendezvous_runtime(
            &couriers,
            dht,
            &dht_startup_bootstrap_set(db_path),
            &dht_startup_extra_nodes(db_path),
            load_dht_entry_node_enabled(db_path),
        )?;
        runtime.helper.set_publish_plans(crate::app_publish_plans::load_publish_plans(db_path));
        // The empty-table clock starts with the engine, not with the first look at it (a status poll
        // or a sweep): else a profile whose settings nobody opens found its Mainline dead minutes late.
        if runtime.dht_helper.is_some() {
            clear_dht_empty_since(db_path);
            note_dht_node_count(db_path, 0, current_unix_ms());
        }
        Ok(runtime)
    })
}

/// Spawn the full per-profile rendezvous runtime: the best-effort `omni-dht-helper` (R7 Phase 2) +
/// the rendezvous helper pointed at its endpoint via `OMNI_DHT_HELPER_ENDPOINT`. The DHT helper is
/// spawned FIRST so we have its loopback address to hand the rendezvous child; if it can't start, the
/// rendezvous helper is spawned without it (the BT-DHT leg is skipped and N-of-many covers it — a
/// missing DHT engine must never block a dial). The single place that wires the two helpers together,
/// shared by the app-owned dial path and the mesh autoconnect/bootstrap sweeps.
fn spawn_rendezvous_runtime(
    providers: &[String],
    dht_enabled: bool,
    dht_bootstrap: &[String],
    dht_extra_nodes: &[String],
    dht_entry_node: bool,
) -> OmniResult<RendezvousRuntime> {
    // The BT-DHT leg exposes this node's IP to the global swarm, so it only starts when the
    // profile has opted in (app_dht_rendezvous). Off → skip the helper entirely; the web couriers
    // still carry rendezvous. `dht_bootstrap` (custom nodes for an isolated segment) is threaded
    // to the helper; empty → the global Mainline routers. `dht_entry_node` additionally runs it as a
    // reachable in-segment ENTRY node (server_mode + fixed port + UPnP) — others bootstrap FROM us.
    let dht_helper = if dht_enabled {
        match DhtHelper::spawn(dht_bootstrap, dht_extra_nodes, dht_entry_node) {
            Ok(helper) => Some(std::sync::Arc::new(helper)),
            Err(error) => {
                eprintln!("rendezvous: omni-dht-helper unavailable ({error}) — DHT leg disabled");
                None
            }
        }
    } else {
        None
    };
    let dht_endpoint = dht_helper.as_ref().map(|h| h.endpoint().to_string());
    let helper = RendezvousHelper::spawn_with_providers(providers, dht_endpoint.as_deref())?;
    Ok(RendezvousRuntime {
        helper: std::sync::Arc::new(helper),
        dht_helper,
    })
}

impl OmniApp {
    /// The node's PUBLIC DHT entry endpoint (`ip:port`) when running as a UPnP-mapped entry node — for
    /// the shell to display/share. `None` if entry-node mode is off, the rendezvous runtime isn't up
    /// yet, or UPnP didn't yield a public mapping (no IGD gateway / CGNAT). Ephemeral: the UPnP external
    /// port can change across restarts — re-share on change, like the reach token. See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub fn dht_entry_node_endpoint(&self) -> Option<String> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let registry = rendezvous_runtime_registry().lock().ok()?;
        registry
            .get(&db_path)
            .and_then(|rt| rt.dht_helper.as_ref())
            .and_then(|helper| helper.public_endpoint())
            .map(str::to_string)
    }
}

/// Drop the owned rendezvous helper for `db_path` (killing the child on the last `Arc`
/// ref). Called from `stop_runtime_context` so the helper never outlives the profile.
pub(crate) fn clear_rendezvous_runtime_for_profile(db_path: &str) {
    clear_dht_empty_since(db_path);
    if let Some(registry) = RENDEZVOUS_RUNTIME_REGISTRY.get() {
        if let Ok(mut registry) = registry.lock() {
            registry.remove(db_path);
        }
    }
    // Forget when we last published this profile's identity beacon: a profile coming back up
    // should announce itself immediately, not wait out the republish floor.
    if let Some(registry) = IDENTITY_BEACON_PUBLISHES.get() {
        if let Ok(mut registry) = registry.lock() {
            registry.remove(db_path);
        }
    }
    if let Some(registry) = RENDEZVOUS_SKIPS_TRACED.get() {
        if let Ok(mut registry) = registry.lock() {
            registry.remove(db_path);
        }
    }
}

use omni_rendezvous::{
    open_beacon, rendezvous_topics, rendezvous_topics_with_keys, seal_beacon, Beacon, DhtSlotClaimer,
    NatType, ReplayGuard, SealedBeacon, DHT_SLOT_SALTS,
};

/// Build a self presence beacon (my mesh `node` + reflexive endpoints) for the current
/// topic window. `node` lets the contact tell my beacon from its own on the shared topic.
fn build_self_beacon(
    node: [u8; 20],
    reflexive: Vec<String>,
    nat_type: NatType,
    relay_bundle: Option<String>,
) -> Beacon {
    Beacon {
        node,
        // Forwards (UPnP, global IPv6) — defaults empty; the production publisher
        // (`rendezvous_dial_with`) fills it from `beacon_reflexive`, same as `tcp_direct` below.
        forwarded: Vec::new(),
        reflexive,
        // TCP-direct front doors — defaults empty; the production publisher
        // (`rendezvous_dial_with`) overwrites this with our UPnP-mapped
        // bridge endpoint from `MeshRuntime::direct_tcp_endpoint`. See tcp-direct-peer-fallback.md.
        tcp_direct: Vec::new(),
        nat_type,
        // R1c increment 3b: attach my current signed `om3relay:` bundle (the relays I flagged `shared`)
        // so an approved friend's mesh auto-learns my — possibly rotated — relay set on the next sweep.
        // `None` when I share no relay (a bare presence beacon, as before). The whole beacon is already
        // AEAD-sealed under the pair secret, so only the contact reads it. See r1c-rotatable-white-relay.md.
        relay_bundle,
        revoked_through_ms: None,
        ts_ms: current_unix_ms() as u64,
        ttl_secs: PAIR_BEACON_TTL_SECS,
    }
}

/// A pair beacon is posted once per topic window (see [`publish_own_beacon_via`]), at whatever moment
/// of the window the sweep comes round. It lives a window and an hour: still valid when the next
/// window's beacon goes out on the first sweep after the turn, and gone within about that long
/// after the device stops posting — the stale address a peer may dial meanwhile costs one dial.
const PAIR_BEACON_TTL_SECS: u32 = BEACON_TTL_SECS;

/// A window and an hour (see [`PAIR_BEACON_TTL_SECS`]).
const BEACON_TTL_SECS: u32 = (omni_rendezvous::TOPIC_WINDOW_SECS + 60 * 60) as u32;

/// Seal a beacon for the current blinded topics (current + previous bucket) under a
/// known pair secret. Returns `(topic_id_hex, dhtkey_hex, sealed_wire)` triples to hand the helper —
/// the helper only ever sees opaque blobs under opaque topics + the per-bucket BT-DHT seed (NOT the
/// raw pair secret). Pure (no storage).
fn seal_beacon_under_secret(secret: &[u8; 32], beacon: &Beacon) -> Vec<(String, String, Vec<u8>)> {
    seal_beacon_for_topics(secret, beacon, rendezvous_topics_with_keys(secret, current_unix_ms() as u64))
}

fn seal_beacon_for_topics(
    secret: &[u8; 32],
    beacon: &Beacon,
    topics: Vec<(omni_rendezvous::Topic, [u8; 32])>,
) -> Vec<(String, String, Vec<u8>)> {
    topics
        .iter()
        .map(|(topic, dht_key)| {
            (topic.id(), hex::encode(dht_key), seal_beacon(secret, topic, beacon).to_wire())
        })
        .collect()
}

/// Open the freshest valid beacon from opaque blobs under a known pair secret, skipping
/// any whose `node` matches `exclude_node` (used to drop my OWN echo on the shared
/// topic). Tries the current + previous topic bucket; AEAD/TTL/replay are enforced by
/// `open_beacon`. Pure.
fn open_beacon_under_secret(
    secret: &[u8; 32],
    blobs: &[Vec<u8>],
    exclude_nodes: &[[u8; 20]],
) -> Option<Beacon> {
    let now_ms = current_unix_ms() as u64;
    let topics = rendezvous_topics(secret, now_ms);
    // Anti-replay dedup scoped to THIS poll (fresh guard per call): a polling/caching courier (ntfy
    // `since=all`, BT-DHT, …) re-serves the same still-valid beacon on every sweep, so opening must be
    // idempotent. A guard persisted across polls would reject the re-served beacon as `Replay` → the
    // friend's beacon vanishes after sweep 1 (the R7 convergence bug). See omni_rendezvous::collect_beacons.
    let mut guard = ReplayGuard::new();
    let mut best: Option<Beacon> = None;
    for (i, blob) in blobs.iter().enumerate() {
        let Some(sealed) = SealedBeacon::from_wire(blob) else {
            omni_app_trace_log("rendezvous/open", &format!("blob[{i}] from_wire=FAIL len={}", blob.len()));
            continue;
        };
        let mut opened = false;
        let mut last_err = String::new();
        for topic in &topics {
            match open_beacon(secret, topic, &sealed, now_ms, &mut guard) {
                Ok(beacon) => {
                    opened = true;
                    let excluded = exclude_nodes.contains(&beacon.node);
                    omni_app_trace_log(
                        "rendezvous/open",
                        &format!(
                            "blob[{i}] OPENED node={} excluded={excluded}",
                            beacon.node.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>()
                        ),
                    );
                    // Skip my own beacon (same node) on the shared topic, and any peer the
                    // caller already has.
                    if !excluded {
                        best = match best {
                            Some(prev) if prev.ts_ms >= beacon.ts_ms => Some(prev),
                            _ => Some(beacon),
                        };
                    }
                    break;
                }
                // Each topic is tried; under the wrong one any blob is `Forged`, so that says
                // something only when every topic said it (a copy from a second courier is a
                // `Replay` under its own topic, not a foreign blob).
                Err(e) => {
                    if last_err.is_empty() || !matches!(e, omni_rendezvous::RvError::Forged) {
                        last_err = format!("{e:?}");
                    }
                }
            }
        }
        if !opened {
            omni_app_trace_log("rendezvous/open", &format!("blob[{i}] open=FAIL last_err={last_err} len={}", blob.len()));
        }
    }
    best
}

/// Seal `beacon` for the current blinded topics and publish each via `courier`. Returns
/// how many topics were published to. The core seals (it holds the secret); the courier
/// carries only opaque blobs.
fn publish_beacon_via(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    beacon: &Beacon,
) -> OmniResult<usize> {
    publish_sealed_beacon_via(courier, secret, beacon, seal_beacon_under_secret(secret, beacon))
}

/// What this profile last posted under one pair topic secret, from one node of ours: the topic window
/// and what the beacon said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BeaconPublishRecord {
    bucket: u64,
    fingerprint: u64,
}

/// (pair secret, our node) → what we last posted there, and when. Kept by the profile's rendezvous
/// helper and dropped with it, so a profile coming back up posts at once.
#[derive(Default)]
pub struct BeaconPublishLog(
    Mutex<HashMap<([u8; 32], [u8; 20]), (BeaconPublishRecord, u64)>>,
    Mutex<HashMap<([u8; 32], [u8; 20]), BeaconReach>>,
);

/// How many courier kinds a pair's beacon goes to this topic window, and how many it went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BeaconReach {
    bucket: u64,
    /// Kinds it should reach now (1 at first).
    wanted: u8,
    /// Kinds the last post reached (0 — not posted in this window yet).
    posted: u8,
    /// Since when the pair's device has been seen up without coming, sweep after sweep (or `wanted`
    /// last grew); `0` — not seen.
    since_ms: u64,
    /// When it was last seen so. A longer gap than [`PAIR_REACH_SEEN_GAP_MS`] (it was linked in
    /// between, or off) starts the clock again.
    last_seen_ms: u64,
}

/// Sightings further apart than this are not one stretch of "up but not coming" (a sweep comes every
/// 90–120 s; a linked device is not swept at all).
const PAIR_REACH_SEEN_GAP_MS: u64 = 5 * 60 * 1000;

/// A pair's device whose fresh beacon we keep reading, yet which does not come, may not see where we
/// post (its side blocks that courier, or it turned it off): every this long, our beacon for the pair
/// goes to one more kind of courier. A device that is simply off posts nothing new — we do not widen
/// for it, so a contact away for days costs one kind (the free ones first by default).
const PAIR_REACH_WIDEN_EVERY_MS: u64 = 10 * 60 * 1000;

/// The courier kinds there are: Nostr, Mainline, ntfy.
const COURIER_KIND_COUNT: u8 = 3;

/// A beacon posted this long ago that no courier returns any more was lost (a courier restarted, an
/// item fell out of the DHT): post it again. Shorter, and a fetch racing the post that just went out
/// would read as a loss.
const LOST_BEACON_GRACE_MS: u64 = 60_000;

impl BeaconPublishLog {
    /// How many kinds the pair's beacon should reach now, and how many the last post reached.
    fn reach(&self, secret: &[u8; 32], node: [u8; 20], bucket: u64) -> (u8, u8) {
        let reach = self.1.lock().unwrap_or_else(|poison| poison.into_inner());
        match reach.get(&(*secret, node)) {
            Some(r) if r.bucket == bucket => (r.wanted, r.posted),
            _ => (1, 0),
        }
    }

    fn note_posted(&self, secret: &[u8; 32], node: [u8; 20], bucket: u64, kinds: u8) {
        let mut reach = self.1.lock().unwrap_or_else(|poison| poison.into_inner());
        let entry = reach
            .entry((*secret, node))
            .or_insert(BeaconReach { bucket, wanted: 1, posted: 0, since_ms: 0, last_seen_ms: 0 });
        if entry.bucket != bucket {
            *entry = BeaconReach { bucket, wanted: 1, posted: 0, since_ms: 0, last_seen_ms: 0 };
        }
        entry.posted = entry.posted.max(kinds);
    }

    /// The pair's device has a fresh beacon up, and we are still without a link: after
    /// [`PAIR_REACH_WIDEN_EVERY_MS`] of that, our beacon for the pair goes to one more kind.
    fn note_peer_seen_unlinked(&self, secret: &[u8; 32], node: [u8; 20], now_ms: u64) {
        let bucket = now_ms / 1000 / omni_rendezvous::TOPIC_WINDOW_SECS;
        let mut reach = self.1.lock().unwrap_or_else(|poison| poison.into_inner());
        let entry = reach
            .entry((*secret, node))
            .or_insert(BeaconReach { bucket, wanted: 1, posted: 0, since_ms: 0, last_seen_ms: 0 });
        if entry.bucket != bucket {
            *entry = BeaconReach { bucket, wanted: 1, posted: 0, since_ms: 0, last_seen_ms: 0 };
        }
        let continued = entry.since_ms != 0 && now_ms.saturating_sub(entry.last_seen_ms) <= PAIR_REACH_SEEN_GAP_MS;
        entry.last_seen_ms = now_ms;
        if !continued {
            entry.since_ms = now_ms;
            return;
        }
        if entry.wanted < COURIER_KIND_COUNT && now_ms.saturating_sub(entry.since_ms) >= PAIR_REACH_WIDEN_EVERY_MS {
            entry.wanted += 1;
            entry.since_ms = now_ms;
            omni_app_trace_log(
                "rendezvous/publish",
                &format!("pair device up but not coming: beacon goes to {} courier kinds", entry.wanted),
            );
        }
    }

    /// The fetch saw no beacon of ours under this pair topic: unless we posted a moment ago, forget
    /// that we posted, so the next sweep posts again — the beacon now lives half a day, and a lost one
    /// would otherwise stay lost until the window turns.
    fn note_own_beacon_missing(&self, secret: &[u8; 32], node: [u8; 20], now_ms: u64) {
        let mut log = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if log
            .get(&(*secret, node))
            .is_some_and(|(_, at)| now_ms.saturating_sub(*at) >= LOST_BEACON_GRACE_MS)
        {
            log.remove(&(*secret, node));
            omni_app_trace_log("rendezvous/publish", "own beacon not found at any courier: posting again");
        }
    }
}

/// Everything a beacon tells the contact, without when it was made — two beacons with the same
/// fingerprint send the contact to the same place.
fn beacon_content_fingerprint(beacon: &Beacon) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let unstamped = Beacon { ts_ms: 0, ttl_secs: 0, ..beacon.clone() };
    serde_json::to_string(&unstamped).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

/// Which topics this beacon goes to now: `None` — nowhere, the one posted this window still says the
/// same; otherwise the current window's topic, plus the previous one for a beacon that is new or says
/// something new (a reader whose clock is behind still looks there).
fn beacon_topics_due(last: Option<BeaconPublishRecord>, now: BeaconPublishRecord) -> Option<bool> {
    match last {
        Some(last) if last.fingerprint == now.fingerprint && last.bucket == now.bucket => None,
        Some(last) if last.fingerprint == now.fingerprint => Some(false),
        _ => Some(true),
    }
}

/// Post my own beacon under a pair topic, as often as it is worth it: once per topic window, or at
/// once when what it says changed (a new address after a network switch is exactly what rendezvous
/// is for). A sweep comes every 90–120 s and used to post into the current AND the previous topic each
/// time — some 1700 posts a day for one contact we have no direct link to, the main thing that spent
/// the daily per-IP quota of ntfy.sh. Now it is about 150. The beacon lives two windows (see
/// [`PAIR_BEACON_TTL_SECS`]), so fewer posts leave no gap.
fn publish_own_beacon_via(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    beacon: &Beacon,
) -> OmniResult<usize> {
    let now_ms = current_unix_ms() as u64;
    let Some(log) = courier.beacon_publish_log() else {
        return publish_beacon_via(courier, secret, beacon);
    };
    let key = (*secret, beacon.node);
    let now = BeaconPublishRecord {
        bucket: now_ms / 1000 / omni_rendezvous::TOPIC_WINDOW_SECS,
        fingerprint: beacon_content_fingerprint(beacon),
    };
    let last = log
        .0
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&key)
        .map(|(record, _)| *record);
    let (wanted, posted) = log.reach(secret, beacon.node, now.bucket);
    // The plan's memory of what took this pair's beacon, per kind: the pair's topic, from our node.
    let plan_key = format!("contacts:{}:{}", hex::encode(&secret[..8]), hex::encode(beacon.node));
    let (previous_too, steps) = match beacon_topics_due(last, now) {
        Some(previous_too) => (previous_too, courier.due_steps(crate::Publication::Contacts, &plan_key, true)),
        // Unchanged, but it should reach more couriers than it did: the current window again.
        None if wanted > posted => (false, courier.due_steps(crate::Publication::Contacts, &plan_key, true)),
        // Unchanged: only the blocks whose frequency came up (a block at «раз в 12 ч» never does
        // within the window it was posted in).
        None => {
            let steps = courier.due_steps(crate::Publication::Contacts, &plan_key, false);
            if steps.is_empty() {
                omni_app_trace_log("rendezvous/publish", "skip=unchanged_this_window");
                return Ok(0);
            }
            (false, steps)
        }
    };
    let route = PublishRoute { key: plan_key, steps, reach: wanted, priority: PublishPriority::Contact };
    let mut topics = rendezvous_topics_with_keys(secret, now_ms);
    if !previous_too {
        topics.truncate(1); // the current window comes first
    }
    let published = publish_sealed_beacon_reaching(
        courier,
        secret,
        beacon,
        seal_beacon_for_topics(secret, beacon, topics),
        &route,
    )?;
    // Taken nowhere: the next sweep tries again.
    if published > 0 {
        log.0.lock().unwrap_or_else(|poison| poison.into_inner()).insert(key, (now, now_ms));
        log.note_posted(secret, beacon.node, now.bucket, wanted);
    }
    Ok(published)
}

fn publish_sealed_beacon_via(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    beacon: &Beacon,
    sealed: Vec<(String, String, Vec<u8>)>,
) -> OmniResult<usize> {
    publish_sealed_beacon_reaching(courier, secret, beacon, sealed, &PublishRoute::everywhere(PublishPriority::Contact))
}

/// [`publish_sealed_beacon_via`] along `route`.
fn publish_sealed_beacon_reaching(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    beacon: &Beacon,
    sealed: Vec<(String, String, Vec<u8>)>,
    route: &PublishRoute,
) -> OmniResult<usize> {
    // Topics in parallel: each publish waits for the slowest courier (BT-DHT: 5–10 s).
    let topics = sealed.len();
    let published = std::thread::scope(|scope| {
        let handles: Vec<_> = sealed
            .iter()
            .map(|(topic_id, dhtkey_hex, wire)| {
                let slim = slim_beacon_wire(secret, topic_id, beacon, wire).map(hex::encode);
                scope.spawn(move || {
                    courier
                        .publish_routed(topic_id, dhtkey_hex, &hex::encode(wire), slim.as_deref(), beacon.ttl_secs, route)
                        .is_ok()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .filter(|accepted| *accepted)
            .count()
    });
    // Observability for the unlinked-beacon fallback (R1c): how many blinded topics we sealed for and
    // how many the courier accepted. A `published=0` here = the publish leg failed (broker/helper), not
    // a topic mismatch. See docs/notes/CONTINUE-HERE.md (R1c ntfy convergence).
    omni_app_trace_log(
        "rendezvous/publish",
        &format!("topics={topics} published={published} steps={:?} reach={} relay_bundle={}", route.steps, route.reach, beacon.relay_bundle.is_some()),
    );
    Ok(published)
}

/// The slim copy of a sealed beacon — the same beacon without its relay bundle — when the full one is
/// over what a BT-DHT record holds ([`omni_rendezvous::DHT_MAX_BLOB`]): the DHT then still gets the
/// addresses, and the bundle goes by the web couriers and the live link. `None` when the full one fits.
fn slim_beacon_wire(secret: &[u8; 32], topic_id: &str, beacon: &Beacon, wire: &[u8]) -> Option<Vec<u8>> {
    if wire.len() <= omni_rendezvous::DHT_MAX_BLOB || beacon.relay_bundle.is_none() {
        return None;
    }
    let topic = omni_rendezvous::Topic::from_hex(topic_id)?;
    let slim = Beacon { relay_bundle: None, ..beacon.clone() };
    Some(seal_beacon(secret, &topic, &slim).to_wire())
}

/// Fetch the blobs on the current blinded topics via `courier` and open the freshest
/// valid beacon (None if nothing valid), skipping `exclude_node`'s own echo.
#[cfg(test)]
fn fetch_beacon_via(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    exclude_nodes: &[[u8; 20]],
) -> OmniResult<Option<Beacon>> {
    let (blobs, _) = fetch_beacon_blobs_via(courier, secret, CourierLegs::All)?;
    let opened = open_beacon_under_secret(secret, &blobs, exclude_nodes);
    // Observability (R1c ntfy convergence): `blobs=0` = the peer hasn't published to our topics yet (or
    // the fetch leg failed); `blobs>0 opened=false` = a blob arrived but didn't open (own-echo skipped,
    // wrong secret, replay-guard-dedup, or stale TTL). Distinguishes "peer absent" from "exchange bug".
    omni_app_trace_log(
        "rendezvous/fetch",
        &format!("blobs={} opened={}", blobs.len(), opened.is_some()),
    );
    Ok(opened)
}

/// Whether any of `blobs` is a valid beacon of `node` under this pair secret.
fn beacon_of_node_among(secret: &[u8; 32], blobs: &[Vec<u8>], node: [u8; 20]) -> bool {
    open_beacons_quietly(secret, blobs).iter().any(|beacon| beacon.node == node)
}

/// Every blob the couriers in `legs` hold under the current and previous pair topics, and whether any
/// of them answered at all.
fn fetch_beacon_blobs_via(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    legs: CourierLegs,
) -> OmniResult<(Vec<Vec<u8>>, bool)> {
    // Topics in parallel, like the publish: each fetch waits for the slowest courier.
    let keyed = rendezvous_topics_with_keys(secret, current_unix_ms() as u64);
    let fetched: Vec<OmniResult<(Vec<String>, bool)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = keyed
            .iter()
            .map(|(topic, dht_key)| {
                scope.spawn(move || courier.fetch_with_reach(&topic.id(), &hex::encode(dht_key), legs))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut reached = false;
    for result in fetched {
        let (hex_blobs, answered) = result?;
        reached |= answered;
        for hex_blob in hex_blobs {
            if let Ok(bytes) = hex::decode(&hex_blob) {
                blobs.push(bytes);
            }
        }
    }
    Ok((blobs, reached))
}

/// The profile's helper as one pair sees it: a post goes only where the other device looks, a read only
/// where it posts ([`PairCouriers`]). Everything else is the helper's.
struct PairCourier<'a> {
    helper: &'a RendezvousHelper,
    couriers: &'a PairCouriers,
}

impl RendezvousCourier for PairCourier<'_> {
    fn publish(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        ttl_secs: u32,
        priority: PublishPriority,
    ) -> OmniResult<()> {
        let spread = self.helper.spread_for(priority);
        self.helper.publish_reaching(
            topic_hex,
            dhtkey_hex,
            blob_hex,
            ttl_secs,
            priority,
            self.couriers.publish_to.as_deref(),
            None,
            &spread,
            false,
        )
    }
    fn publish_routed(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        blob_hex: &str,
        slim_hex: Option<&str>,
        ttl_secs: u32,
        route: &PublishRoute,
    ) -> OmniResult<()> {
        self.helper.publish_routed_among(
            topic_hex,
            dhtkey_hex,
            blob_hex,
            slim_hex,
            ttl_secs,
            route,
            self.couriers.publish_to.as_deref(),
        )
    }
    fn due_steps(&self, publication: crate::Publication, key: &str, force: bool) -> Vec<Vec<String>> {
        self.helper.due_steps_for(publication, key, force)
    }
    fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>> {
        self.fetch_with_reach(topic_hex, dhtkey_hex, legs).map(|(blobs, _)| blobs)
    }
    fn fetch_with_reach(
        &self,
        topic_hex: &str,
        dhtkey_hex: &str,
        legs: CourierLegs,
    ) -> OmniResult<(Vec<String>, bool)> {
        self.helper
            .fetch_with_reach(topic_hex, dhtkey_hex, legs, self.couriers.read_from.as_deref())
    }
    fn watch(&self, topic_hexes: &[String], wait: std::time::Duration) -> OmniResult<Vec<String>> {
        self.helper.watch(topic_hexes, wait)
    }
    fn beacon_publish_log(&self) -> Option<&BeaconPublishLog> {
        Some(&self.helper.beacon_publishes)
    }
    fn dht_read_log(&self) -> Option<&DhtReadLog> {
        Some(&self.helper.dht_reads)
    }
}

/// How much a post matters to a metered courier (ntfy.sh, 250 a day per IP): when its allowance runs
/// low, a background post goes through the other couriers only, and what is left is kept for the
/// beacons a contact is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishPriority {
    /// A pair beacon, the account's beacon.
    Contact,
    /// A find-by-ID beacon, a mailbox note.
    Background,
}

impl PublishPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Contact => "contact",
            Self::Background => "background",
        }
    }
}

/// How one planned publication goes out: the steps of its plan due now (`app_publish_plans`), how
/// many of them must take it (a pair whose device does not come gets more), and the memory key the
/// frequencies count from.
#[derive(Debug, Clone)]
pub struct PublishRoute {
    pub key: String,
    /// Empty — every courier (a path without a plan: an invite's first beacon, a test).
    pub steps: Vec<Vec<String>>,
    pub reach: u8,
    pub priority: PublishPriority,
}

impl PublishRoute {
    /// Every courier, no plan.
    pub fn everywhere(priority: PublishPriority) -> Self {
        Self { key: String::new(), steps: Vec::new(), reach: 0, priority }
    }

    fn spread(&self) -> String {
        if self.steps.iter().all(|step| step.is_empty()) {
            "all".to_string()
        } else {
            crate::app_publish_plans::spread_token(&self.steps, self.reach)
        }
    }
}

/// Which couriers a fetch asks. The BT-DHT courier takes seconds per query and has no subscription,
/// so it is asked apart: a round reads the web couriers first and goes to the DHT only for what they
/// did not have (see [`dht_read_due`]) — it used to wait for the DHT on every fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CourierLegs {
    Web,
    Dht,
    All,
    /// Couriers of one web kind only: the relay directory keeps its own shelves on each kind, and
    /// asking the others under them only spends their quota (ntfy answered 429 to 18 such reads).
    Nostr,
    Ntfy,
}

impl CourierLegs {
    fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Dht => "dht",
            Self::All => "all",
            Self::Nostr => "nostr",
            Self::Ntfy => "ntfy",
        }
    }
}

/// While the web couriers answer but do not have a contact's beacon, the DHT is asked for it this
/// often — the case it is for: the contact's web couriers are blocked and it posts only to the DHT.
/// When no web courier answers, the DHT is asked every time.
const DHT_READ_EVERY_MS: u64 = 5 * 60 * 1000;

/// pair secret → when we last asked the DHT under it.
#[derive(Default)]
pub struct DhtReadLog(Mutex<HashMap<[u8; 32], u64>>);

/// Whether to ask the DHT under `secret` now (and if so, note that we did).
fn dht_read_due(courier: &dyn RendezvousCourier, secret: &[u8; 32], web_reached: bool, now_ms: u64) -> bool {
    let Some(log) = courier.dht_read_log() else {
        return true;
    };
    let mut last = log.0.lock().unwrap_or_else(|poison| poison.into_inner());
    let due = !web_reached
        || last
            .get(secret)
            .is_none_or(|at| now_ms.saturating_sub(*at) >= DHT_READ_EVERY_MS);
    if due {
        last.insert(*secret, now_ms);
    }
    due
}


/// Outcome of a rendezvous-driven connect attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RendezvousOutcome {
    /// Dialed the peer at these endpoints (the relay→direct punch upgrade then runs on
    /// the mesh reconcile timer). `relay_bundle` is the peer's signed `om3relay:` bundle, if its
    /// (sealed, per-pair) beacon carried one (R1c inc.3b) — the caller applies it under the
    /// first-party gate (`apply_relay_bundle` with the peer's node as the expected issuer).
    Dialed {
        node: [u8; 20],
        endpoints: Vec<String>,
        relay_bundle: Option<String>,
    },
    /// No (fresh) peer beacon was found on the topic yet — retry later.
    NoPeerBeacon,
    /// The peer's beacon carried no parseable endpoints.
    NoEndpoints,
}

/// How long ONE DHT-over-peer op (a proxied BEP44 get/put round-trip over the mesh) may take. The
/// proxy runs a real Mainline query, so this is seconds, not ms; the fail-fast breaker below means a
/// non-serving proxy costs at most one of these per sweep, not one per op.
const DHT_OVER_PEER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// How many friends a DHT op may go through before the pkarr relays: a friend that does not answer
/// costs one `DHT_OVER_PEER_DEADLINE` per sweep, so three bound the worst case to 15 s.
const DHT_FRIENDS_TO_TRY: usize = 3;

/// The friends a DHT op goes through, in order: our own devices first (they serve us whatever our
/// contacts decide), then the rest as the runtime ranked them (nearest first); a few at most.
fn dht_friends_in_order(mut candidates: Vec<MeshNodeAddress>, own_devices: &[[u8; 20]]) -> Vec<MeshNodeAddress> {
    // Stable: the RTT order holds within each group.
    candidates.sort_by_key(|peer| !own_devices.contains(peer.as_bytes()));
    candidates.truncate(DHT_FRIENDS_TO_TRY);
    candidates
}

/// The core's DHT-over-peer rendezvous leg (R7 Phase 2, mode b / 2e-3): publish + fetch the
/// sealed-beacon BEP44 slots through a proxy peer's DHT over the mesh, keyed by the per-bucket
/// `dht_key`. Runs IN ADDITION to the helper's couriers (N-of-many) so a node whose own DHT UDP is
/// blocked still reaches the BT-DHT rendezvous class via a peer with working UDP. Injected into
/// `rendezvous_connect` so it stays fully testable in-memory. See docs/decisions/dht-over-peer.md.
trait DhtOverPeerLeg {
    /// Publish `sealed` (the AEAD-sealed beacon wire) under this bucket's `dht_key`, claiming a slot.
    fn publish(&self, dht_key: &[u8; 32], sealed: &[u8]);
    /// All values stored in BOTH slots under `dht_key` (the peer's sealed beacon + our own echo).
    fn fetch(&self, dht_key: &[u8; 32]) -> Vec<Vec<u8>>;
}

/// One way to reach a slot of Mainline when our own engine sees no swarm: a friend's engine (mode b)
/// or the pkarr relays over HTTPS. `get` is `None` when it could not ask at all (no route, a timeout,
/// no relay answering) — not `Some(empty)`, an empty slot.
trait DhtSlotWay {
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>>;
    fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]);
    /// For the trace.
    fn name(&self) -> &'static str;
}

/// A friend's engine over the mesh (mode b, docs/decisions/dht-over-peer.md).
struct PeerSlots<'a> {
    runtime: &'a MeshRuntime,
    proxy: MeshNodeAddress,
}

impl DhtSlotWay for PeerSlots<'_> {
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>> {
        self.runtime.dht_get_over_peer(self.proxy, *key, salt.to_vec(), DHT_OVER_PEER_DEADLINE)
    }

    fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]) {
        let _ = self.runtime.dht_put_over_peer(self.proxy, *key, salt.to_vec(), value.to_vec(), DHT_OVER_PEER_DEADLINE);
    }

    fn name(&self) -> &'static str {
        "friend"
    }
}

/// The public pkarr relays, through our own DHT helper (`rget`/`rput`: it derives the slot keys and
/// shapes the record, as for its own engine) — the last way in, with no friend to go through.
/// docs/decisions/dht-over-pkarr-relays.md.
struct RelaySlots {
    endpoint: String,
}

impl DhtSlotWay for RelaySlots {
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>> {
        let reply = dht_helper_request(&self.endpoint, &format!("rget {} {}", to_hex(key), to_hex(salt)))?;
        let mut parts = reply.split_whitespace();
        match parts.next() {
            Some("OK") => Some(parts.next().and_then(from_hex).unwrap_or_default()),
            _ => None,
        }
    }

    fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]) {
        let line = format!("rput {} {} {}", to_hex(key), to_hex(salt), to_hex(value));
        if let Some(reply) = dht_helper_request(&self.endpoint, &line).filter(|reply| !reply.starts_with("OK")) {
            omni_app_trace_log("rendezvous/dht-leg", &format!("relays refused: {reply}"));
        }
    }

    fn name(&self) -> &'static str {
        "pkarr relays"
    }
}

/// `DhtOverPeerLeg` over the ways in order — a friend first, the relays when there is no friend to
/// ask or the one picked does not answer (owner, 2026-10-12: the relays are the last hope). Uses the
/// shared [`DhtSlotClaimer`] so it addresses the SAME two slots as the local `BtDhtChannel` IPC leg.
struct SlotDhtLeg<'a> {
    ways: Vec<Box<dyn DhtSlotWay + 'a>>,
    claimer: DhtSlotClaimer,
    /// How many ways, from the first, could not ask: they are passed over for the rest of this sweep,
    /// so a friend that is not actually a DHT proxy costs one deadline, not one per op. Single-threaded
    /// use (one rendezvous worker), so a `Cell` suffices.
    failed: std::cell::Cell<usize>,
    /// The profile whose Mainline status shows the way the ops went (`None` in tests).
    db_path: Option<String>,
}

impl<'a> SlotDhtLeg<'a> {
    fn new(ways: Vec<Box<dyn DhtSlotWay + 'a>>) -> Self {
        Self { ways, claimer: DhtSlotClaimer::new(), failed: std::cell::Cell::new(0), db_path: None }
    }

    /// Tell this profile's Mainline status which way the ops go («через друга» / «через pkarr-relay»).
    fn for_profile(mut self, db_path: Option<&str>) -> Self {
        self.db_path = db_path.map(str::to_string);
        self
    }

    fn note_way(&self, way: &'static str) {
        if let Some(db_path) = &self.db_path {
            crate::note_dht_way_in(db_path, way, current_unix_ms());
        }
    }

    /// The way that answers now.
    fn live(&self) -> Option<&(dyn DhtSlotWay + 'a)> {
        self.ways.get(self.failed.get()).map(|way| way.as_ref())
    }

    /// One `get` on the first way that answers.
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>> {
        while let Some(way) = self.live() {
            if let Some(value) = way.get(key, salt) {
                self.note_way(way.name());
                return Some(value);
            }
            omni_app_trace_log("rendezvous/dht-leg", &format!("{} did not answer", way.name()));
            self.failed.set(self.failed.get() + 1);
        }
        self.note_way("none");
        None
    }
}

impl DhtOverPeerLeg for SlotDhtLeg<'_> {
    fn publish(&self, dht_key: &[u8; 32], sealed: &[u8]) {
        // Both ways store a pkarr-shaped record and refuse a bigger blob: don't send it.
        if sealed.len() > omni_rendezvous::DHT_MAX_BLOB {
            return;
        }
        // Claim our slot via the shared policy, probing occupancy through the way that answers.
        let Ok(slot) = self.claimer.claim(dht_key, |salt| {
            Ok::<_, ()>(self.get(dht_key, salt).map(|v| !v.is_empty()).unwrap_or(false))
        }) else {
            return;
        };
        // The put goes where the probe got its answer; none answered — don't bother putting.
        if let Some(way) = self.live() {
            way.put(dht_key, DHT_SLOT_SALTS[slot], sealed);
        }
    }

    fn fetch(&self, dht_key: &[u8; 32]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for salt in DHT_SLOT_SALTS {
            if let Some(value) = self.get(dht_key, salt) {
                if !value.is_empty() {
                    out.push(value);
                }
            }
        }
        out
    }
}

/// Fetch + open the freshest valid peer beacon from the DHT-over-peer leg (across the current +
/// previous bucket keys), skipping our own echo by `exclude_node`. Mirrors `fetch_beacon_via` but for
/// the mode-b leg (keyed by `dht_key`, not topic). `None` if the proxy returned nothing valid.
fn fetch_overpeer_beacon(
    leg: &dyn DhtOverPeerLeg,
    secret: &[u8; 32],
    exclude_nodes: &[[u8; 20]],
) -> Option<Beacon> {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for (_topic, dht_key) in rendezvous_topics_with_keys(secret, current_unix_ms() as u64) {
        blobs.extend(leg.fetch(&dht_key));
    }
    open_beacon_under_secret(secret, &blobs, exclude_nodes)
}

/// The fresher of two candidate beacons by `ts_ms` (either may be `None`).
fn freshest_beacon(a: Option<Beacon>, b: Option<Beacon>) -> Option<Beacon> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.ts_ms >= b.ts_ms { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Core rendezvous connect (courier + dial both injected → fully testable in-memory):
/// publish my beacon so the peer can find me, fetch the peer's (skipping my own echo on
/// the shared topic by `node`), and dial it at its reflexive endpoints via `dial`. The
/// `dht_leg` (R7 2e-3) is an optional ADDITIONAL N-of-many leg that publishes/fetches the beacon
/// through a proxy peer's DHT (mode b) — `None` in tests / when no proxy is available. The
/// only thing this can't prove on one machine is the live cross-NAT round-trip.
fn rendezvous_connect(
    courier: &dyn RendezvousCourier,
    // R7 2e-3: optional DHT-over-peer leg (mode b) run alongside the courier's web/local-DHT legs.
    dht_leg: Option<&dyn DhtOverPeerLeg>,
    secret: &[u8; 32],
    my_beacon: &Beacon,
    // Peers on the topic not to dial: on the account's own topic, the devices already linked
    // (several publish there). My own beacon is always passed over.
    skip_nodes: &[[u8; 20]],
    // dial(node, udp_reflexive, tcp_direct): UDP-direct is the preferred carrier; tcp_direct are the
    // peer's TLS-over-TCP front doors to fall back to on a UDP-hostile carrier (same wrapper as the
    // relay fallback). The dialer attempts both; `path_rank` prefers UDP when it comes up.
    dial: impl FnMut(
        [u8; 20],
        Vec<std::net::SocketAddr>,
        Vec<std::net::SocketAddr>,
    ) -> OmniResult<()>,
    // The peer's beacon, as soon as it opens — before and independent of the dial: what it carries
    // for us (a relay bundle, a revocation mark) matters most exactly when the dial cannot succeed.
    consume_peer_beacon: impl FnMut(&Beacon),
    // The master relay→direct upgrade gate (ConnectionPrivacyMode: `RelayOnly` ⇒ false). When false
    // this is the privacy chokepoint — see the beacon-strip + dial-skip below.
    allow_direct: bool,
    // Connectivity Phase C: called with (peer node, peer NAT class, whether the peer advertises an
    // endpoint that is dialable REGARDLESS of its NAT class — a forward or a TCP door) the moment
    // their beacon opens. The caller records it; the connection state uses it to decide whether a
    // direct link is genuinely impossible, which a NAT class alone cannot say.
    on_peer_nat: impl FnMut([u8; 20], NatType, bool),
) -> OmniResult<RendezvousOutcome> {
    // RelayOnly privacy (follow-up d): when the direct upgrade is OFF, advertise NO direct endpoints —
    // the beacon is sealed to the contact, so leaking my reflexive / UPnP / TCP-direct endpoints there
    // would hand my IP to exactly the peer this mode hides it from. Strip them before publishing; the
    // relay path (mesh announce-routed) still reaches me and the signed relay bundle (R1c) still
    // propagates. The dial below is likewise skipped — so ONE flag gates BOTH my advertised endpoints
    // AND my outbound direct-dial (UDP-direct + TCP-direct), the master direct-upgrade toggle. See
    // docs/decisions/connection-privacy-modes.md + tcp-direct-peer-fallback.md (follow-up d).
    let relay_only_beacon;
    let beacon_to_publish = if allow_direct {
        my_beacon
    } else {
        let mut stripped = my_beacon.clone();
        // `forwarded` is a UPnP port-forward and a global IPv6 — the most identifying of the three,
        // since both are stable. Leaving it out of this strip would hand the peer my IP in exactly
        // the mode that exists to withhold it.
        stripped.forwarded.clear();
        stripped.reflexive.clear();
        stripped.tcp_direct.clear();
        relay_only_beacon = stripped;
        &relay_only_beacon
    };
    // My beacon goes out on its own thread while the peer's is fetched and dialed: the dial needs only
    // THEIR beacon, and every courier operation waits for the slowest courier. With BT-DHT on (5–10 s
    // an operation) the old publish-then-fetch order put ~28 s in front of every dial — live
    // 2026-10-03, docs/bugs/reconnect-hangs-on-public-couriers.md. The scope still waits for the
    // publish, so the call returns with my beacon out, as before.
    std::thread::scope(|scope| {
        let publishing = scope.spawn(|| publish_own_beacon_via(courier, secret, beacon_to_publish));
        let outcome = find_and_dial_peer(
            courier,
            dht_leg,
            secret,
            my_beacon,
            beacon_to_publish,
            skip_nodes,
            dial,
            consume_peer_beacon,
            allow_direct,
            on_peer_nat,
        );
        match publishing.join() {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => omni_app_trace_log("rendezvous/publish", &format!("failed: {error}")),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        // The peer's beacon was there — it is up — and we are here because there is no link yet.
        if let (Some(log), Ok(RendezvousOutcome::Dialed { .. } | RendezvousOutcome::NoEndpoints)) =
            (courier.beacon_publish_log(), &outcome)
        {
            log.note_peer_seen_unlinked(secret, my_beacon.node, current_unix_ms() as u64);
        }
        outcome
    })
}

/// The half of [`rendezvous_connect`] that needs only the PEER's beacon: fetch it, record what it
/// says, dial. Runs while my own beacon is being published.
#[allow(clippy::too_many_arguments)]
fn find_and_dial_peer(
    courier: &dyn RendezvousCourier,
    dht_leg: Option<&dyn DhtOverPeerLeg>,
    secret: &[u8; 32],
    my_beacon: &Beacon,
    beacon_to_publish: &Beacon,
    skip_nodes: &[[u8; 20]],
    mut dial: impl FnMut(
        [u8; 20],
        Vec<std::net::SocketAddr>,
        Vec<std::net::SocketAddr>,
    ) -> OmniResult<()>,
    mut consume_peer_beacon: impl FnMut(&Beacon),
    allow_direct: bool,
    mut on_peer_nat: impl FnMut([u8; 20], NatType, bool),
) -> OmniResult<RendezvousOutcome> {
    // R7 2e-3: also publish my beacon to the CURRENT bucket's DHT slot via a proxy peer (mode b), the
    // same way the local BT-DHT leg publishes the current bucket — additive N-of-many.
    if let Some(leg) = dht_leg {
        if let Some((topic, dht_key)) =
            rendezvous_topics_with_keys(secret, current_unix_ms() as u64).into_iter().next()
        {
            let wire = seal_beacon(secret, &topic, beacon_to_publish).to_wire();
            let slim = slim_beacon_wire(secret, &topic.id(), beacon_to_publish, &wire);
            leg.publish(&dht_key, slim.as_deref().unwrap_or(&wire));
        }
    }
    // Fetch the peer's beacon from the courier legs, and (2e-3) merge with whatever the DHT-over-peer
    // leg found — the freshest wins. Either source alone is enough for the dial.
    let exclude: Vec<[u8; 20]> =
        std::iter::once(my_beacon.node).chain(skip_nodes.iter().copied()).collect();
    // The web couriers first; the DHT only for a peer they did not have — and our own engine only while
    // it sees a swarm. A DHT leg over a friend or the pkarr relays exists exactly when it does not
    // (`dht_locally_blocked`): our own Mainline then answers empty after ~4.3 s on every fetch (stand,
    // public Wi-Fi, 2026-10-14), and the leg below reads the same slots the way that works. Its
    // recovery is seen by the node count every sweep, not by these reads.
    let (mut blobs, web_reached) = fetch_beacon_blobs_via(courier, secret, CourierLegs::Web)?;
    let mut helper_peer = open_beacon_under_secret(secret, &blobs, &exclude);
    if helper_peer.is_none()
        && dht_leg.is_none()
        && dht_read_due(courier, secret, web_reached, current_unix_ms() as u64)
    {
        if let Ok((dht_blobs, _)) = fetch_beacon_blobs_via(courier, secret, CourierLegs::Dht) {
            blobs.extend(dht_blobs);
            helper_peer = open_beacon_under_secret(secret, &blobs, &exclude);
        }
    }
    omni_app_trace_log(
        "rendezvous/fetch",
        &format!("blobs={} opened={}", blobs.len(), helper_peer.is_some()),
    );
    if let Some(log) = courier.beacon_publish_log() {
        if !beacon_of_node_among(secret, &blobs, my_beacon.node) {
            log.note_own_beacon_missing(secret, my_beacon.node, current_unix_ms() as u64);
        }
    }
    let peer = match dht_leg {
        Some(leg) => freshest_beacon(helper_peer, fetch_overpeer_beacon(leg, secret, &exclude)),
        None => helper_peer,
    };
    let Some(peer) = peer else {
        return Ok(RendezvousOutcome::NoPeerBeacon);
    };
    // Phase C: record the peer's observed NAT class (before any early return below).
    on_peer_nat(
        peer.node,
        peer.nat_type,
        !peer.forwarded.is_empty() || !peer.tcp_direct.is_empty(),
    );
    // R1c inc.3b: apply a propagated relay bundle straight from the OPENED beacon — BEFORE and
    // INDEPENDENT of the dial. Relay propagation exists precisely for when we CANNOT reach the friend
    // directly (NAT / a UDP-hostile carrier), so an absent endpoint or a failed dial must NOT drop the
    // bundle. The dial below is a best-effort direct-punch on top; it is not a precondition to learning
    // the friend's relay set. See docs/decisions/r1c-rotatable-white-relay.md.
    consume_peer_beacon(&peer);
    // RelayOnly: rendezvous is complete (my stripped beacon published, the peer's relay bundle consumed),
    // but we never direct-dial — the relay backstop carries the data. Honest trace: no endpoints dialed.
    if !allow_direct {
        return Ok(RendezvousOutcome::Dialed {
            node: peer.node,
            endpoints: Vec::new(),
            relay_bundle: peer.relay_bundle,
        });
    }
    // Phase C — skip the wasted direct dial to a STUN address for a symmetric×symmetric pair: that
    // address is a per-destination mapping, valid only towards the STUN server, and punch can't help
    // (documented hole, no port-prediction).
    //
    // The rule covers the STUN address and NOTHING ELSE. A UPnP port-forward and a native global
    // IPv6 (`forwarded`) are inbound-dialable regardless of NAT class — the same argument that has
    // always exempted the TCP door. They used to share one list with the STUN address, and this rule
    // threw them away together: live 2026-09-24, a laptop and a PC that had talked over UDP to the
    // PC's forward three weeks earlier were pushed onto the TCP door the day the laptop's NAT started
    // classifying as symmetric. See docs/bugs/a-forwarded-port-was-dropped-as-symmetric.md.
    let both_symmetric =
        my_beacon.nat_type == NatType::Symmetric && peer.nat_type == NatType::Symmetric;
    // Forwards first: the order is the dialer's preference (`Network::dial` starts the endpoints in
    // this order, overlapping them Happy Eyeballs style), so what is dialable by anyone goes before
    // what is dialable only maybe.
    let endpoints: Vec<std::net::SocketAddr> = peer
        .forwarded
        .iter()
        .chain(peer.reflexive.iter().filter(|_| !both_symmetric))
        .filter_map(|s| s.parse().ok())
        .collect();
    let tcp_endpoints: Vec<std::net::SocketAddr> =
        peer.tcp_direct.iter().filter_map(|s| s.parse().ok()).collect();
    // Why the UDP leg has (or hasn't) anything to dial. A peer advertising a perfectly good UDP
    // endpoint still ends up with an EMPTY list when both sides are symmetric — the suppression
    // below — and without this line that is indistinguishable from "the peer published nothing",
    // which is exactly the wrong turn a live investigation took.
    omni_app_trace_log(
        "rendezvous/dial-plan",
        &format!(
            "peer_nat={:?} my_nat={:?} both_symmetric={both_symmetric} peer_forwarded={} peer_reflexive={} udp_kept={} tcp_direct={}",
            peer.nat_type,
            my_beacon.nat_type,
            peer.forwarded.len(),
            peer.reflexive.len(),
            endpoints.len(),
            tcp_endpoints.len(),
        ),
    );
    if endpoints.is_empty() && tcp_endpoints.is_empty() {
        return Ok(RendezvousOutcome::NoEndpoints);
    }
    dial(peer.node, endpoints.clone(), tcp_endpoints.clone())?;
    Ok(RendezvousOutcome::Dialed {
        node: peer.node,
        // Trace every endpoint we dialed — UDP reflexive + TCP-direct front doors.
        endpoints: endpoints
            .iter()
            .chain(tcp_endpoints.iter())
            .map(|e| e.to_string())
            .collect(),
        relay_bundle: peer.relay_bundle,
    })
}

/// Core rendezvous dial over a concrete helper + mesh runtime, with the pair secret +
/// my mesh node already resolved by the caller. No `&OmniApp` / storage access — so this
/// is exactly what a background thread runs (it holds only `Arc`d handles + the `Copy`
/// secret/node), keeping the SLOW network I/O off the serializer. Gets my reflexive
/// endpoint from the helper's STUN consensus (a beacon with none is still published, so
/// the peer learns my NAT class), then publishes my beacon + dials the peer's.
/// The reflexive endpoints + NAT class to advertise in my beacon: the STUN consensus
/// endpoint (helper), the UPnP-mapped public endpoint **only once AutoNAT has confirmed it is
/// actually reachable** (validate-before-advertise — Phase B), AND our native global-IPv6 endpoint
/// (both from the runtime), if any. The UPnP endpoint and the IPv6 endpoint are the ones a
/// peer behind a DIFFERENT NAT can actually dial — a symmetric NAT's STUN reflexive is a
/// per-destination mapping that isn't inbound-dialable, whereas a *confirmed* UPnP port-forward is
/// stable for any inbound and a global IPv6 has no NAT at all. The peer tries every
/// candidate. Best-effort: any one failing still yields the others. See
/// docs/decisions/connectivity-upgrade-via-peer-relay.md + serverless-bootstrap-rendezvous.md.
/// Which UPnP-mapped WAN endpoint (if any) to advertise in our beacon, given the current mapping, the
/// AutoNAT verdict, our proven egress IP (STUN reflexive), and whether direct is allowed (privacy mode).
/// Pure decision so it is unit-testable:
///  • `confirmed == Some(mapped)` → advertise (validate-before-advertise, Phase B: a verifier actually
///    dialed THIS endpoint back, so it's reachable regardless of our egress).
///  • `confirmed == Some(other)` → withhold (stale/phantom mapping — a router reporting a port it doesn't
///    forward must not become a dial the peer wastes attempts on).
///  • `confirmed == None` (no verdict yet — no verifier has dialed back) → advertise OPTIMISTICALLY iff
///    direct is allowed AND the UPnP endpoint is plausibly on OUR egress path: the relay-less
///    first-contact exception (Option 1; no link ⇒ no verifier ⇒ UPnP withheld forever ⇒ no link). The
///    UPnP endpoint is the LOCAL ROUTER's WAN ip:port; if our proven egress (`reflexive_ip`, what STUN
///    saw) has a DIFFERENT IP, our traffic leaves via something else — a full/included system tunnel
///    (a VPN) or double-NAT/CGNAT — so the UPnP endpoint is a PHANTOM for our path (a peer
///    dialing it reaches the router while our replies egress elsewhere) → withhold; reflexive + relay
///    cover us. Matching IP (router IS our egress) or unknown reflexive (fail-open) lets it through. This
///    keeps Omni tunnel-agnostic with no per-OS tunnel probe — see docs/decisions/tunnel-agnostic-connectivity.md.
///  • RelayOnly (`allows_direct == false`) never advertises a direct endpoint.
/// See docs/bugs/relay-less-first-contact-validate-before-advertise.md + connectivity-upgrade-via-peer-relay.md.
fn upnp_beacon_candidate(
    mapped: Option<std::net::SocketAddr>,
    confirmed: Option<std::net::SocketAddr>,
    reflexive_ip: Option<std::net::IpAddr>,
    allows_direct: bool,
) -> Option<std::net::SocketAddr> {
    let upnp = mapped?;
    match confirmed {
        Some(c) if c == upnp => Some(upnp),
        Some(_) => None,
        None => {
            if !allows_direct {
                return None;
            }
            // Optimistic advertise only when the UPnP WAN IP is on our proven egress path (or egress
            // unknown → fail-open). A mismatch = tunnel/double-NAT → phantom → withhold.
            match reflexive_ip {
                Some(rip) if rip != upnp.ip() => None,
                _ => Some(upnp),
            }
        }
    }
}

/// This node's UDP endpoints for the beacon, split by whether our NAT class matters for them.
///
/// The split is the point. The dialer's symmetric×symmetric rule is correct for a STUN address and
/// wrong for a forward, and the dialer cannot tell the two apart by looking at `host:port` — only we
/// know which is which, so we say so. See docs/bugs/a-forwarded-port-was-dropped-as-symmetric.md.
struct BeaconUdpEndpoints {
    /// Dialable by anyone regardless of our NAT class: a UPnP port-forward, a native global IPv6.
    forwarded: Vec<String>,
    /// The STUN-observed address — on a symmetric NAT, valid only towards the STUN server.
    reflexive: Vec<String>,
    nat_type: NatType,
    /// Our proven egress IP (what STUN saw), for the TCP front door's phantom-mapping guard.
    reflexive_ip: Option<std::net::IpAddr>,
}

impl BeaconUdpEndpoints {
    /// One ordered list, forwards first — for the identity beacon, which has no NAT-class rule on
    /// the dialing side and therefore no reason to carry the split.
    fn combined(&self) -> Vec<String> {
        self.forwarded.iter().chain(self.reflexive.iter()).cloned().collect()
    }
}

fn beacon_reflexive(runtime: &MeshRuntime) -> BeaconUdpEndpoints {
    let mut forwarded = Vec::new();
    let mut nat_type = NatType::Unknown;
    // Our proven egress IP (what STUN saw) — used to reject a UPnP endpoint that isn't on our path
    // (tunnel / double-NAT). See `upnp_beacon_candidate`. Returned so the TCP front-door gate
    // (`tcp_beacon_candidate`) applies the SAME phantom-mapping guard.
    let mut reflexive_ip: Option<std::net::IpAddr> = None;
    // STUN first for the IP + NAT class, but its ADDRESS is appended last (see below).
    let mut stun_endpoint: Option<String> = None;
    // Asked from the mesh's OWN socket, so the address is the one a peer's dial reaches. The helper
    // used to ask from a throwaway socket and we published the port of a socket already closed
    // (docs/bugs/every-nat-was-classified-symmetric.md). RelayOnly publishes none of our endpoints,
    // so it does not ask at all — the query would show our mesh socket to the STUN operators for
    // nothing. Traced whatever it says: a failed STUN used to vanish here without a word.
    if runtime.allows_direct_punch() {
        let found = runtime.stun_reflexive();
        omni_app_trace_log(
            "rendezvous/stun",
            &match found {
                Some(r) => format!(
                    "reply=OK {} {} (mesh socket)",
                    r.addr,
                    if r.endpoint_independent { "cone" } else { "symmetric" }
                ),
                None => "reply=none: fewer than two STUN servers agreed".to_string(),
            },
        );
        if let Some(r) = found {
            reflexive_ip = Some(r.addr.ip());
            stun_endpoint = Some(r.addr.to_string());
            nat_type = if r.endpoint_independent {
                NatType::Cone
            } else {
                NatType::Symmetric
            };
        }
    }
    // ORDER = PREFERENCE: forwards are dialable by anyone and come first, the STUN address comes
    // last. `Network::dial` starts the endpoints in this order, `DIAL_ATTEMPT_DELAY` apart, so a dead
    // one now costs that delay. It used to walk them one by one: with STUN first, a find-by-Omni-ID
    // burned 30.1 s timing out on it before reaching the UPnP port that worked. The split into two
    // lists keeps this order — `combined()` and the dial plan both put `forwarded` first.
    if let Some(upnp) = upnp_beacon_candidate(
        runtime.mapped_external_endpoint(),
        runtime.confirmed_reachable_endpoint(),
        reflexive_ip,
        runtime.allows_direct_punch(),
    ) {
        forwarded.push(upnp.to_string());
    }
    if let Some(v6) = runtime.advertised_ipv6_endpoint() {
        let v6 = v6.to_string();
        if !forwarded.contains(&v6) {
            forwarded.push(v6);
        }
    }
    // A STUN address equal to a forward adds nothing: the forward already covers it, and listing it
    // again as "maybe" would let the symmetric rule drop a copy of something dialable.
    let reflexive: Vec<String> =
        stun_endpoint.into_iter().filter(|stun| !forwarded.contains(stun)).collect();
    BeaconUdpEndpoints { forwarded, reflexive, nat_type, reflexive_ip }
}

/// Where a public relay can be reached by anyone (docs/decisions/public-relay-directory.md §3): UDP —
/// forwards, plus the STUN address only on a cone NAT (on a symmetric one it is valid towards the STUN
/// server alone); TCP — the front door as the beacon advertises it.
pub(crate) fn public_relay_endpoints(runtime: &MeshRuntime) -> (Vec<String>, Vec<String>) {
    let udp = beacon_reflexive(runtime);
    let mut dialable = udp.forwarded.clone();
    if matches!(udp.nat_type, NatType::Cone | NatType::Public) {
        dialable.extend(udp.reflexive.iter().cloned());
    }
    (dialable, beacon_tcp_direct(runtime, udp.reflexive_ip))
}

/// The TCP front-door endpoint(s) to advertise in the beacon `tcp_direct` — the TCP analogue of the
/// UPnP UDP path in [`beacon_reflexive`]. Prefer the AutoNAT-CONFIRMED endpoint; if none exists yet
/// (first contact — no verifier has TCP-connected back), fall back to the RAW UPnP-mapped TCP
/// endpoint OPTIMISTICALLY, gated by [`tcp_beacon_candidate`] on the egress-IP match so a phantom
/// router mapping is still withheld. This is what lets a peer on a UDP-hostile carrier (mobile
/// that drops UDP) reach us relay-less on the very first contact — the exact gap the live test hit:
/// `tcp_direct=[]` because only the confirmed path was advertised. See
/// docs/decisions/tcp-direct-peer-fallback.md + relay-less-first-contact-validate-before-advertise.md.
fn beacon_tcp_direct(runtime: &MeshRuntime, reflexive_ip: Option<std::net::IpAddr>) -> Vec<String> {
    let mapped_tcp = runtime.mapped_tcp_external_endpoint();
    let confirmed_tcp = runtime.direct_tcp_endpoint();
    let allows_direct = runtime.allows_direct_punch();
    let fixed_port = runtime.configured_direct_tcp_port();
    let base: Vec<String> =
        tcp_beacon_candidate(mapped_tcp, confirmed_tcp, reflexive_ip, allows_direct)
            .into_iter()
            .map(|e| e.to_string())
            .collect();
    let advertised = append_fixed_port_tcp_direct(base, fixed_port, reflexive_ip, allows_direct);
    // Diagnostic for the relay-less TCP front door: this is the exact decision that determines
    // whether a peer whose UDP is dead (mobile carrier drop) can still reach us over TCP. If
    // `advertised` is empty the peer sees "no verified announce" and TCP-direct dial fails — the
    // reason is right here (no UPnP TCP map / RelayOnly / reflexive-IP mismatch). See
    // docs/decisions/connectivity-upgrade-via-peer-relay.md (optimistic TCP advertise).
    omni_app_trace_log(
        "rendezvous/tcp_front_door",
        &format!(
            "mapped_tcp={mapped_tcp:?} confirmed_tcp={confirmed_tcp:?} reflexive_ip={reflexive_ip:?} \
             fixed_port={fixed_port:?} allows_direct={allows_direct} advertised={advertised:?}"
        ),
    );
    advertised
}

/// Manual-forward mode: when the user pinned a FIXED direct-TCP port (and forwarded it on their
/// router), advertise `reflexive_ip:fixed_port` DIRECTLY — no UPnP needed. This is the reliable
/// relay-less door for a UDP-hostile (mobile) peer: the ephemeral UPnP mapping proved unreliable live
/// (`tcp/reconnect-fail` to a phantom-forwarded map). Gated on `allows_direct` (never in RelayOnly)
/// and a known egress IP; appended to (and deduped against) the UPnP-derived candidates. Pure so it
/// is unit-testable. See docs/decisions/tcp-direct-peer-fallback.md.
fn append_fixed_port_tcp_direct(
    mut base: Vec<String>,
    fixed_port: Option<u16>,
    reflexive_ip: Option<std::net::IpAddr>,
    allows_direct: bool,
) -> Vec<String> {
    if !allows_direct {
        return base;
    }
    if let (Some(port), Some(ip)) = (fixed_port.filter(|p| *p != 0), reflexive_ip) {
        let endpoint = std::net::SocketAddr::new(ip, port).to_string();
        if !base.contains(&endpoint) {
            base.push(endpoint);
        }
    }
    base
}

/// Decide the TCP front-door endpoint to advertise, mirroring [`upnp_beacon_candidate`] for TCP:
///  • `!allows_direct` (RelayOnly) → never advertise a direct endpoint.
///  • `confirmed == Some(c)` → advertise the AutoNAT-validated endpoint (always safe).
///  • else → advertise the RAW UPnP-mapped TCP endpoint OPTIMISTICALLY iff it is plausibly on our
///    egress path (`reflexive_ip` matches its IP, or egress unknown → fail-open). A mismatch =
///    tunnel / double-NAT / phantom mapping → withhold, exactly as the UDP gate does.
fn tcp_beacon_candidate(
    mapped_tcp: Option<std::net::SocketAddr>,
    confirmed_tcp: Option<std::net::SocketAddr>,
    reflexive_ip: Option<std::net::IpAddr>,
    allows_direct: bool,
) -> Option<std::net::SocketAddr> {
    if !allows_direct {
        return None;
    }
    if let Some(confirmed) = confirmed_tcp {
        return Some(confirmed);
    }
    let mapped = mapped_tcp?;
    match reflexive_ip {
        Some(rip) if rip != mapped.ip() => None,
        _ => Some(mapped),
    }
}

/// Step B (requester): if we USE the DHT but have NO configured bootstrap nodes yet, ask a few linked
/// peers for the in-segment seeds they know and merge them — so a node converges on working entry points
/// from whatever peer it can reach (LAN neighbour / relay / stranger-by-ID), **not only its contacts**.
/// Gated tight: only when DHT-rendezvous is on AND our list is empty (self-limiting — once we hold a
/// seed the list is non-empty and we stop asking). Runs on the blocking rendezvous worker, so the sync
/// `request_dht_bootstrap` wrapper is fine. See docs/decisions/dht-bootstrap-mesh-propagation.md.
fn maybe_seed_dht_bootstrap_from_peers(runtime: &MeshRuntime, db_path: &str) {
    if !load_dht_rendezvous_enabled(db_path) || !load_dht_bootstrap_nodes(db_path).is_empty() {
        return;
    }
    for peer in runtime.dht_bootstrap_seed_peers() {
        let learned = runtime.request_dht_bootstrap(peer, std::time::Duration::from_secs(4));
        if learned.is_empty() {
            continue;
        }
        let added = merge_learned_dht_bootstrap_nodes(db_path, &learned);
        if added > 0 {
            omni_app_trace_log(
                "rendezvous/dht_boot_seed",
                &format!("learned {added} in-segment DHT bootstrap node(s) from peer {peer}"),
            );
            break;
        }
    }
}

/// Node-cache harvest (segment-reboot survival): while DHT-rendezvous is active, periodically dump the
/// engine's live routing table and persist it as this profile's node cache, so a restart re-bootstraps
/// from peers actually seen online — the segment DHT survives a reboot without the global routers.
/// Throttled by the cache file's mtime (`DHT_NODES_HARVEST_INTERVAL`). Runs on the blocking rendezvous
/// worker (the sync helper request is fine here). See docs/decisions/dht-bootstrap-mesh-propagation.md.
fn maybe_harvest_dht_node_cache(db_path: &str) {
    if !load_dht_rendezvous_enabled(db_path) {
        return;
    }
    // Throttle: skip if the cache was refreshed within the interval.
    if let Ok(modified) =
        std::fs::metadata(dht_nodes_cache_sidecar_path(db_path)).and_then(|meta| meta.modified())
    {
        if modified.elapsed().map(|e| e < DHT_NODES_HARVEST_INTERVAL).unwrap_or(false) {
            return;
        }
    }
    // The DHT helper for this profile owns the routing table; ask it over the loopback line protocol.
    //
    // КАЖДЫЙ путь отказа отсюда трассируется. Раньше молчали все, и «кэша нет» было неотличимо от
    // «не пробовали»: на живом прогоне с включённым DHT (2026-08-13) было 8 дозвонов и 14 успешных
    // публикаций в DHT — и ни строчки о харвесте, потому что логировался только успех.
    let endpoint = {
        let Ok(registry) = rendezvous_runtime_registry().lock() else {
            omni_app_trace_log("rendezvous/dht_node_cache", "skip=registry_locked");
            return;
        };
        match registry.get(db_path).and_then(|rt| rt.dht_helper.as_ref()) {
            Some(helper) => helper.endpoint().to_string(),
            None => {
                omni_app_trace_log("rendezvous/dht_node_cache", "skip=no_dht_helper");
                return;
            }
        }
    };
    let Some(reply) = dht_helper_request(&endpoint, "nodes") else {
        omni_app_trace_log(
            "rendezvous/dht_node_cache",
            &format!("skip=helper_no_reply endpoint={endpoint}"),
        );
        return;
    };
    let Some(list) = reply.strip_prefix("OK") else {
        omni_app_trace_log(
            "rendezvous/dht_node_cache",
            &format!("skip=helper_error reply={}", reply.trim()),
        );
        return;
    };
    let harvested: Vec<String> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    note_dht_node_count(db_path, harvested.len(), crate::current_unix_ms());
    let saved = save_dht_nodes_cache(db_path, &harvested);
    omni_app_trace_log(
        "rendezvous/dht_node_cache",
        &format!("harvested={} saved={saved} (segment-reboot survival)", harvested.len()),
    );
}

fn rendezvous_dial_with(
    helper: &RendezvousHelper,
    couriers: &PairCouriers,
    runtime: &MeshRuntime,
    secret: &[u8; 32],
    my_node: [u8; 20],
    skip_nodes: &[[u8; 20]],
    my_relay_bundle: Option<String>,
    // A revocation of ours the peer's account may not have heard of: its mark rides my beacon, the
    // notice itself goes to the pair's mailbox topic (runtime_revocation_notice.rs).
    revocation_notice: Option<RevocationNotice>,
    consume_db_path: Option<String>,
) -> OmniResult<RendezvousOutcome> {
    // Обслуживание сегмента DHT (обучение сидам + кэш живых узлов) СЮДА НЕ ВХОДИТ — оно
    // периодическое и переехало в `rendezvous_sweep_unlinked_contacts`. Здесь оно было привязано к
    // дозвону и потому не выполнялось в самом нужном случае: нода часами стоит слинкованной, никому
    // не дозванивается, кэш не пишется — и после перезапуска ей нечем вернуться в рой.
    let udp = beacon_reflexive(runtime);
    let nat_type = udp.nat_type;
    let reflexive_ip = udp.reflexive_ip;
    let mut my_beacon = build_self_beacon(my_node, udp.reflexive, nat_type, my_relay_bundle);
    my_beacon.forwarded = udp.forwarded;
    my_beacon.revoked_through_ms = revocation_notice.as_ref().map(|notice| notice.mark_ms);
    // Advertise our direct-TCP front door so a peer on a UDP-hostile carrier can dial us over
    // TCP-direct — now including the OPTIMISTIC raw UPnP-TCP map on first contact (see
    // `beacon_tcp_direct`), not only the AutoNAT-confirmed one. See tcp-direct-peer-fallback.md.
    my_beacon.tcp_direct = beacon_tcp_direct(runtime, reflexive_ip);
    // Our hop's carrier decides what the peer may dial us by: TCP — only our door (no UDP address, so
    // the peer does not bring the link up over UDP); QUIC — no door.
    let carrier = runtime.carrier_preference();
    match carrier {
        omni_mesh_runtime::CarrierPreference::Tcp => {
            my_beacon.reflexive.clear();
            my_beacon.forwarded.clear();
        }
        omni_mesh_runtime::CarrierPreference::Quic => my_beacon.tcp_direct.clear(),
        omni_mesh_runtime::CarrierPreference::Auto => {}
    }
    // What the PEER can reach us by regardless of our NAT class — only what we actually publish, so
    // a RelayOnly profile (which strips every endpoint) never claims to be directly reachable.
    let allows_direct = runtime.allows_direct_punch();
    let local_nat_independent =
        allows_direct && (!my_beacon.forwarded.is_empty() || !my_beacon.tcp_direct.is_empty());
    // R7 2e-3: a node whose own DHT UDP is blocked still reaches the BT-DHT rendezvous class — through
    // a friend's engine (mode b) if one on a link serves, else (or when it does not answer) through
    // the pkarr relays over HTTPS (docs/decisions/dht-over-pkarr-relays.md). Only while our own engine
    // sees no swarm (half a minute with an empty table: the network drops DHT traffic) — a node whose
    // Mainline works leans on neither.
    // Ask the engine now (an instant loopback read): the verdict must not lean on the last poll.
    if let Some(db_path) = consume_db_path.as_deref() {
        if let Some(count) = dht_helper_endpoint(db_path).and_then(|endpoint| dht_helper_nodes_count(&endpoint)) {
            note_dht_node_count(db_path, count, current_unix_ms());
        }
    }
    let locally_blocked = consume_db_path.as_deref().is_some_and(dht_locally_blocked);
    let dht_leg = locally_blocked
        .then(|| {
            let mut ways: Vec<Box<dyn DhtSlotWay + '_>> = Vec::new();
            let own = consume_db_path.as_deref().map(own_device_nodes_for).unwrap_or_default();
            for proxy in dht_friends_in_order(runtime.dht_proxy_candidates(), &own) {
                ways.push(Box::new(PeerSlots { runtime, proxy }));
            }
            if let Some(endpoint) = consume_db_path.as_deref().and_then(dht_helper_endpoint) {
                ways.push(Box::new(RelaySlots { endpoint }));
            }
            let names: Vec<&str> = ways.iter().map(|way| way.name()).collect();
            omni_app_trace_log("rendezvous/dht-leg", &format!("own Mainline sees no swarm; ways: {names:?}"));
            SlotDhtLeg::new(ways).for_profile(consume_db_path.as_deref())
        })
        .filter(|leg| !leg.ways.is_empty());
    let courier = PairCourier { helper, couriers };
    if let (Some(db_path), Some(notice)) = (consume_db_path.as_deref(), revocation_notice.as_ref()) {
        publish_revocation_notice_if_due(&courier, db_path, secret, notice, current_unix_ms());
    }
    let consume_db_path_for_mark = consume_db_path.clone();
    let outcome = rendezvous_connect(
        &courier,
        dht_leg.as_ref().map(|l| l as &dyn DhtOverPeerLeg),
        secret,
        &my_beacon,
        skip_nodes,
        |node, udp_endpoints, tcp_endpoints| {
            let peer = MeshNodeAddress::from_bytes(node);
            // S3 honest failover: remember the peer's advertised direct-TCP door (first endpoint) so a
            // later phantom-drop migrates this peer's connection onto it instead of the inert UDP demote.
            // Approved (db_path-bearing) context only; refreshed every sweep so a rotated door overwrites.
            if let (Some(db_path), Some(door)) = (consume_db_path.as_deref(), tcp_endpoints.first()) {
                crate::app_peer_door::set_peer_door(db_path, &node, *door);
            }
            // UDP-direct (preferred carrier) + an additive TCP-direct attempt over the peer's
            // advertised front doors (same TLS-over-TCP wrapper as the relay fallback). path_rank
            // prefers UDP-direct when both come up; TCP-direct carries the link on a UDP-hostile
            // carrier. Either carrier establishing a direct link is success; an error only when BOTH
            // legs fail (the relay backstop still carries traffic regardless).
            // "Start reliable, then move to fast": a peer we reached over its door is retried on
            // UDP every sweep. `dial_at` below short-circuits once a link exists, so without this
            // an door-carried link would never even attempt the faster carrier. Additive and
            // validated — see `Network::upgrade_peer_to_udp`.
            // Our hop's carrier: TCP dials only the door, QUIC only UDP.
            let udp_allowed = carrier != omni_mesh_runtime::CarrierPreference::Tcp;
            let door_allowed = carrier != omni_mesh_runtime::CarrierPreference::Quic;
            if udp_allowed
                && !udp_endpoints.is_empty()
                && runtime.upgrade_peer_to_udp(peer, udp_endpoints.clone())
            {
                return Ok(());
            }
            let udp = if udp_allowed {
                runtime.dial_at(peer, udp_endpoints)
            } else {
                Err(omni_mesh_runtime::MeshRuntimeError::Dial("our hops are TCP".to_string()))
            };
            // `Ok(false)` = the door had nothing to migrate (no live connection yet), which is NOT a
            // connection. Treating it as success is what made a UDP-hostile carrier report `Dialed`
            // forever while nothing was ever dialed. See docs/bugs/door-first-contact.md.
            let tcp = match door_allowed.then(|| runtime.dial_peer_tcp_direct(peer, tcp_endpoints)) {
                Some(Ok(true)) => Ok(()),
                Some(Ok(false)) => Err(omni_mesh_runtime::MeshRuntimeError::Dial(
                    "no live connection to route over the peer's door (door cannot make first contact)"
                        .to_string(),
                )),
                Some(Err(error)) => Err(error),
                None => Err(omni_mesh_runtime::MeshRuntimeError::Dial("our hops are QUIC".to_string())),
            };
            match (udp, tcp) {
                (Ok(_), _) | (_, Ok(_)) => Ok(()),
                (Err(error), Err(_)) => {
                    Err(OmniError::Transport(format!("rendezvous dial: {error}")))
                }
            }
        },
        |peer| {
            // First-party-gated apply via a fresh Storage (no `&self`/`Storage` here). `None` db_path =
            // a bootstrap (non-approved) context that never consumes — see `spawn_rendezvous_dial`.
            let Some(db_path) = consume_db_path.as_deref() else {
                return;
            };
            if let Some(token) = peer.relay_bundle.as_deref() {
                apply_propagated_relay_bundle_on_worker(db_path, peer.node, token);
            }
            if let Some(mark) = peer.revoked_through_ms {
                read_revocation_notice_if_marked(&courier, db_path, secret, mark, current_unix_ms());
            }
        },
        // Master direct-upgrade gate, read LIVE (follow-up d): a `RelayOnly` profile strips my beacon
        // endpoints + skips the direct-dial inside `rendezvous_connect`, so the data path stays
        // relay-mediated. Re-read each sweep, so a privacy-mode change applies without a mesh restart.
        allows_direct,
        // Phase C: record our own + the peer's NAT class, and whether each side advertises an
        // endpoint dialable regardless of it, so the connection state can be honest about a pair
        // where direct is genuinely impossible. Only in an approved (db_path-bearing) context.
        |peer_node, peer_nat, peer_nat_independent| {
            if let Some(db_path) = consume_db_path.as_deref() {
                crate::app_nat_class::set_local_nat_class(db_path, nat_type, local_nat_independent);
                crate::app_nat_class::set_peer_nat_class(
                    db_path,
                    &peer_node,
                    peer_nat,
                    peer_nat_independent,
                );
            }
        },
    );
    // Read, whatever the beacons said: sending to that account need not wait for them any more.
    if let (Ok(_), Some(db_path)) = (&outcome, consume_db_path_for_mark.as_deref()) {
        note_pair_beacons_read(db_path, secret);
    }
    outcome
}

/// Spawn the windowless background worker that runs ONE rendezvous dial off the hot path:
/// publish my beacon + fetch/dial the peer's, serialized per profile by the owned replay
/// guard. Only `Arc` handles + the `Copy` secret/node cross the thread boundary (no
/// `&self`/storage), so the slow STUN/provider/dial I/O never blocks the caller. The
/// outcome is traced; a successful dial surfaces as the mesh link coming up. Shared by the
/// on-demand `OmniApp::rendezvous_reach_contact_if_no_link` and the autoconnect sweep.
///
/// R1c inc.3b consume: when `consume_db_path` is `Some` (a PER-PAIR / approved-contact context — never
/// bootstrap), a peer beacon carrying a `relay_bundle` is applied via a fresh `Storage::open` on this
/// worker (the worker holds no `&self`/`Storage`, and `Storage` isn't `Sync`; SQLite WAL supports the
/// short-lived second connection). The first-party gate (`apply_relay_bundle` with the peer's node as
/// the expected issuer) ensures a contact propagates only its OWN set. See r1c-rotatable-white-relay.md.
/// Identity-beacon validity (Model A): a window and an hour, like a pair beacon.
const IDENTITY_BEACON_TTL_SECS: u32 = BEACON_TTL_SECS;

/// How often the identity beacon is actually re-posted to the couriers.
///
/// It used to go out on EVERY sweep — about once every 100 s for a beacon valid for 900 s,
/// i.e. nine times more often than needed, times two topics times every courier. The free
/// public brokers meter publishing, and two peers behind one home IP were between them
/// spending the whole ntfy.sh budget on re-posting something that had not changed: on the
/// laptop ntfy publish NEVER succeeded (25 fail, 81 skip, 0 ok) while fetch was fine and
/// nostr was at 100%. Then a third of a 15-min TTL — still 576 posts a day into two topics, more
/// than twice the daily quota of ntfy.sh on its own. Now once per topic window: the beacon lives a
/// window and an hour, so the one posted in the previous window is still found until this one is out.
///
/// The floor on TIME is now the frequency of each block of the Omni-ID plan (`app_publish_plans`,
/// «Авто» — once a window); a beacon whose endpoints changed republishes at once — a stale endpoint
/// after a network switch is exactly what rendezvous exists to avoid.
///
/// db_path → (when we last published, fingerprint of what we published).
static IDENTITY_BEACON_PUBLISHES: std::sync::OnceLock<Mutex<HashMap<String, (i64, u64)>>> =
    std::sync::OnceLock::new();

/// The devices the last pass of `rendezvous_peers` skipped, per profile, and why. The list is rebuilt
/// every second or two; a reason went to the trace every time (stand 2026-10-13: the same «no session»
/// line every 1.5 s for minutes), now only when the set changes.
static RENDEZVOUS_SKIPS_TRACED: std::sync::OnceLock<Mutex<HashMap<String, Vec<String>>>> = std::sync::OnceLock::new();

pub(crate) fn trace_rendezvous_skips_on_change(db_path: &str, skips: Vec<String>) {
    let registry = RENDEZVOUS_SKIPS_TRACED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut traced) = registry.lock() else {
        return;
    };
    if traced.get(db_path) == Some(&skips) {
        return;
    }
    for skip in &skips {
        omni_app_trace_log("rendezvous/sweep", skip);
    }
    traced.insert(db_path.to_string(), skips);
}

/// Whether the beacon says something new: never posted, or its fingerprint changed. Unchanged, it
/// goes only where a block's frequency came up.
fn identity_beacon_changed(last: Option<(i64, u64)>, fingerprint: u64) -> bool {
    last.is_none_or(|(_, published_fingerprint)| published_fingerprint != fingerprint)
}

/// What the peer actually needs from the beacon. Deliberately excludes `ts_ms`/`seq` (they
/// change on every build by construction) — otherwise nothing would ever look unchanged.
fn identity_beacon_fingerprint(
    node: &[u8; 20],
    reflexive: &[String],
    tcp_direct: &[String],
    relay_bundle: Option<&str>,
    prekeys: &[u8],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    node.hash(&mut hasher);
    reflexive.hash(&mut hasher);
    tcp_direct.hash(&mut hasher);
    relay_bundle.hash(&mut hasher);
    prekeys.hash(&mut hasher);
    hasher.finish()
}

/// Model A (MA2): publish my SIGNED identity beacon under the Omni-ID-keyed topic, so a
/// stranger holding my Omni-ID can find me (fetch my prekey bundle + endpoints, verify
/// against the ID, then X3DH + request). Called from the sweep only when Discoverability
/// is Public. Best-effort — a courier being down just means retry next sweep.
/// См. docs/decisions/global-identity-discovery.md.
pub(crate) fn publish_identity_beacon(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    my_relay_bundle: Option<String>,
) {
    let _ = post_identity_beacon(storage, runtime, db_path, my_relay_bundle, false);
}

/// "Findable by ID" was turned off: post a WITHDRAWAL — a signed beacon of this device with no
/// prekey bundle and no endpoints, newer than the last real one. Finders keep the freshest beacon
/// of each device (`lookup_identity_beacon`), so the withdrawal hides the device at once, instead of
/// the last real beacon staying findable for its whole TTL (a window and an hour — up to 13 h after
/// the switch was turned off; owner's suspicion, 2026-10-11). Couriers cannot be asked to delete,
/// but they keep the newest. Returns whether at least one courier took it.
pub(crate) fn publish_identity_beacon_withdrawal(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
) -> bool {
    let published = post_identity_beacon(storage, runtime, db_path, None, true);
    // Whatever was published last is gone: turning it back on must post at once.
    IDENTITY_BEACON_PUBLISHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(db_path);
    published
}

/// Forget that the identity beacon is out, so the next sweep posts it (findable turned on again).
pub(crate) fn forget_identity_beacon_publish(db_path: &str) {
    IDENTITY_BEACON_PUBLISHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(db_path);
}

/// This device's signed identity beacon as of now — what a finder needs to reach it: certificate
/// chain, prekey bundle, endpoints. Also what a passport holder posts as its mark
/// (runtime_account_passport.rs). `None` for a device without a certificate yet.
pub(crate) fn build_signed_identity_beacon(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    my_relay_bundle: Option<String>,
) -> Option<omni_rendezvous::IdentityBeacon> {
    build_identity_beacon_body(storage, runtime, my_relay_bundle, false).map(|(beacon, _)| beacon)
}

/// The beacon and the fingerprint of what it says (see [`identity_beacon_fingerprint`]).
fn build_identity_beacon_body(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    my_relay_bundle: Option<String>,
    withdrawn: bool,
) -> Option<(omni_rendezvous::IdentityBeacon, u64)> {
    let account = storage.get_local_account().ok()??;
    let identity = storage.get_local_identity_key(&account.descriptor.account_id).ok()??;
    let now = current_unix_ms();
    let prekey_bundle = storage.build_local_prekey_bundle(now).ok()?;
    let device_label = storage
        .get_local_device()
        .ok()
        .flatten()
        .map(|device| device.device_label)
        .unwrap_or_default();
    // What the finder needs beyond the chain (identity_beacon_prekey_bundle puts the rest back).
    let prekey_compact = prekey_bundle.to_compact();
    // The prekeys themselves, not when the bundle was assembled (that is "now" on every build, and
    // a fingerprint holding it never repeated — the unchanged beacon went out on every sweep).
    let prekeys: Vec<u8> = {
        let mut keys = prekey_bundle.signed_prekey.public_key.0.to_vec();
        keys.extend_from_slice(&prekey_bundle.signed_prekey.prekey_id.to_le_bytes());
        if let Some(one_time) = &prekey_bundle.one_time_prekey {
            keys.extend_from_slice(&one_time.public_key.0);
        }
        keys
    };
    let (prekey_compact, reflexive, tcp_direct) = if withdrawn {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        let udp = beacon_reflexive(runtime);
        let reflexive = udp.combined();
        // Optimistic first-contact TCP front door (relay-less path for a UDP-hostile finder), not
        // only the AutoNAT-confirmed endpoint — the live gap where the identity beacon shipped
        // `tcp_direct=[]`.
        let tcp_direct: Vec<String> = beacon_tcp_direct(runtime, udp.reflexive_ip);
        (prekey_compact, reflexive, tcp_direct)
    };
    let node = *runtime.node_address().as_bytes();
    let fingerprint = identity_beacon_fingerprint(
        &node,
        &reflexive,
        &tcp_direct,
        my_relay_bundle.as_deref(),
        if withdrawn { &[] } else { &prekeys },
    );
    // Only a certified device can be found by the account's Omni-ID.
    let Some(chain) = prekey_bundle.device_certificate_chain.as_ref() else {
        omni_app_trace_log("identity/publish", "skip=no_device_certificate");
        return None;
    };
    let omni_id = omni_crypto::omni_id_bytes(&chain.working.master_public_key);
    let mut beacon = omni_rendezvous::IdentityBeacon {
        omni_id,
        chain_compact: chain.to_compact(),
        node,
        prekey_compact,
        device_label,
        reflexive,
        tcp_direct,
        relay_bundle: my_relay_bundle,
        ts_ms: now as u64,
        ttl_secs: IDENTITY_BEACON_TTL_SECS,
        seq: now as u64, // wall-clock is a monotone-enough freshest-wins seq (no persist needed)
        signature: Vec::new(),
    };
    let signing_bytes = omni_rendezvous::identity_beacon_signing_bytes(&beacon);
    let sig = omni_crypto::sign_message(&identity.protected_signing_private_key, &signing_bytes).ok()?;
    beacon.signature = sig.bytes;
    Some((beacon, fingerprint))
}

/// The one body of a real identity beacon and of its withdrawal (`withdrawn`: no bundle, no
/// endpoints, never deduplicated). Returns whether a courier took it.
fn post_identity_beacon(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    my_relay_bundle: Option<String>,
    withdrawn: bool,
) -> bool {
    let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
        return false;
    };
    let helper = std::sync::Arc::clone(&rendezvous.helper);
    let now = current_unix_ms();
    let Some((beacon, fingerprint)) = build_identity_beacon_body(storage, runtime, my_relay_bundle, withdrawn) else {
        return false;
    };
    // Don't spend the couriers' publish budget re-posting an unchanged, still-valid beacon: unchanged,
    // only the plan's blocks whose frequency came up.
    let steps = if withdrawn {
        helper.due_steps_for(crate::Publication::OmniId, "omni_id", true)
    } else {
        let registry = IDENTITY_BEACON_PUBLISHES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = registry.lock().unwrap_or_else(|poison| poison.into_inner());
        let changed = identity_beacon_changed(map.get(db_path).copied(), fingerprint);
        let steps = helper.due_steps_for(crate::Publication::OmniId, "omni_id", changed);
        if steps.is_empty() {
            return false;
        }
        map.insert(db_path.to_string(), (now, fingerprint));
        steps
    };
    let route = PublishRoute {
        key: "omni_id".to_string(),
        steps,
        reach: 1,
        priority: PublishPriority::Background,
    };
    let omni_id = beacon.omni_id;
    let wire_hex = hex::encode(beacon.to_wire());
    let mut published = 0;
    for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(&omni_id, now as u64) {
        // The identity-topic DHT key is public (derived from the Omni-ID) — it grants no
        // secrecy, but the helper wire protocol requires a non-empty key token, so we pass
        // the real per-bucket seed (ntfy/Nostr ignore it; the BT-DHT leg uses it).
        if helper.publish_routed_among(&topic.id(), &hex::encode(dht_key), &wire_hex, None, IDENTITY_BEACON_TTL_SECS, &route, None).is_ok() {
            published += 1;
        }
    }
    omni_app_trace_log(
        "identity/publish",
        &format!("omni_id={} withdrawn={withdrawn} published={published}", hex::encode(omni_id)),
    );
    published > 0
}

/// The device certificate chain a beacon carries (not yet verified — see `verify_identity_beacon`).
pub(crate) fn identity_beacon_device_chain(
    beacon: &omni_rendezvous::IdentityBeacon,
) -> OmniResult<omni_crypto::DeviceCertificateChainV1> {
    omni_crypto::DeviceCertificateChainV1::from_compact(&beacon.chain_compact)
        .ok_or_else(|| OmniError::InvalidInput("identity beacon device chain is malformed".to_string()))
}

/// The prekey bundle a beacon carries, whole: its compact prekeys on top of the beacon's chain, in the
/// wire form the rest of the app takes (not yet validated — see `validate_prekey_bundle_v1`).
pub(crate) fn identity_beacon_prekey_bundle(
    beacon: &omni_rendezvous::IdentityBeacon,
) -> OmniResult<omni_protocol::PrekeyBundleV1> {
    let chain = identity_beacon_device_chain(beacon)?;
    let bundle = omni_crypto::PrekeyBundle::from_compact(&beacon.prekey_compact, &chain)
        .ok_or_else(|| OmniError::InvalidInput("identity beacon prekey bundle is malformed".to_string()))?;
    Ok(prekey_bundle_v1_from_raw(&bundle, beacon.device_label.clone()))
}

/// Verify a fetched identity beacon against the Omni-ID a looker-up used: the chain's master key
/// must COMMIT to the ID, the chain must hold (master → working key → device), the node address must
/// be the certified device's, and the device key must have signed the canonical bytes. A malicious
/// courier can drop the beacon, never forge one for an ID it doesn't own.
/// See docs/decisions/global-identity-discovery.md §2, multi-device-equal-nodes.md.
pub(crate) fn verify_identity_beacon(
    omni_id: &[u8; 20],
    beacon: &omni_rendezvous::IdentityBeacon,
) -> OmniResult<()> {
    let chain = identity_beacon_device_chain(beacon)?;
    omni_crypto::verify_device_certificate_chain(&chain)?;
    if !omni_crypto::verify_omni_id_commitment(omni_id, &chain.working.master_public_key) {
        return Err(OmniError::InvalidInput(
            "identity beacon account does not match the Omni-ID".to_string(),
        ));
    }
    let device_signing_key = chain.device.identity_signing_public_key;
    let node = node_address_from_ed25519_pubkey(device_signing_key.0)
        .map_err(|error| OmniError::InvalidInput(format!("identity beacon device key: {error}")))?;
    if node.as_bytes() != &beacon.node {
        return Err(OmniError::InvalidInput(
            "identity beacon node is not the certified device".to_string(),
        ));
    }
    omni_crypto::verify_signature(
        &device_signing_key,
        &omni_rendezvous::identity_beacon_signing_bytes(beacon),
        &omni_crypto::Signature {
            bytes: beacon.signature.clone(),
        },
    )
}

/// Fetch + verify the freshest identity beacon for `omni_id` from the couriers
/// (Model A find-by-ID). Returns `None` if nothing valid is published (the peer
/// isn't findable-by-ID or is offline). Only commitment+signature-valid, fresh
/// beacons are considered; among those, the highest `seq` wins.
/// Whether any courier in a helper attribution string actually answered.
///
/// The helper renders `label=ok+Nms` / `=empty+Nms` / `=fail+Nms` / `=skip`. `ok` and
/// `empty` are answers — the provider was reached and said what it had. `fail` and `skip`
/// are not: nothing was learned from them.
pub(crate) fn any_courier_answered(attribution: &str) -> bool {
    attribution.split_whitespace().any(|entry| {
        entry
            .split_once('=')
            .map(|(_, status)| status.starts_with("ok") || status.starts_with("empty"))
            .unwrap_or(false)
    })
}

/// The result of looking an Omni-ID up on the rendezvous couriers.
#[derive(Debug)]
pub(crate) enum IdentityBeaconLookup {
    /// One beacon per device of the account — every device that turned discoverability on
    /// publishes its own under the Omni-ID (docs/decisions/multi-device-equal-nodes.md, 2.6).
    /// Freshest first; never empty.
    Found(Vec<omni_rendezvous::IdentityBeacon>),
    /// Couriers answered and nobody has a fresh beacon for this ID: the peer is either not
    /// findable-by-ID (discoverability Off — the default) or offline.
    NotPublished,
    /// No courier could be reached. We know nothing; saying "not findable" here would be a
    /// guess dressed as a fact.
    Unreachable,
}

/// The beacons of every device behind `omni_id`; empty when none could be had.
pub(crate) fn fetch_identity_beacons(
    courier: &dyn RendezvousCourier,
    omni_id: &[u8; 20],
) -> OmniResult<Vec<omni_rendezvous::IdentityBeacon>> {
    match lookup_identity_beacon(courier, omni_id)? {
        IdentityBeaconLookup::Found(beacons) => Ok(beacons),
        IdentityBeaconLookup::NotPublished | IdentityBeaconLookup::Unreachable => Ok(Vec::new()),
    }
}

pub(crate) fn lookup_identity_beacon(
    courier: &dyn RendezvousCourier,
    omni_id: &[u8; 20],
) -> OmniResult<IdentityBeaconLookup> {
    let now = current_unix_ms() as u64;
    // The freshest beacon of each device (node).
    let mut found: Vec<omni_rendezvous::IdentityBeacon> = Vec::new();
    let mut reached_any = false;
    for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(omni_id, now) {
        let (blobs, reached) = courier.fetch_with_reach(&topic.id(), &hex::encode(dht_key), CourierLegs::All)?;
        reached_any |= reached;
        for hex_blob in blobs {
            let Ok(bytes) = hex::decode(&hex_blob) else {
                continue;
            };
            let Some(beacon) = omni_rendezvous::IdentityBeacon::from_wire(&bytes) else {
                continue;
            };
            if &beacon.omni_id != omni_id
                || !beacon.is_fresh(now)
                || verify_identity_beacon(omni_id, &beacon).is_err()
            {
                continue;
            }
            match found.iter_mut().find(|known| known.node == beacon.node) {
                Some(known) if beacon.seq > known.seq => *known = beacon,
                Some(_) => {}
                None => found.push(beacon),
            }
        }
    }
    // A device whose freshest beacon is a withdrawal turned "findable by ID" off.
    found.retain(|beacon| !beacon.is_withdrawal());
    found.sort_by_key(|beacon| std::cmp::Reverse(beacon.seq));
    Ok(match (found.is_empty(), reached_any) {
        (false, _) => IdentityBeaconLookup::Found(found),
        (true, true) => IdentityBeaconLookup::NotPublished,
        (true, false) => IdentityBeaconLookup::Unreachable,
    })
}

fn spawn_rendezvous_dial(
    rendezvous: RendezvousRuntime,
    runtime: Arc<MeshRuntime>,
    secret: [u8; 32],
    couriers: PairCouriers,
    my_node: [u8; 20],
    skip_nodes: Vec<[u8; 20]>,
    my_relay_bundle: Option<String>,
    revocation_notice: Option<RevocationNotice>,
    consume_db_path: Option<String>,
    heard: Option<(PeerHeardLog, [u8; 20])>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("omni-rendezvous-dial".to_string())
        .spawn(move || {
            let outcome = rendezvous_dial_with(
                rendezvous.helper.as_ref(),
                &couriers,
                runtime.as_ref(),
                &secret,
                my_node,
                &skip_nodes,
                my_relay_bundle,
                revocation_notice,
                consume_db_path.clone(),
            );
            // The peer's relay bundle is now consumed INSIDE `rendezvous_connect` straight from the
            // opened beacon (decoupled from the dial), so it lands even when the direct dial fails
            // (NAT / UDP-hostile carrier) — which is exactly when relay propagation matters.
            // A fresh beacon of the peer on the topic = the peer is alive, whatever the dial did:
            // its silence ends here (see `silent_contact_sweep_due`).
            if let (Some((log, peer)), Ok(RendezvousOutcome::Dialed { .. } | RendezvousOutcome::NoEndpoints)) =
                (&heard, &outcome)
            {
                if let Ok(mut log) = log.lock() {
                    log.insert(*peer, std::time::Instant::now());
                }
            }
            match outcome {
                Ok(result) => omni_app_trace_log("rendezvous/dial", &format!("outcome: {result:?}")),
                Err(error) => omni_app_trace_log("rendezvous/dial", &format!("error: {error}")),
            }
        })
        .map(|_| ())
}

/// How long one courier subscription is held before it is renewed. A renewal also takes up a changed
/// set of topics (a contact that lost its direct link, a new window), so it is kept short.
const BEACON_WATCH_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// After a failed watch (helper gone, every courier refusing), wait this long before the next — a
/// failing courier must not be hammered in a tight loop.
const BEACON_WATCH_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// An empty answer sooner than this is not a subscription that ran its course: no courier could hold
/// one (`none-can-watch` — every one cooling down after 429s, or none subscribable) or all of them
/// failed at once. Treated as a failure, with the same pause. Without it the loop re-asked at once,
/// hundreds of times a second, each over a new loopback connection: 14 841 sockets in TIME_WAIT on the
/// laptop stand (2026-10-14), the ephemeral port range all but spent — other connects failed with
/// WSAEADDRINUSE.
const BEACON_WATCH_QUICK_EMPTY: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a watch that answered `blobs` after `elapsed` should be followed by a pause.
fn beacon_watch_needs_pause(blobs: &[String], elapsed: std::time::Duration) -> bool {
    blobs.is_empty() && elapsed < BEACON_WATCH_QUICK_EMPTY
}

/// Topics one subscription carries at most: a contact without a direct link brings two (this window
/// and the previous), so a subscription covers sixteen. ntfy takes the topics in the URL (64 hex
/// each), which bounds how many fit one request.
pub(crate) const BEACON_WATCH_TOPICS_PER_SUBSCRIPTION: usize = 32;

/// What a subscription listens for under one secret: a contact's device on our pair topic, or — with
/// `node: None` — on the account topic, any device of ours that is not in `exclude` (the ones already
/// linked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BeaconWatchTarget {
    pub(crate) secret: [u8; 32],
    pub(crate) node: Option<[u8; 20]>,
    pub(crate) exclude: Vec<[u8; 20]>,
    /// Where the pair's other device posts (see [`PairCouriers`]); `None` — our own set.
    pub(crate) read_from: Option<Vec<String>>,
}

/// What a fresh beacon on a subscription asks the worker to do: rendezvous that contact device, or
/// meet a device of ours it has not met yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BeaconWake {
    Contact([u8; 20]),
    OwnAccount,
}

/// The beacons among `blobs` that open under `secret`, quietly: on a shared subscription every target
/// tries every blob, and most of them belong to another target.
fn open_beacons_quietly(secret: &[u8; 32], blobs: &[Vec<u8>]) -> Vec<Beacon> {
    let now_ms = current_unix_ms() as u64;
    let topics = rendezvous_topics(secret, now_ms);
    let mut guard = ReplayGuard::new();
    blobs
        .iter()
        .filter_map(|blob| SealedBeacon::from_wire(blob))
        .filter_map(|sealed| {
            topics
                .iter()
                .find_map(|topic| open_beacon(secret, topic, &sealed, now_ms, &mut guard).ok())
        })
        .collect()
}

/// Which of `targets` the blobs that arrived on a subscription wake. Our own beacon never does.
fn beacon_wakes(targets: &[BeaconWatchTarget], my_node: [u8; 20], blobs: &[Vec<u8>]) -> Vec<BeaconWake> {
    let mut wakes: Vec<BeaconWake> = Vec::new();
    for target in targets {
        for beacon in open_beacons_quietly(&target.secret, blobs) {
            if beacon.node == my_node || target.exclude.contains(&beacon.node) {
                continue;
            }
            let wake = match target.node {
                Some(node) if beacon.node == node => BeaconWake::Contact(node),
                Some(_) => continue,
                None => BeaconWake::OwnAccount,
            };
            if !wakes.contains(&wake) {
                wakes.push(wake);
            }
        }
    }
    wakes
}

/// The topics a subscription for `targets` holds, without repeats — each with the couriers it is
/// watched at when they are a pair's own (`<topic>@<url>,<url>`).
fn beacon_watch_topics(targets: &[BeaconWatchTarget]) -> Vec<String> {
    let now_ms = current_unix_ms() as u64;
    let mut topics: Vec<String> = Vec::new();
    for target in targets {
        for topic in rendezvous_topics(&target.secret, now_ms) {
            let token = match target.read_from.as_deref() {
                Some(urls) if !urls.is_empty() => format!("{}@{}", topic.id(), urls.join(",")),
                _ => topic.id(),
            };
            if !topics.contains(&token) {
                topics.push(token);
            }
        }
    }
    topics
}

/// The shared target list of a profile's subscriptions, split into groups of
/// [`BEACON_WATCH_TOPICS_PER_SUBSCRIPTION`] topics; subscription `i` serves group `i`.
pub(crate) type BeaconWatchGroups = Arc<Mutex<Vec<Vec<BeaconWatchTarget>>>>;

/// Push-model wake-ups for one group of targets: hold ONE courier subscription on all their topics,
/// renewed every `BEACON_WATCH_WAIT` (taking up the group as it is then), and send a wake the moment a
/// contact's — or a not-yet-met own device's — fresh beacon is published, instead of finding it on the
/// next periodic look (90–120 s, up to 30 min for a silent contact).
///
/// It used to be a subscription per contact: with N contacts offline, N long connections to every
/// courier, renewed every two minutes — against ntfy's per-IP limit on open subscriptions.
///
/// Runs until `stop`, or until its group is gone (checked between subscriptions). Each subscription
/// takes the profile's runtime anew, so a changed courier set reaches the watches at their next renewal.
pub(crate) fn spawn_beacon_watch_group(
    db_path: String,
    groups: BeaconWatchGroups,
    index: usize,
    my_node: [u8; 20],
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: std::sync::mpsc::Sender<BeaconWake>,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    std::thread::Builder::new()
        .name("omni-beacon-watch".to_string())
        .spawn(move || loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let targets = match groups
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(index)
            {
                Some(targets) if !targets.is_empty() => targets.clone(),
                _ => return,
            };
            // The runtime is taken anew for each subscription: a changed courier set restarts it.
            let started = std::time::Instant::now();
            let watched = profile_rendezvous_runtime(&db_path)
                .and_then(|rendezvous| rendezvous.helper.watch(&beacon_watch_topics(&targets), BEACON_WATCH_WAIT))
                .and_then(|hex_blobs| {
                    if beacon_watch_needs_pause(&hex_blobs, started.elapsed()) {
                        Err(OmniError::Transport("no courier held the subscription".into()))
                    } else {
                        Ok(hex_blobs)
                    }
                });
            match watched {
                Ok(hex_blobs) => {
                    let blobs: Vec<Vec<u8>> =
                        hex_blobs.iter().filter_map(|hex_blob| hex::decode(hex_blob).ok()).collect();
                    for woken in beacon_wakes(&targets, my_node, &blobs) {
                        if stop.load(Ordering::Relaxed) || wake.send(woken).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    omni_app_trace_log("rendezvous/watch", &format!("failed: {error}"));
                    let until = std::time::Instant::now() + BEACON_WATCH_RETRY_AFTER;
                    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        })
        .map(|_| ())
}

/// Apply a peer's propagated `om3relay:` bundle from the rendezvous worker (no `&self`): open a fresh
/// `Storage` on `db_path` and run the first-party-gated `apply_relay_bundle` (issuer MUST be the dialed
/// `peer_node`). Best-effort + traced — a propagation failure never sinks the dial. R1c inc.3b.
fn apply_propagated_relay_bundle_on_worker(db_path: &str, peer_node: [u8; 20], token: &str) {
    let storage = match omni_storage::Storage::open(db_path) {
        Ok(storage) => storage,
        Err(error) => {
            omni_app_trace_log("rendezvous/relay-bundle", &format!("open storage failed: {error}"));
            return;
        }
    };
    let issuer = MeshNodeAddress::from_bytes(peer_node).to_string();
    match apply_relay_bundle(&storage, token, Some(&issuer)) {
        Ok(n) => omni_app_trace_log("rendezvous/relay-bundle", &format!("applied {n} relay(s) from {issuer}")),
        Err(error) => omni_app_trace_log("rendezvous/relay-bundle", &format!("rejected: {error}")),
    }
}

impl OmniApp {
    /// DEBUG/validation: the current blinded rendezvous topic id(s) for `account_id` — the exact
    /// public keys my beacon publishes to / fetches on (current + previous time bucket), for each
    /// device of the account in turn. Two unlinked devices auto-rendezvous ONLY if these match on
    /// BOTH sides (i.e. they share the same per-pair secret). `None` if no device has a rendezvous
    /// secret yet. Used to diagnose a non-converging beacon exchange (both sides `NoPeerBeacon` ⇒
    /// topic mismatch ⇒ secret mismatch).
    pub fn rendezvous_topic_for_contact(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Option<Vec<String>>> {
        let now = current_unix_ms() as u64;
        let topics: Vec<String> = rendezvous_peers_for_account(&self.storage, account_id)
            .iter()
            .flat_map(|peer| rendezvous_topics(&peer.secret, now))
            .map(|topic| topic.id())
            .collect();
        Ok((!topics.is_empty()).then_some(topics))
    }

    /// This node's mesh NodeAddress (20 bytes), or None if the mesh isn't started.
    fn mesh_node_bytes(&self) -> Option<[u8; 20]> {
        self.mesh_runtime().map(|rt| *rt.node_address().as_bytes())
    }

    /// Connect to `account_id` over serverless rendezvous when there is no mesh path: get
    /// my reflexive endpoint from the helper's STUN consensus, publish my beacon + fetch
    /// the contact's via the helper, then dial the contact at its reflexive endpoints
    /// (`MeshRuntime::dial_at`). The relay→direct hole-punch upgrade then runs on the mesh
    /// reconcile timer. See docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
    pub fn rendezvous_dial(
        &self,
        helper: &RendezvousHelper,
        account_id: &AccountId,
    ) -> OmniResult<RendezvousOutcome> {
        let Some(my_node) = self.mesh_node_bytes() else {
            return Err(OmniError::Transport(
                "mesh not started — no node address for rendezvous".to_string(),
            ));
        };
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh not started".to_string()))?;
        // One device of the account: the first not linked yet, else the first. Its node and its pair
        // secret come from the same session — mixing two devices' would never converge.
        let peers = rendezvous_peers_for_account(&self.storage, account_id);
        let Some(peer) = peers
            .iter()
            .find(|peer| !runtime.has_link(&peer.node))
            .or_else(|| peers.first())
        else {
            return Ok(RendezvousOutcome::NoPeerBeacon);
        };
        let secret = peer.secret;
        // Publish MY current relay bundle (R1c inc.3b) alongside my reflexive endpoints, so the
        // contact auto-learns my rotated relay set. Best-effort: a read error just omits the bundle.
        let my_relay_bundle = self.my_relay_bundle_token().ok().flatten();
        // Consume a propagated bundle straight from the opened beacon (inside `rendezvous_connect`),
        // decoupled from the dial — so it lands even when the direct dial fails (R1c inc.3b). The
        // first-party gate (issuer == peer node) is applied there via `apply_relay_bundle`.
        let consume_db_path = Some(self.storage.db_path().to_string_lossy().to_string());
        rendezvous_dial_with(
            helper,
            &PairCouriers::default(),
            runtime.as_ref(),
            &secret,
            my_node,
            &[],
            my_relay_bundle,
            peer.revocation_notice.clone(),
            consume_db_path,
        )
    }

    /// The owned, long-lived rendezvous runtime for the active profile — spawns the windowless
    /// helpers once and reuses them for every later dial (see `RendezvousRuntime`). Spawns the
    /// `omni-dht-helper` FIRST so its loopback endpoint can be handed to the rendezvous child
    /// (`OMNI_DHT_HELPER_ENDPOINT`); if the DHT helper can't start, the rendezvous helper is still
    /// spawned without it (the BT-DHT leg is then skipped and N-of-many covers it — a missing DHT
    /// engine must never block a dial). See docs/decisions/omni-dht-helper.md (R7 Phase 2).
    fn ensure_rendezvous_runtime(&self) -> OmniResult<RendezvousRuntime> {
        // The helper gets THIS profile's courier set (storage / invite-merged / default); a change in
        // Settings restarts it (`restart_rendezvous_with_new_couriers`). See global-identity-discovery.md.
        profile_rendezvous_runtime(&self.storage.db_path().to_string_lossy())
    }

    /// Connect to `account_id` over serverless rendezvous using the APP-OWNED helper: the
    /// caller-managed variant is [`OmniApp::rendezvous_dial`]; this one owns the helper
    /// lifecycle (spawn-once + reuse) and the cross-call replay guard, so a trigger only has
    /// to name the contact. NOTE: this performs SLOW network I/O (STUN consensus + provider
    /// fetch + dial — seconds), so a caller on the FFI/serializer path MUST run it off the
    /// hot thread (e.g. a dedicated background thread) or it will block the whole app. See
    /// docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
    pub fn rendezvous_dial_contact(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<RendezvousOutcome> {
        let runtime = self.ensure_rendezvous_runtime()?;
        self.rendezvous_dial(runtime.helper.as_ref(), account_id)
    }

    /// The mesh node that stands for `account_id`: of its devices' nodes (each from that device's
    /// stored prekey bundle — the Ed25519 signing key both sides derive the address from), one we
    /// have a link to, else the latest device's. A contact is reachable when any of its devices is
    /// (docs/decisions/multi-device-equal-nodes.md). The account-keyed sibling of
    /// [`OmniApp::mesh_node_for_contact_dialog`]; `Ok(None)` when there is no stored bundle yet.
    fn mesh_node_for_account(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Option<MeshNodeAddress>> {
        let nodes: Vec<MeshNodeAddress> = self
            .storage
            .list_remote_prekey_bundles_for_account(account_id)
            .unwrap_or_default()
            .iter()
            .filter_map(|bundle| node_address_from_ed25519_pubkey(bundle.identity_signing_key.0).ok())
            .collect();
        let linked = self
            .mesh_runtime()
            .and_then(|runtime| nodes.iter().find(|node| runtime.has_link(node)).copied());
        Ok(linked.or_else(|| nodes.first().copied()))
    }

    /// Auto-trigger for "no mesh path to an approved contact": for each device of `account_id`
    /// the mesh has NO link to, kick off a serverless-rendezvous dial to reach it — each device is
    /// its own node with its own pair secret. Returns `Ok(true)` if a dial was started,
    /// `Ok(false)` when nothing was needed or possible (every device linked, mesh not started, or
    /// no device is rendezvous-eligible — no session / no pair secret yet).
    ///
    /// All eligibility checks and the consistent snapshot (the unsealed pair secret + my
    /// mesh node) run HERE, under the caller's lock — they are fast — and only the SLOW
    /// dial (STUN consensus + provider publish/fetch + mesh dial, seconds) is moved to a
    /// background thread, so the hot/FFI path is never blocked. The owned helper, the mesh
    /// runtime, and the replay guard cross to the worker as `Arc`s (plus the `Copy`
    /// secret/node); no `&self`/storage crosses the thread boundary — which also avoids
    /// re-unsealing the secret off the unlocked profile context. A successful dial surfaces
    /// as the mesh link coming up. See docs/decisions/serverless-bootstrap-rendezvous.md.
    pub fn rendezvous_reach_contact_if_no_link(&self, account_id: &AccountId) -> OmniResult<bool> {
        // Honour the profile's connection-privacy choice: `DirectOnly` means "zero third
        // parties", so the on-demand trigger must not touch a public rendezvous provider
        // (STUN/ntfy) either — exactly as the autoconnect sweep gates itself with
        // `uses_rendezvous()`. Gating HERE, at the single core entry point, means no caller
        // (FFI / shell / a future one) can bypass the privacy guarantee. Checked first, so
        // `DirectOnly` short-circuits before any mesh/helper interaction.
        if !self.connection_privacy_mode().uses_rendezvous() {
            return Ok(false);
        }
        let Some(runtime) = self.mesh_runtime() else {
            return Ok(false); // mesh not started — nothing to dial onto
        };
        // Devices without a session or with a pre-rendezvous one (no blinded pair secret) are not
        // in the list; linked ones are already reachable on the mesh.
        let unlinked: Vec<RendezvousPeer> = rendezvous_peers_for_account(&self.storage, account_id)
            .into_iter()
            .filter(|peer| !runtime.has_link(&peer.node))
            .collect();
        if unlinked.is_empty() {
            return Ok(false);
        }
        let Some(my_node) = self.mesh_node_bytes() else {
            return Ok(false);
        };
        let rendezvous = self.ensure_rendezvous_runtime()?;
        // Publish MY relay bundle (R1c inc.3b) in the worker's beacon (computed here under the lock,
        // off the worker — the worker holds no `&self`/storage). `None` when I share no relay.
        let my_relay_bundle = self.my_relay_bundle_token().ok().flatten();
        // Approved-contact per-pair path → consume the peer's relay bundle too (first-party gated on
        // the worker, fresh Storage). See docs/decisions/r1c-rotatable-white-relay.md.
        let consume_db_path = Some(self.storage.db_path().to_string_lossy().to_string());
        for peer in unlinked {
            spawn_rendezvous_dial(
                rendezvous.clone(),
                runtime.clone(),
                peer.secret,
                peer.couriers.clone(),
                my_node,
                Vec::new(),
                my_relay_bundle.clone(),
                peer.revocation_notice.clone(),
                consume_db_path.clone(),
                None,
            )
            .map_err(|error| OmniError::Internal(format!("spawn rendezvous dial thread: {error}")))?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod beacon_watch_pause_tests {
    use super::{beacon_watch_needs_pause, BEACON_WATCH_WAIT};
    use std::time::Duration;

    #[test]
    fn an_empty_answer_at_once_pauses_and_a_subscription_that_ran_or_woke_does_not() {
        assert!(beacon_watch_needs_pause(&[], Duration::from_millis(3)), "none-can-watch: no hammering");
        assert!(!beacon_watch_needs_pause(&[], BEACON_WATCH_WAIT), "ran its course: renew at once");
        assert!(!beacon_watch_needs_pause(&["ab".to_string()], Duration::from_millis(3)), "woke: go on");
    }
}

#[cfg(test)]
mod rendezvous_helper_tests {
    use super::{
        CourierLegs, PublishPriority,
        build_self_beacon, current_unix_ms, dht_helper_get, dht_helper_put, dht_proxy_admit_origin,
        any_courier_answered, fetch_beacon_via, fetch_identity_beacons, helper_binary_path,
        helper_launch, identity_beacon_fingerprint, identity_beacon_changed,
        lookup_identity_beacon, IdentityBeaconLookup,
        open_beacon_under_secret, publish_beacon_via, rendezvous_connect, verify_identity_beacon,
        HELPER_SELF_EXEC_ENV,
        rendezvous_topics_with_keys, seal_beacon, seal_beacon_under_secret, DhtOverPeerLeg,
        DhtSlotClaimer, NatType, OmniResult, RendezvousCourier, RendezvousHelper, RendezvousOutcome,
        DHT_PROXY_MAX_SERVES_PER_WINDOW, DHT_PROXY_WINDOW_MS, DHT_SLOT_SALTS,
    };
    use std::collections::HashMap;

    /// The helper's `OK <n>` is a count, and `OK 0` means every courier refused — that must not
    /// read as a publication (live 2026-10-03: `published=2` while ntfy said 429 and Nostr refused).
    #[test]
    fn a_publish_no_courier_accepted_is_not_a_publication() {
        use super::publish_reply_accepted;
        // Negative half: every refusal shape is an error.
        assert!(publish_reply_accepted("OK 0").is_err(), "OK 0 = nobody took it");
        assert!(publish_reply_accepted("ERR bad topic hex").is_err());
        assert!(publish_reply_accepted("OK").is_err(), "a reply without its count is malformed");
        // Positive half: one courier is enough.
        assert!(publish_reply_accepted("OK 1").is_ok());
        assert!(publish_reply_accepted("OK 3").is_ok());
    }
    use std::sync::Mutex;

    /// An in-process rendezvous courier (topic → published blobs). Lets the
    /// publish→fetch→open beacon flow be tested locally, without the real providers
    /// (which need the network). A single instance stands in for the shared channel both
    /// contacts publish to / fetch from.
    #[derive(Default)]
    struct InMemoryCourier {
        topics: Mutex<HashMap<String, Vec<String>>>,
    }

    impl RendezvousCourier for InMemoryCourier {
        fn publish(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, _ttl_secs: u32, _priority: PublishPriority) -> OmniResult<()> {
            // Mirror the real courier helper's wire contract: the dhtkey token is
            // mandatory and non-empty. An empty key is a malformed `publish`/`fetch`
            // command the helper rejects — encoding that here is what guards against the
            // identity-beacon regression where the public topic passed `""`.
            assert!(!dhtkey_hex.is_empty(), "courier dhtkey must be non-empty");
            self.topics
                .lock()
                .unwrap()
                .entry(topic_hex.to_string())
                .or_default()
                .push(blob_hex.to_string());
            Ok(())
        }
        fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, _legs: CourierLegs) -> OmniResult<Vec<String>> {
            assert!(!dhtkey_hex.is_empty(), "courier dhtkey must be non-empty");
            Ok(self
                .topics
                .lock()
                .unwrap()
                .get(topic_hex)
                .cloned()
                .unwrap_or_default())
        }
    }

    /// A certified device of the account behind `root` and a beacon it signed.
    fn signed_identity_beacon(
        root: u8,
    ) -> (
        omni_rendezvous::IdentityBeacon,
        [u8; 20],
        omni_crypto::LocalIdentityKeyPair,
    ) {
        signed_identity_beacon_of_device(root, "dev-x")
    }

    /// The identity beacon of device `device` of the account behind root secret `root`.
    fn signed_identity_beacon_of_device(
        root: u8,
        device: &str,
    ) -> (
        omni_rendezvous::IdentityBeacon,
        [u8; 20],
        omni_crypto::LocalIdentityKeyPair,
    ) {
        use omni_crypto::{generate_local_key_hierarchy, AccountMasterKey, DEFAULT_ONE_TIME_PREKEY_COUNT};
        use omni_identity::{AccountRootSecret, DeviceId};

        let master = AccountMasterKey::derive(&AccountRootSecret(hex::encode([root; 32]))).unwrap();
        let working = master.working_key(1).unwrap();
        let working_certificate = omni_crypto::issue_working_key_certificate(&master, &working, 1_000);
        let hierarchy = generate_local_key_hierarchy(
            &master.public_key().account_id(),
            &DeviceId(device.into()),
            1_000,
            DEFAULT_ONE_TIME_PREKEY_COUNT,
        )
        .expect("keys");
        let identity = hierarchy.identity_key.clone();
        let chain = omni_crypto::issue_device_certificate(
            &working_certificate,
            &working,
            &DeviceId(device.into()),
            identity.public_key,
            identity.signing_public_key,
            hierarchy.device_key.public_key,
            1_000,
        )
        .unwrap();
        let omni_id = omni_crypto::omni_id_bytes(&master.public_key());
        let node = crate::node_address_from_ed25519_pubkey(identity.signing_public_key.0).unwrap();
        let now = current_unix_ms() as u64;
        let mut beacon = omni_rendezvous::IdentityBeacon {
            omni_id,
            chain_compact: chain.to_compact(),
            node: *node.as_bytes(),
            prekey_compact: vec![1],
            device_label: String::new(),
            reflexive: vec!["203.0.113.9:51820".to_string()],
            tcp_direct: vec![],
            relay_bundle: None,
            ts_ms: now,
            ttl_secs: 900,
            seq: now,
            signature: vec![],
        };
        beacon.signature = omni_crypto::sign_message(
            &identity.protected_signing_private_key,
            &omni_rendezvous::identity_beacon_signing_bytes(&beacon),
        )
        .expect("sign")
        .bytes;
        (beacon, omni_id, identity)
    }

    #[test]
    fn looking_an_omni_id_up_finds_every_device_of_the_account() {
        let (laptop, omni_id, _) = signed_identity_beacon_of_device(4, "laptop");
        let (desktop, desktop_id, _) = signed_identity_beacon_of_device(4, "desktop");
        assert_eq!(omni_id, desktop_id, "one account, one Omni-ID");
        assert_ne!(laptop.node, desktop.node, "two devices, two nodes");
        let now = current_unix_ms() as u64;
        let courier = InMemoryCourier::default();
        for beacon in [&laptop, &desktop] {
            for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(&omni_id, now) {
                courier
                    .publish(&topic.id(), &hex::encode(dht_key), &hex::encode(beacon.to_wire()), 900, PublishPriority::Contact)
                    .unwrap();
            }
        }
        let mut nodes: Vec<[u8; 20]> = fetch_identity_beacons(&courier, &omni_id)
            .unwrap()
            .iter()
            .map(|beacon| beacon.node)
            .collect();
        nodes.sort();
        let mut expected = vec![laptop.node, desktop.node];
        expected.sort();
        assert_eq!(nodes, expected);
    }

    /// "Findable by ID" turned off: the device's withdrawal (newer, no bundle) hides it from a
    /// look-up at once, though its last real beacon is still on the courier and still valid; the
    /// account's other device is still found; turned on again, a newer real beacon brings it back.
    #[test]
    fn a_withdrawn_device_is_not_found_by_its_omni_id() {
        let (laptop, omni_id, laptop_key) = signed_identity_beacon_of_device(5, "laptop");
        let (desktop, _, _) = signed_identity_beacon_of_device(5, "desktop");
        let resign = |mut beacon: omni_rendezvous::IdentityBeacon| {
            beacon.signature = omni_crypto::sign_message(
                &laptop_key.protected_signing_private_key,
                &omni_rendezvous::identity_beacon_signing_bytes(&beacon),
            )
            .unwrap()
            .bytes;
            beacon
        };
        let withdrawal = resign(omni_rendezvous::IdentityBeacon {
            prekey_compact: Vec::new(),
            reflexive: vec![],
            seq: laptop.seq + 1,
            ..laptop.clone()
        });
        let now = current_unix_ms() as u64;
        let courier = InMemoryCourier::default();
        let post = |beacon: &omni_rendezvous::IdentityBeacon| {
            for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(&omni_id, now) {
                courier
                    .publish(&topic.id(), &hex::encode(dht_key), &hex::encode(beacon.to_wire()), 900, PublishPriority::Contact)
                    .unwrap();
            }
        };
        for beacon in [&laptop, &desktop, &withdrawal] {
            post(beacon);
        }
        let nodes: Vec<[u8; 20]> = fetch_identity_beacons(&courier, &omni_id).unwrap().iter().map(|b| b.node).collect();
        assert_eq!(nodes, vec![desktop.node], "the withdrawn laptop is not found, the desktop is");

        post(&resign(omni_rendezvous::IdentityBeacon { seq: laptop.seq + 2, ..laptop.clone() }));
        let nodes: Vec<[u8; 20]> = fetch_identity_beacons(&courier, &omni_id).unwrap().iter().map(|b| b.node).collect();
        assert!(nodes.contains(&laptop.node), "findable again once a newer real beacon is out");
    }

    #[test]
    fn fetch_identity_beacon_verifies_commitment_and_signature() {
        let (beacon, omni_id, identity) = signed_identity_beacon(1);
        let now = current_unix_ms() as u64;

        // Published + valid → fetched and verified.
        let courier = InMemoryCourier::default();
        for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(&omni_id, now) {
            courier
                .publish(&topic.id(), &hex::encode(dht_key), &hex::encode(beacon.to_wire()), 900, PublishPriority::Contact)
                .unwrap();
        }
        assert_eq!(fetch_identity_beacons(&courier, &omni_id).unwrap(), vec![beacon.clone()]);

        // Tampered signature → rejected (a courier can't forge for an ID it doesn't own).
        let mut bad = beacon.clone();
        bad.signature = vec![9u8; 64];
        let courier2 = InMemoryCourier::default();
        for (topic, dht_key) in omni_rendezvous::identity_topics_with_keys(&omni_id, now) {
            courier2
                .publish(&topic.id(), &hex::encode(dht_key), &hex::encode(bad.to_wire()), 900, PublishPriority::Contact)
                .unwrap();
        }
        assert!(fetch_identity_beacons(&courier2, &omni_id).unwrap().is_empty());

        // Another account's valid, self-signed beacon passed off under this ID → rejected.
        let (other, _, _) = signed_identity_beacon(2);
        assert!(verify_identity_beacon(&omni_id, &other).is_err());

        // A node that is not the certified device's → rejected, even re-signed by the device.
        let mut wrong_node = beacon.clone();
        wrong_node.node = [7u8; 20];
        wrong_node.signature = omni_crypto::sign_message(
            &identity.protected_signing_private_key,
            &omni_rendezvous::identity_beacon_signing_bytes(&wrong_node),
        )
        .unwrap()
        .bytes;
        assert!(verify_identity_beacon(&omni_id, &wrong_node).is_err());
    }

    #[test]
    fn an_unchanged_identity_beacon_is_not_a_change() {
        // The waste that starved ntfy.sh: a 900 s beacon re-posted every ~100 s. Unchanged, it goes
        // only where a block of the Omni-ID plan is due (app_publish_plans).
        assert!(identity_beacon_changed(None, 7), "the first publish always goes out");
        assert!(!identity_beacon_changed(Some((1_700_000_000_000, 7)), 7));
    }

    #[test]
    fn a_beacon_whose_endpoints_changed_republishes_at_once() {
        // A network switch must not leave a stale endpoint out there for minutes — the
        // floor is on time, not on content.
        let now = 1_700_000_000_000i64;
        let _ = now;
        assert!(identity_beacon_changed(Some((now, 7)), 8));

        let a = identity_beacon_fingerprint(&[1u8; 20], &["1.2.3.4:1".into()], &[], None, b"{}");
        let moved = identity_beacon_fingerprint(&[1u8; 20], &["5.6.7.8:1".into()], &[], None, b"{}");
        let door = identity_beacon_fingerprint(
            &[1u8; 20],
            &["1.2.3.4:1".into()],
            &["1.2.3.4:2".into()],
            None,
            b"{}",
        );
        assert_ne!(a, moved, "a new reflexive endpoint must republish");
        assert_ne!(a, door, "a new TCP front door must republish");
        assert_eq!(
            a,
            identity_beacon_fingerprint(&[1u8; 20], &["1.2.3.4:1".into()], &[], None, b"{}"),
            "identical content must fingerprint identically"
        );
    }

    #[test]
    fn a_courier_that_was_asked_and_had_nothing_counts_as_an_answer() {
        // `empty` is a real answer: that provider was reached and has no beacon. Only `ok`
        // and `empty` let us say anything about the peer at all.
        assert!(any_courier_answered(
            "ntfy:ntfy.sh=empty+174ms nostr:offchain.pub=ok+1028ms"
        ));
        assert!(any_courier_answered("ntfy:ntfy.sh=empty+174ms"));
    }

    #[test]
    fn a_lookup_where_every_courier_failed_or_was_skipped_answers_nothing() {
        // Exactly the live shape on 2026-08-11 once nostr also blipped: reporting this as
        // "the peer is not findable" would be a claim about someone else's settings that we
        // have no evidence for.
        assert!(!any_courier_answered(
            "ntfy:ntfy.sh=fail+500ms nostr:offchain.pub=skip"
        ));
        assert!(!any_courier_answered("dht:mainline=skip"));
        assert!(!any_courier_answered(""));
    }

    #[test]
    fn an_unreachable_lookup_is_not_reported_as_an_absent_peer() {
        struct DeadCouriers;
        impl RendezvousCourier for DeadCouriers {
            fn publish(&self, _: &str, _: &str, _: &str, _: u32, _priority: PublishPriority) -> OmniResult<()> {
                Ok(())
            }
            fn fetch(&self, _: &str, _: &str, _: CourierLegs) -> OmniResult<Vec<String>> {
                Ok(Vec::new())
            }
            fn fetch_with_reach(&self, _: &str, _: &str, _: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
                Ok((Vec::new(), false))
            }
        }

        assert!(matches!(
            lookup_identity_beacon(&DeadCouriers, &[3u8; 20]).unwrap(),
            IdentityBeaconLookup::Unreachable
        ));
        // While a courier that answered with nothing IS evidence of absence.
        assert!(matches!(
            lookup_identity_beacon(&InMemoryCourier::default(), &[3u8; 20]).unwrap(),
            IdentityBeaconLookup::NotPublished
        ));
    }

    /// Locate the cargo-built helper binary relative to the test runner
    /// (`target/<triple>/debug/deps/omni_app-*` → `…/debug/omni-rendezvous-helper`).
    fn built_helper_binary() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        let deps = exe.parent().expect("deps dir");
        let debug = deps.parent().expect("debug dir");
        debug.join(format!(
            "omni-rendezvous-helper{}",
            std::env::consts::EXE_SUFFIX
        ))
    }

    #[test]
    fn helper_binary_path_prefers_the_omni_helper_dir_override() {
        // A host that keeps the helpers somewhere other than next to itself points us at that dir via
        // OMNI_HELPER_DIR; the override must win over the sibling lookup.
        let dir = std::env::temp_dir().join(format!("omni-helper-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let name = format!("omni-rendezvous-helper{}", std::env::consts::EXE_SUFFIX);
        let planted = dir.join(&name);
        std::fs::write(&planted, b"stand-in for the extracted helper").expect("plant helper");

        std::env::set_var("OMNI_HELPER_DIR", &dir);
        let resolved = helper_binary_path("omni-rendezvous-helper");
        // A dir that exists but holds no such helper must fall through to the sibling lookup rather
        // than hard-fail on the override.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).expect("empty dir");
        std::env::set_var("OMNI_HELPER_DIR", &empty);
        let fell_through = helper_binary_path("omni-rendezvous-helper");
        std::env::remove_var("OMNI_HELPER_DIR");

        assert_eq!(resolved.expect("resolved via override"), planted);
        // The fall-through resolves to the cargo-built sibling when present, else errors — either way
        // it must NOT be the (absent) override path.
        if let Ok(path) = fell_through {
            assert_ne!(path.parent(), Some(empty.as_path()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A kind no build ever produces a binary for, so the sibling lookup is guaranteed to miss and
    /// the fallback is what's under test (for the real kinds, cargo's own bins are sitting right
    /// next to the test runner and would win).
    const KIND_WITH_NO_BINARY: &str = "absent-in-this-build";

    /// Both halves of the resolution order live in ONE test on purpose: they drive the same process
    /// env var, and as two `#[test]`s cargo's thread-parallel runner has each one clearing the var
    /// out from under the other.
    #[test]
    fn helper_launch_prefers_a_sibling_and_self_execs_only_when_the_host_opted_in() {
        std::env::remove_var(HELPER_SELF_EXEC_ENV);
        let without_opt_in = helper_launch(KIND_WITH_NO_BINARY);

        std::env::set_var(HELPER_SELF_EXEC_ENV, "1");
        let with_opt_in = helper_launch(KIND_WITH_NO_BINARY);
        // Self-exec is the LAST resort: with a real binary on disk (the normal multi-file layout) it
        // must still win, keeping the dev build's behaviour exactly what it was.
        let sibling = built_helper_binary()
            .exists()
            .then(|| helper_launch("rendezvous"));
        std::env::remove_var(HELPER_SELF_EXEC_ENV);

        // Without the opt-in a missing sibling is an error — NOT a relaunch of whatever we happen to
        // be running, which for a host that ignores `--run-helper` would fork the whole app.
        assert!(without_opt_in.is_err());
        let launch = with_opt_in.expect("self-exec fallback");
        assert_eq!(launch.program, std::env::current_exe().expect("current_exe"));
        assert_eq!(
            launch.prefix_args,
            vec!["--run-helper".to_string(), KIND_WITH_NO_BINARY.to_string()]
        );
        if let Some(sibling) = sibling {
            let sibling = sibling.expect("sibling resolved");
            assert!(sibling.prefix_args.is_empty());
            assert_eq!(
                sibling.program.file_stem(),
                built_helper_binary().file_stem()
            );
        }
    }

    #[test]
    fn spawns_helper_and_round_trips_a_line_command() {
        let bin = built_helper_binary();
        if !bin.exists() {
            // The helper bin isn't built in this test invocation — skip rather than fail.
            eprintln!("SKIP: helper binary not built at {}", bin.display());
            return;
        }
        let helper = RendezvousHelper::spawn_binary(&bin).expect("spawn helper");
        assert!(
            helper.endpoint().starts_with("127.0.0.1:"),
            "bound to an ephemeral loopback port, got {}",
            helper.endpoint()
        );
        // An unknown command round-trips a reply WITHOUT touching the network — proves the
        // full spawn → connect → send → reply path deterministically offline.
        let reply = helper.command("not-a-real-command").expect("command round-trips");
        assert!(
            reply.starts_with("ERR unknown command"),
            "expected unknown-command error, got: {reply}"
        );
        // Dropping the helper kills the child (verified by no leaked process — best-effort).
    }

    /// The core's own publish and fetch lines, through the real helper process, reach its handlers
    /// whole: each comes back with a per-courier report. A helper that split `publish` into too few
    /// parts answered "ERR publish needs …" to every post (2026-10-10), with no report at all — and the
    /// in-process tests, calling the handlers directly, passed. Offline: the only courier is a closed
    /// local port, and a read-only one is reported for the fetch only.
    #[test]
    fn the_cores_publish_and_fetch_lines_reach_the_real_helper_whole() {
        use super::{CourierLegs, PublishPriority};
        let bin = built_helper_binary();
        if !bin.exists() {
            eprintln!("SKIP: helper binary not built at {}", bin.display());
            return;
        }
        let couriers = vec!["http://127.0.0.1:1".to_string(), "http://127.0.0.2:1#read".to_string()];
        let helper = RendezvousHelper::spawn_binary_with_providers(&bin, &couriers, None).expect("spawn helper");
        let (topic, key) = ("ab".repeat(32), "5a".repeat(32));
        let _ = helper.publish(&topic, &key, "00", 600, PublishPriority::Contact, None, None);
        let posted = helper.courier_status("ntfy:127.0.0.1:1").expect("the publish line reached the helper whole");
        assert_eq!(posted.state, "fail", "a closed port does not answer");
        assert!(helper.courier_status("ntfy:127.0.0.2:1").is_none(), "a read-only courier takes no post");
        let _ = helper.fetch(&topic, &key, CourierLegs::Web);
        assert!(helper.courier_status("ntfy:127.0.0.2:1").is_some(), "and is read");
    }

    #[test]
    fn owned_helper_runtime_spawns_once_reuses_then_clears() {
        use super::{clear_rendezvous_runtime_for_profile, ensure_rendezvous_runtime_with, RendezvousRuntime};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bin = built_helper_binary();
        if !bin.exists() {
            eprintln!("SKIP: helper binary not built at {}", bin.display());
            return;
        }
        // A db_path unique to this test so the shared registry doesn't collide with others.
        let db = "owned-helper-runtime-spawns-once.sqlite";
        let spawns = AtomicUsize::new(0);
        // The runtime's lifecycle (spawn-once / reuse / clear) is independent of the DHT leg, so this
        // test builds a runtime with only the rendezvous helper (`dht_helper: None`).
        let spawn_one = || {
            spawns.fetch_add(1, Ordering::SeqCst);
            Ok(RendezvousRuntime {
                helper: std::sync::Arc::new(RendezvousHelper::spawn_binary(&bin)?),
                dht_helper: None,
            })
        };

        let rt1 = ensure_rendezvous_runtime_with(db, spawn_one).expect("spawn 1");
        let endpoint = rt1.helper.endpoint().to_string();
        // A second call for the same profile reuses the running helper — spawn NOT invoked.
        let rt2 = ensure_rendezvous_runtime_with(db, spawn_one).expect("reuse");
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "reused, not re-spawned");
        assert_eq!(rt2.helper.endpoint(), endpoint.as_str(), "same owned helper process");

        // Stopping the profile drops the helper; the next call spawns a fresh one.
        clear_rendezvous_runtime_for_profile(db);
        let rt3 = ensure_rendezvous_runtime_with(db, spawn_one).expect("respawn after clear");
        assert_eq!(spawns.load(Ordering::SeqCst), 2, "re-spawned after clear");
        let _ = (rt1, rt2, rt3);
        clear_rendezvous_runtime_for_profile(db);
    }

    /// The DHT-over-peer responder's IPC client (`dht_helper_put`/`get`) round-trips against a mock
    /// `omni-dht-helper` line server (no real DHT, no network) — proves we speak the helper's put/get
    /// protocol. The full mesh round-trip is covered once the requester await lands (2e-2). See
    /// docs/decisions/dht-over-peer.md.
    #[test]
    fn dht_helper_ipc_client_put_get_round_trips() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::sync::Arc;

        // Mock helper: a loopback line server backed by an in-memory (key_hex,salt_hex)->value_hex map.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock helper");
        let endpoint = listener.local_addr().unwrap().to_string();
        let store: Arc<Mutex<HashMap<(String, String), String>>> = Arc::new(Mutex::new(HashMap::new()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let Ok(read_half) = stream.try_clone() else { return };
                    let mut reader = BufReader::new(read_half);
                    let mut writer = stream;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let mut p = trimmed.splitn(4, ' ');
                        let reply = match p.next() {
                            Some("put") => {
                                if let (Some(k), Some(s), Some(v)) = (p.next(), p.next(), p.next()) {
                                    store.lock().unwrap().insert((k.into(), s.into()), v.into());
                                    "OK".to_string()
                                } else {
                                    "ERR".to_string()
                                }
                            }
                            Some("get") => {
                                if let (Some(k), Some(s)) = (p.next(), p.next()) {
                                    match store.lock().unwrap().get(&(k.into(), s.into())) {
                                        Some(v) => format!("OK {v}"),
                                        None => "OK".to_string(),
                                    }
                                } else {
                                    "ERR".to_string()
                                }
                            }
                            _ => "ERR".to_string(),
                        };
                        if writeln!(writer, "{reply}").is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                });
            }
        });

        let key = [0x5Au8; 32];
        let salt = b"0";
        let value = b"sealed-beacon-bytes";

        // An empty slot reads back as an OK-with-no-value.
        assert_eq!(dht_helper_get(&endpoint, &key, salt), Ok(None), "empty slot is an OK miss");
        // Put, then get returns exactly what we wrote.
        assert!(dht_helper_put(&endpoint, &key, salt, value), "put accepted");
        assert_eq!(
            dht_helper_get(&endpoint, &key, salt),
            Ok(Some(value.to_vec())),
            "get returns the put value"
        );
        // A dead endpoint is an IPC failure, not a false miss.
        assert_eq!(dht_helper_get("127.0.0.1:1", &key, salt), Err(()), "dead helper = Err, not miss");
        assert!(!dht_helper_put("127.0.0.1:1", &key, salt, value), "dead helper put fails");
    }

    #[test]
    fn upnp_beacon_candidate_advertise_policy() {
        use super::upnp_beacon_candidate;
        use std::net::{IpAddr, SocketAddr};
        let upnp: SocketAddr = "203.0.113.5:50149".parse().unwrap();
        let other: SocketAddr = "203.0.113.5:63006".parse().unwrap();
        let egress_match: IpAddr = "203.0.113.5".parse().unwrap(); // == UPnP WAN IP (router IS our egress)
        let egress_tunnel: IpAddr = "198.51.100.9".parse().unwrap(); // VPN exit / double-NAT — different IP
        // No UPnP mapping → nothing to advertise.
        assert_eq!(upnp_beacon_candidate(None, None, None, true), None);
        // AutoNAT confirmed THIS endpoint → advertise regardless of egress (a verifier dialed it back).
        assert_eq!(upnp_beacon_candidate(Some(upnp), Some(upnp), Some(egress_tunnel), true), Some(upnp));
        // AutoNAT confirmed a DIFFERENT endpoint → withhold (stale/phantom mapping).
        assert_eq!(upnp_beacon_candidate(Some(upnp), Some(other), None, true), None);
        // No verdict + direct allowed + egress matches the router WAN → optimistic advertise (the
        // relay-less first-contact fix; LIVE-confirmed A↔B).
        assert_eq!(upnp_beacon_candidate(Some(upnp), None, Some(egress_match), true), Some(upnp));
        // No verdict + egress UNKNOWN (STUN failed) → fail-open, advertise.
        assert_eq!(upnp_beacon_candidate(Some(upnp), None, None, true), Some(upnp));
        // No verdict + egress is a TUNNEL/double-NAT (reflexive IP != router WAN) → withhold the phantom.
        assert_eq!(upnp_beacon_candidate(Some(upnp), None, Some(egress_tunnel), true), None);
        // No verdict + RelayOnly → never advertise a direct endpoint (egress irrelevant).
        assert_eq!(upnp_beacon_candidate(Some(upnp), None, Some(egress_match), false), None);
    }

    #[test]
    fn self_beacon_seals_and_opens_under_the_shared_secret() {
        // The core seal/topic glue: build a self-beacon, seal it for the current blinded
        // topics under the shared pair secret, then open it back with the SAME secret (both
        // peers derive the identical secret — see step 1). The helper never sees the secret.
        let secret = [7u8; 32];
        let beacon =
            build_self_beacon([5u8; 20], vec!["203.0.113.7:51820".to_string()], NatType::Cone, None);
        let sealed = seal_beacon_under_secret(&secret, &beacon);
        assert!(!sealed.is_empty(), "produced at least the current topic");
        assert_eq!(sealed[0].0.len(), 64, "topic id is 32-byte hex");

        let blobs: Vec<Vec<u8>> = sealed.into_iter().map(|(_, _, wire)| wire).collect();
        let opened = open_beacon_under_secret(&secret, &blobs, &[])
            .expect("opens under the same secret");
        assert_eq!(opened.reflexive, vec!["203.0.113.7:51820".to_string()]);
        assert_eq!(opened.nat_type, NatType::Cone);

        // A different secret can't open it (AEAD forgery / wrong blinded topic).
        assert!(
            open_beacon_under_secret(&[9u8; 32], &blobs, &[]).is_none(),
            "a non-contact secret yields nothing"
        );
    }

    /// The helper's per-courier report becomes a status per courier: answering (with or without a
    /// beacon for us), a spent allowance, cooling down, failing — with the courier's own reason.
    #[test]
    fn a_courier_report_becomes_a_status_per_courier() {
        let statuses = super::courier_statuses(
            "ntfy:ntfy.sh=quota+30ms:ntfy_publish:_status_429 nostr:nos.lol=ok+300ms dht:mainline=empty+4000ms nostr:nostr.mom=skip nostr:x=fail+5ms:connect_refused",
            7,
        );
        let state = |label: &str| statuses.iter().find(|(l, _)| l == label).map(|(_, s)| s.state.as_str());
        assert_eq!(state("ntfy:ntfy.sh"), Some("quota"));
        assert_eq!(state("nostr:nos.lol"), Some("ok"));
        assert_eq!(state("dht:mainline"), Some("ok"), "nothing for us is still an answer");
        assert_eq!(state("nostr:nostr.mom"), Some("cooling"));
        assert_eq!(state("nostr:x"), Some("fail"));
        let ntfy = &statuses.iter().find(|(l, _)| l == "ntfy:ntfy.sh").unwrap().1;
        assert_eq!(ntfy.detail, "ntfy publish: status 429");
    }

    /// A courier that remembers what it posted, like the production helper.
    #[derive(Default)]
    struct LoggedCourier {
        inner: InMemoryCourier,
        log: super::BeaconPublishLog,
    }

    impl RendezvousCourier for LoggedCourier {
        fn publish(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, _ttl_secs: u32, _priority: PublishPriority) -> OmniResult<()> {
            self.inner.publish(topic_hex, dhtkey_hex, blob_hex, _ttl_secs, _priority)
        }
        fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, _legs: CourierLegs) -> OmniResult<Vec<String>> {
            self.inner.fetch(topic_hex, dhtkey_hex, _legs)
        }
        fn beacon_publish_log(&self) -> Option<&super::BeaconPublishLog> {
            Some(&self.log)
        }
    }

    impl LoggedCourier {
        fn posts(&self) -> usize {
            self.inner.topics.lock().unwrap().values().map(Vec::len).sum()
        }
    }

    /// Sweeps within one topic window post the beacon once; a new address goes out at once; the
    /// contact finds the beacon all along. The window turn itself is the pure rule below.
    #[test]
    fn an_unchanged_pair_beacon_is_posted_once_per_window_and_a_new_address_at_once() {
        let secret = [12u8; 32];
        let courier = LoggedCourier::default();
        let beacon = build_self_beacon([3u8; 20], vec!["198.51.100.9:7777".to_string()], NatType::Cone, None);

        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 2, "new: both topics");
        let posts = courier.posts();
        let later = build_self_beacon([3u8; 20], vec!["198.51.100.9:7777".to_string()], NatType::Cone, None);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &later).unwrap(), 0);
        assert_eq!(courier.posts(), posts, "the next sweep in the same window posts nothing");
        assert!(fetch_beacon_via(&courier, &secret, &[]).unwrap().is_some(), "and the contact still finds it");

        let moved = build_self_beacon([3u8; 20], vec!["203.0.113.4:4444".to_string()], NatType::Cone, None);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &moved).unwrap(), 2, "a new address goes at once");
        let found = fetch_beacon_via(&courier, &secret, &[]).unwrap().unwrap();
        assert_eq!(found.reflexive, vec!["203.0.113.4:4444".to_string()]);

        // Another device of ours on the same topic keeps its own record.
        let other = build_self_beacon([4u8; 20], vec!["198.51.100.10:7777".to_string()], NatType::Cone, None);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &other).unwrap(), 2);

        // A pair beacon outlives the window it was posted in, by an hour.
        assert_eq!(beacon.ttl_secs as u64, omni_rendezvous::TOPIC_WINDOW_SECS + 3600);
    }

    /// A courier that records how many courier kinds each beacon post asked for.
    #[derive(Default)]
    struct ReachRecordingCourier {
        inner: LoggedCourier,
        kinds: Mutex<Vec<u8>>,
    }

    impl RendezvousCourier for ReachRecordingCourier {
        fn publish(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, ttl_secs: u32, priority: PublishPriority) -> OmniResult<()> {
            self.inner.publish(topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority)
        }
        fn publish_routed(
            &self,
            topic_hex: &str,
            dhtkey_hex: &str,
            blob_hex: &str,
            _slim_hex: Option<&str>,
            ttl_secs: u32,
            route: &super::PublishRoute,
        ) -> OmniResult<()> {
            self.kinds.lock().unwrap().push(route.reach);
            self.inner.publish(topic_hex, dhtkey_hex, blob_hex, ttl_secs, route.priority)
        }
        fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>> {
            self.inner.fetch(topic_hex, dhtkey_hex, legs)
        }
        fn beacon_publish_log(&self) -> Option<&super::BeaconPublishLog> {
            Some(&self.inner.log)
        }
    }

    /// A pair beacon goes to one courier kind; while the pair's device is up (its beacon is there) but
    /// does not come, it goes to one more kind every 10 minutes, at most all three — and is posted again
    /// for that, though it says nothing new. A device that is off (no beacon of its) widens nothing.
    #[test]
    fn a_pair_beacon_reaches_more_courier_kinds_while_the_device_is_up_but_does_not_come() {
        let secret = [21u8; 32];
        let node = [8u8; 20];
        let courier = ReachRecordingCourier::default();
        let beacon = build_self_beacon(node, vec!["198.51.100.9:7777".to_string()], NatType::Cone, None);
        let kinds = || courier.kinds.lock().unwrap().clone();

        super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap();
        assert!(kinds().iter().all(|k| *k == 1), "one kind at first: {:?}", kinds());
        let posts = kinds().len();
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 0, "nothing new, no wider");

        let log = &courier.inner.log;
        // The start of the current window: every step below stays in it, as the posts do.
        let window_ms = omni_rendezvous::TOPIC_WINDOW_SECS * 1000;
        let t0 = current_unix_ms() as u64 / window_ms * window_ms;
        let minute = 60_000;
        // Seen once, then linked for a long while, then seen again: not one stretch — no wider.
        log.note_peer_seen_unlinked(&secret, node, t0);
        log.note_peer_seen_unlinked(&secret, node, t0 + 30 * minute);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 0, "a gap starts the clock again");

        // Seen sweep after sweep (every 2 minutes) for 10 minutes: one kind more.
        let start = t0 + 30 * minute;
        for step in 1..5 {
            log.note_peer_seen_unlinked(&secret, node, start + step * 2 * minute);
        }
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 0, "8 minutes is not yet");
        log.note_peer_seen_unlinked(&secret, node, start + super::PAIR_REACH_WIDEN_EVERY_MS);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 1, "wider: the current window again");
        assert_eq!(kinds()[posts..], [2]);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 0, "posted that wide already");

        for step in 6..60 {
            log.note_peer_seen_unlinked(&secret, node, start + step * 2 * minute);
        }
        super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap();
        assert_eq!(*kinds().last().unwrap(), super::COURIER_KIND_COUNT, "at most every kind");
    }

    /// A beacon that no courier returns any more (a courier restarted, a DHT item expired) is posted
    /// again on the next sweep — it lives half a day now, and would otherwise stay lost until the
    /// window turns. One that went out a moment ago is not taken for lost (a fetch racing the post).
    #[test]
    fn a_lost_beacon_is_posted_again_but_not_one_posted_a_moment_ago() {
        let secret = [13u8; 32];
        let courier = LoggedCourier::default();
        let beacon = build_self_beacon([6u8; 20], vec!["198.51.100.9:7777".to_string()], NatType::Cone, None);
        super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap();
        let (blobs, _) = super::fetch_beacon_blobs_via(&courier, &secret, super::CourierLegs::All).unwrap();
        assert!(super::beacon_of_node_among(&secret, &blobs, [6u8; 20]), "our beacon is there");
        assert!(!super::beacon_of_node_among(&secret, &blobs, [7u8; 20]), "nobody else's");

        courier.inner.topics.lock().unwrap().clear(); // the courier lost it
        let (blobs, _) = super::fetch_beacon_blobs_via(&courier, &secret, super::CourierLegs::All).unwrap();
        assert!(!super::beacon_of_node_among(&secret, &blobs, [6u8; 20]));
        let now = current_unix_ms() as u64;
        courier.log.note_own_beacon_missing(&secret, [6u8; 20], now);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 0, "just posted: not lost");

        courier.log.note_own_beacon_missing(&secret, [6u8; 20], now + super::LOST_BEACON_GRACE_MS);
        assert_eq!(super::publish_own_beacon_via(&courier, &secret, &beacon).unwrap(), 2, "lost: posted again");
        assert!(super::fetch_beacon_via(&courier, &secret, &[]).unwrap().is_some());
    }

    /// The courier is told how long a beacon is worth keeping — a Nostr relay turns it into a NIP-40
    /// `expiration` and stops serving the beacon after it.
    #[test]
    fn a_pair_beacon_tells_the_courier_how_long_to_keep_it() {
        #[derive(Default)]
        struct TtlCourier(Mutex<Vec<u32>>);
        impl RendezvousCourier for TtlCourier {
            fn publish(&self, _: &str, _: &str, _: &str, ttl_secs: u32, _priority: PublishPriority) -> OmniResult<()> {
                self.0.lock().unwrap().push(ttl_secs);
                Ok(())
            }
            fn fetch(&self, _: &str, _: &str, _: CourierLegs) -> OmniResult<Vec<String>> {
                Ok(Vec::new())
            }
        }
        let courier = TtlCourier::default();
        let beacon = build_self_beacon([8u8; 20], vec!["198.51.100.9:7777".to_string()], NatType::Cone, None);
        publish_beacon_via(&courier, &[14u8; 32], &beacon).unwrap();
        let told = courier.0.lock().unwrap().clone();
        assert!(!told.is_empty());
        assert!(told.iter().all(|ttl| *ttl == beacon.ttl_secs), "{told:?}");
    }

    /// The DHT is asked for a peer the web couriers did not have — at once when no web courier
    /// answers, otherwise at most every `DHT_READ_EVERY_MS` per pair.
    #[test]
    fn the_dht_is_asked_for_what_the_web_did_not_have_and_not_every_round() {
        struct WithReadLog(super::DhtReadLog);
        impl RendezvousCourier for WithReadLog {
            fn publish(&self, _: &str, _: &str, _: &str, _: u32, _priority: PublishPriority) -> OmniResult<()> {
                Ok(())
            }
            fn fetch(&self, _: &str, _: &str, _: super::CourierLegs) -> OmniResult<Vec<String>> {
                Ok(Vec::new())
            }
            fn dht_read_log(&self) -> Option<&super::DhtReadLog> {
                Some(&self.0)
            }
        }
        use super::{dht_read_due, DHT_READ_EVERY_MS};
        let courier = WithReadLog(super::DhtReadLog::default());
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert!(dht_read_due(&courier, &a, true, 1_000), "never asked: ask");
        assert!(!dht_read_due(&courier, &a, true, 2_000), "the web answers: not again so soon");
        assert!(dht_read_due(&courier, &b, true, 2_000), "another pair keeps its own pace");
        assert!(dht_read_due(&courier, &a, false, 3_000), "no web courier answers: ask every time");
        assert!(!dht_read_due(&courier, &a, true, 3_000 + DHT_READ_EVERY_MS - 1));
        assert!(dht_read_due(&courier, &a, true, 3_000 + DHT_READ_EVERY_MS));
    }

    #[test]
    fn a_pair_beacon_goes_to_the_new_window_only_when_it_says_the_same() {
        use super::{beacon_topics_due, BeaconPublishRecord};
        let posted = BeaconPublishRecord { bucket: 10, fingerprint: 1 };
        assert_eq!(beacon_topics_due(None, posted), Some(true), "never posted: both topics");
        assert_eq!(beacon_topics_due(Some(posted), posted), None, "same window, same content: nothing");
        assert_eq!(
            beacon_topics_due(Some(posted), BeaconPublishRecord { bucket: 11, fingerprint: 1 }),
            Some(false),
            "a new window: its topic only"
        );
        assert_eq!(
            beacon_topics_due(Some(posted), BeaconPublishRecord { bucket: 10, fingerprint: 2 }),
            Some(true),
            "new content: both topics at once"
        );
    }

    #[test]
    fn beacon_publishes_and_fetches_through_a_courier() {
        // The whole publish → fetch → open chain, locally (the part network-gated through
        // the real helper/providers). One contact publishes its beacon to the shared
        // courier; the other fetches + opens it under the SAME pair secret (both derive it
        // identically — step 1).
        let secret = [11u8; 32];
        let courier = InMemoryCourier::default();
        let beacon =
            build_self_beacon([2u8; 20], vec!["198.51.100.9:7777".to_string()], NatType::Public, None);

        let published = publish_beacon_via(&courier, &secret, &beacon).expect("publish");
        assert!(published >= 1, "published to at least the current topic");

        let fetched = fetch_beacon_via(&courier, &secret, &[])
            .expect("fetch ok")
            .expect("a beacon was found");
        assert_eq!(fetched.reflexive, vec!["198.51.100.9:7777".to_string()]);
        assert_eq!(fetched.nat_type, NatType::Public);

        // A stranger's secret recovers nothing (different blinded topics + AEAD).
        assert!(
            fetch_beacon_via(&courier, &[12u8; 32], &[])
                .expect("fetch ok")
                .is_none(),
            "a non-contact secret recovers nothing"
        );
    }

    #[test]
    fn rendezvous_connect_dials_the_peer_not_my_own_echo() {
        // Both contacts publish to the SAME blinded topic (symmetric secret), so a fetch
        // returns my own beacon too. rendezvous_connect must skip mine (by node) and dial
        // the PEER's endpoints. One shared in-memory courier stands in for the channel.
        let secret = [3u8; 32];
        let courier = InMemoryCourier::default();
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];

        // The peer has already published its beacon (different node + endpoint).
        let peer_beacon =
            build_self_beacon(peer_node, vec!["198.51.100.50:9000".to_string()], NatType::Cone, None);
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");

        // I connect: publish mine, fetch, skip my own echo, dial the peer.
        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Cone, None);
        let mut dialed: Vec<([u8; 20], Vec<std::net::SocketAddr>)> = Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |node, eps, _tcp| {
                dialed.push((node, eps));
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");

        assert_eq!(
            outcome,
            RendezvousOutcome::Dialed {
                node: peer_node,
                endpoints: vec!["198.51.100.50:9000".to_string()],
                relay_bundle: None,
            },
            "dialed the PEER's node + endpoint, not my own echo"
        );
        assert_eq!(dialed.len(), 1, "dialed exactly once");
        assert_eq!(dialed[0].0, peer_node);
        assert_eq!(
            dialed[0].1,
            vec!["198.51.100.50:9000".parse::<std::net::SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn on_the_accounts_own_topic_the_device_not_met_yet_is_dialed() {
        // Several devices of one account publish to its own topic. A device already linked is
        // passed over, even with the freshest beacon: the one to dial is the one not met yet.
        let secret = [5u8; 32];
        let courier = InMemoryCourier::default();
        let my_node = [0xA1u8; 20];
        let known_node = [0xA2u8; 20];
        let new_node = [0xA3u8; 20];
        let new_beacon =
            build_self_beacon(new_node, vec!["198.51.100.53:9000".to_string()], NatType::Cone, None);
        publish_beacon_via(&courier, &secret, &new_beacon).expect("new device publishes");
        let mut known_beacon =
            build_self_beacon(known_node, vec!["198.51.100.52:9000".to_string()], NatType::Cone, None);
        known_beacon.ts_ms = new_beacon.ts_ms + 1_000;
        publish_beacon_via(&courier, &secret, &known_beacon).expect("known device publishes");

        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Cone, None);
        let mut dialed = Vec::new();
        rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[known_node],
            |node, _eps, _tcp| {
                dialed.push(node);
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert_eq!(dialed, vec![new_node]);
    }

    #[test]
    fn rendezvous_connect_skips_the_wasted_dial_for_a_symmetric_pair() {
        // Phase C: BOTH ends symmetric-NAT → the reflexive endpoint isn't inbound-
        // dialable and punch can't help (documented hole). rendezvous_connect must
        // NOT dial the peer's reflexive candidate (pure waste) and must report the
        // peer's NAT class through the callback (so the state can be honest).
        let secret = [4u8; 32];
        let courier = InMemoryCourier::default();
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];

        // Peer is symmetric with a (undialable) reflexive endpoint, no TCP-direct.
        let peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.50:9000".to_string()],
            NatType::Symmetric,
            None,
        );
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");

        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Symmetric, None);
        let mut dialed: Vec<[u8; 20]> = Vec::new();
        let mut observed_peer_nat: Option<NatType> = None;
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |node, _eps, _tcp| {
                dialed.push(node);
                Ok(())
            },
            |_| {},
            true,
            |node, nat, _| {
                assert_eq!(node, peer_node);
                observed_peer_nat = Some(nat);
            },
        )
        .expect("connect");

        // No dial (skipped) → no usable endpoints → NoEndpoints, and the peer's NAT
        // class was surfaced to the caller.
        assert!(dialed.is_empty(), "symmetric×symmetric must not waste a direct dial");
        assert_eq!(outcome, RendezvousOutcome::NoEndpoints);
        assert_eq!(observed_peer_nat, Some(NatType::Symmetric));
    }

    /// The live case of 2026-09-24. Both ends symmetric, the peer advertises a forward AND a STUN
    /// address: the forward must be dialed, the STUN address must not. The negative half is the
    /// test above (a symmetric peer with ONLY a STUN address → no dial at all) — together they pin
    /// that the rule drops what it should and nothing more.
    #[test]
    fn a_forward_is_still_dialed_when_both_sides_are_symmetric() {
        let secret = [6u8; 32];
        let courier = InMemoryCourier::default();
        let my_node = [0xA1u8; 20];
        let peer_node = [0xB1u8; 20];

        let mut peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.70:61001".to_string()],
            NatType::Symmetric,
            None,
        );
        peer_beacon.forwarded = vec!["198.51.100.70:40123".to_string()];
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");

        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.3:3333".to_string()], NatType::Symmetric, None);
        let mut dialed: Vec<Vec<std::net::SocketAddr>> = Vec::new();
        let mut peer_reachable_anyway = None;
        rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, udp, _| {
                dialed.push(udp);
                Ok(())
            },
            |_| {},
            true,
            |_, _, nat_independent| peer_reachable_anyway = Some(nat_independent),
        )
        .expect("connect");

        assert_eq!(
            dialed,
            vec![vec!["198.51.100.70:40123".parse::<std::net::SocketAddr>().unwrap()]],
            "exactly the forward — not the STUN address, and not nothing"
        );
        assert_eq!(
            peer_reachable_anyway,
            Some(true),
            "the peer's forward is reported, so the connection state won't claim 'no route'"
        );
    }

    /// Forwards go first even when nothing is dropped: the dialer starts the endpoints in list order,
    /// so the order decides which of several live ones wins (and a dead STUN address ahead of a
    /// forward would cost the attempt delay before the forward is even tried).
    #[test]
    fn forwards_are_dialed_before_the_stun_address() {
        let secret = [7u8; 32];
        let courier = InMemoryCourier::default();
        let peer_node = [0xB2u8; 20];
        let mut peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.71:61002".to_string()],
            NatType::Cone,
            None,
        );
        peer_beacon.forwarded = vec!["198.51.100.71:40124".to_string()];
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");
        let my_beacon =
            build_self_beacon([0xA2u8; 20], vec!["203.0.113.4:4444".to_string()], NatType::Cone, None);
        let mut dialed: Vec<std::net::SocketAddr> = Vec::new();
        rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, udp, _| {
                dialed = udp;
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert_eq!(
            dialed,
            vec![
                "198.51.100.71:40124".parse::<std::net::SocketAddr>().unwrap(),
                "198.51.100.71:61002".parse::<std::net::SocketAddr>().unwrap(),
            ]
        );
    }

    /// One subscription serves many targets: a contact's fresh beacon wakes that contact, a beacon of a
    /// device of ours we have not met wakes the own-account meeting; our own beacon, a linked own
    /// device, a third party under a contact's secret and nothing at all wake nothing.
    #[test]
    fn a_shared_watch_wakes_each_target_by_its_own_beacon_only() {
        use crate::{beacon_wakes, BeaconWake, BeaconWatchTarget};
        use omni_rendezvous::rendezvous_topics;
        let (pair_secret, account_secret) = ([11u8; 32], [12u8; 32]);
        let (me, peer, linked_own, new_own) = ([0xA1u8; 20], [0xB2u8; 20], [0xC3u8; 20], [0xD4u8; 20]);
        let sealed = |secret: [u8; 32], node: [u8; 20]| {
            let beacon = build_self_beacon(node, vec!["198.51.100.1:1".to_string()], NatType::Cone, None);
            let topic = rendezvous_topics(&secret, current_unix_ms() as u64)[0];
            seal_beacon(&secret, &topic, &beacon).to_wire()
        };
        let targets = vec![
            BeaconWatchTarget { secret: pair_secret, node: Some(peer), exclude: Vec::new(), read_from: None },
            BeaconWatchTarget { secret: account_secret, node: None, exclude: vec![linked_own], read_from: None },
        ];
        assert!(beacon_wakes(&targets, me, &[]).is_empty());
        let quiet = [sealed(pair_secret, me), sealed(pair_secret, [0xE5; 20]), sealed(account_secret, me), sealed(account_secret, linked_own)];
        assert!(beacon_wakes(&targets, me, &quiet).is_empty(), "nothing here is news");
        assert_eq!(beacon_wakes(&targets, me, &[sealed(pair_secret, peer)]), vec![BeaconWake::Contact(peer)]);
        assert_eq!(beacon_wakes(&targets, me, &[sealed(account_secret, new_own)]), vec![BeaconWake::OwnAccount]);
        let mut both = quiet.to_vec();
        both.push(sealed(account_secret, new_own));
        both.push(sealed(pair_secret, peer));
        assert_eq!(
            beacon_wakes(&targets, me, &both),
            vec![BeaconWake::Contact(peer), BeaconWake::OwnAccount]
        );
    }

    /// A courier as slow as BT-DHT (every operation waits for it) must cost ONE operation before the
    /// dial, not four. Live 2026-10-03 with DHT on: publish topic 1, publish topic 2, fetch topic 1,
    /// fetch topic 2, one after another — ~28 s before every dial.
    #[test]
    fn a_slow_courier_costs_one_operation_before_the_dial_not_four() {
        struct SlowCourier {
            inner: InMemoryCourier,
            delay: std::time::Duration,
        }
        impl RendezvousCourier for SlowCourier {
            fn publish(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, _ttl_secs: u32, _priority: PublishPriority) -> OmniResult<()> {
                std::thread::sleep(self.delay);
                self.inner.publish(topic_hex, dhtkey_hex, blob_hex, _ttl_secs, _priority)
            }
            fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, _legs: CourierLegs) -> OmniResult<Vec<String>> {
                std::thread::sleep(self.delay);
                self.inner.fetch(topic_hex, dhtkey_hex, _legs)
            }
        }
        let delay = std::time::Duration::from_millis(400);
        let secret = [9u8; 32];
        let courier = SlowCourier { inner: InMemoryCourier::default(), delay };
        let my_beacon =
            build_self_beacon([0xA9u8; 20], vec!["203.0.113.9:4444".to_string()], NatType::Cone, None);

        // Negative half first: no peer beacon yet → no dial, however the timing goes; and my own
        // beacon is out when the call returns (the scope waits for the publish).
        let started = std::time::Instant::now();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| panic!("nothing to dial"),
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert_eq!(outcome, RendezvousOutcome::NoPeerBeacon);
        assert!(
            started.elapsed() < delay * 3,
            "publish and fetch overlap, and the topics too: {:?}",
            started.elapsed()
        );

        // The peer shows up.
        let peer_beacon =
            build_self_beacon([0xB9u8; 20], vec!["198.51.100.9:61002".to_string()], NatType::Cone, None);
        publish_beacon_via(&courier.inner, &secret, &peer_beacon).expect("peer publishes");
        let started = std::time::Instant::now();
        let mut dialed_after = None;
        rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| {
                dialed_after = Some(started.elapsed());
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        let dialed_after = dialed_after.expect("the peer is dialed");
        assert!(
            dialed_after < delay * 2,
            "one courier operation before the dial, not four: {dialed_after:?}"
        );
    }

    #[test]
    fn rendezvous_connect_still_dials_when_only_one_side_is_symmetric() {
        // A symmetric initiator dialing a CONE peer's reflexive endpoint is the
        // normal outbound path (symmetric NAT doesn't block outbound) — do NOT skip.
        let secret = [5u8; 32];
        let courier = InMemoryCourier::default();
        let peer_node = [0xCCu8; 20];
        let peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.60:9100".to_string()],
            NatType::Cone,
            None,
        );
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");
        let my_beacon =
            build_self_beacon([0xDDu8; 20], vec!["203.0.113.2:2222".to_string()], NatType::Symmetric, None);
        let mut dialed = false;
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_node, eps, _tcp| {
                dialed = !eps.is_empty();
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert!(dialed, "symmetric→cone must still dial out (outbound is unaffected)");
        assert!(matches!(outcome, RendezvousOutcome::Dialed { .. }));
    }

    /// While our own Mainline is blocked (a DHT leg over a friend or the pkarr relays is in use), our
    /// own engine is not asked: it would answer empty after ~4.3 s on every fetch (public Wi-Fi,
    /// 2026-10-14). With no such leg it is asked as before.
    #[test]
    fn a_blocked_own_mainline_is_not_read_while_another_way_to_the_dht_is_in_use() {
        struct LegRecordingCourier {
            inner: InMemoryCourier,
            dht_reads: Mutex<usize>,
        }
        impl RendezvousCourier for LegRecordingCourier {
            fn publish(&self, topic_hex: &str, dhtkey_hex: &str, blob_hex: &str, ttl_secs: u32, priority: PublishPriority) -> OmniResult<()> {
                self.inner.publish(topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority)
            }
            fn fetch(&self, topic_hex: &str, dhtkey_hex: &str, legs: CourierLegs) -> OmniResult<Vec<String>> {
                if legs == CourierLegs::Dht {
                    *self.dht_reads.lock().unwrap() += 1;
                    return Ok(Vec::new());
                }
                self.inner.fetch(topic_hex, dhtkey_hex, legs)
            }
        }
        let secret = [41u8; 32];
        let courier = LegRecordingCourier { inner: InMemoryCourier::default(), dht_reads: Mutex::new(0) };
        let my_beacon = build_self_beacon([0x41u8; 20], vec!["203.0.113.41:4141".to_string()], NatType::Cone, None);
        let connect = |leg: Option<&dyn DhtOverPeerLeg>| {
            rendezvous_connect(&courier, leg, &secret, &my_beacon, &[], |_, _, _| Ok(()), |_| {}, true, |_, _, _| {})
                .expect("connect")
        };

        let leg = MockDhtLeg::default();
        assert_eq!(connect(Some(&leg)), RendezvousOutcome::NoPeerBeacon);
        assert_eq!(*courier.dht_reads.lock().unwrap(), 0, "the blocked engine is not asked");

        assert_eq!(connect(None), RendezvousOutcome::NoPeerBeacon);
        assert!(*courier.dht_reads.lock().unwrap() > 0, "a working engine is asked as before (both topic windows)");
    }

    /// In-memory `DhtOverPeerLeg` double: a `(dht_key, salt) -> value` store + the shared claimer, so
    /// it exercises the SAME dual-slot semantics as the real mesh leg with no network.
    #[derive(Default)]
    struct MockDhtLeg {
        #[allow(clippy::type_complexity)] // test mock: (key, salt) → value
        store: Mutex<HashMap<([u8; 32], Vec<u8>), Vec<u8>>>,
        claimer: DhtSlotClaimer,
    }

    impl MockDhtLeg {
        /// Seed a value directly into slot 0 — simulates the PEER publishing from its OWN leg instance
        /// (a separate claimer), so our connect's publish claims the other slot instead of clobbering.
        fn seed_slot0(&self, dht_key: [u8; 32], value: Vec<u8>) {
            self.store.lock().unwrap().insert((dht_key, DHT_SLOT_SALTS[0].to_vec()), value);
        }
    }

    impl DhtOverPeerLeg for MockDhtLeg {
        fn publish(&self, dht_key: &[u8; 32], sealed: &[u8]) {
            let slot = self
                .claimer
                .claim(dht_key, |salt| {
                    Ok::<_, ()>(self.store.lock().unwrap().contains_key(&(*dht_key, salt.to_vec())))
                })
                .unwrap();
            self.store
                .lock()
                .unwrap()
                .insert((*dht_key, DHT_SLOT_SALTS[slot].to_vec()), sealed.to_vec());
        }

        fn fetch(&self, dht_key: &[u8; 32]) -> Vec<Vec<u8>> {
            let store = self.store.lock().unwrap();
            DHT_SLOT_SALTS
                .iter()
                .filter_map(|salt| store.get(&(*dht_key, salt.to_vec())).cloned())
                .collect()
        }
    }

    /// R7 2e-3: the peer is found + dialed via the DHT-over-peer leg even when the courier (web +
    /// local-DHT legs) returns NOTHING — proving the mode-b leg participates in N-of-many. See
    /// docs/decisions/dht-over-peer.md.
    #[test]
    fn rendezvous_connect_finds_the_peer_via_the_dht_over_peer_leg() {
        let secret = [0x5Au8; 32];
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];

        // The peer published its beacon ONLY through the DHT (mode b), into slot 0 of every bucket key.
        let leg = MockDhtLeg::default();
        let peer_beacon =
            build_self_beacon(peer_node, vec!["198.51.100.77:9000".to_string()], NatType::Cone, None);
        for (topic, dht_key) in rendezvous_topics_with_keys(&secret, current_unix_ms() as u64) {
            leg.seed_slot0(dht_key, seal_beacon(&secret, &topic, &peer_beacon).to_wire());
        }

        // The courier sees nothing (the peer never published there) — only the over-peer leg has it.
        let courier = InMemoryCourier::default();
        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Cone, None);
        let mut dialed: Vec<[u8; 20]> = Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            Some(&leg),
            &secret,
            &my_beacon,
            &[],
            |node, _eps, _tcp| {
                dialed.push(node);
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");

        assert!(
            matches!(outcome, RendezvousOutcome::Dialed { node, .. } if node == peer_node),
            "found + dialed the peer via the DHT-over-peer leg: {outcome:?}"
        );
        assert_eq!(dialed, vec![peer_node], "dialed exactly the peer the over-peer leg supplied");
    }

    /// R7 2e-4: the DHT-proxy responder caps serves per origin — admits up to the budget, then refuses
    /// within the window, doesn't penalise other origins, and recovers once the window slides. See
    /// docs/decisions/dht-over-peer.md (masterplan §4 minimal-harm).
    #[test]
    fn dht_proxy_origin_cap_admits_then_blocks_then_recovers() {
        // Unique origins so the node-global log can't collide with another test.
        let origin = "cap-test-origin-A";
        let other = "cap-test-origin-B";
        let t0 = 5_000_000u64;

        for i in 0..DHT_PROXY_MAX_SERVES_PER_WINDOW {
            assert!(dht_proxy_admit_origin(origin, t0), "admit #{i} within budget");
        }
        assert!(!dht_proxy_admit_origin(origin, t0), "over budget within the window → refused");
        assert!(dht_proxy_admit_origin(other, t0), "a different origin is unaffected");
        assert!(
            dht_proxy_admit_origin(origin, t0 + DHT_PROXY_WINDOW_MS + 1),
            "after the window slides the origin is admitted again"
        );
    }

    #[test]
    fn fixed_port_tcp_direct_appends_reflexive_endpoint_without_upnp() {
        use super::append_fixed_port_tcp_direct;
        use std::net::{IpAddr, Ipv4Addr};
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20));
        // No UPnP candidate + pinned fixed port + known egress IP → advertise reflexive:port directly
        // (the manual-forward relay-less door — no UPnP dependency).
        assert_eq!(
            append_fixed_port_tcp_direct(vec![], Some(48555), Some(ip), true),
            vec!["198.51.100.20:48555".to_string()]
        );
        // RelayOnly (no direct) → never advertise a direct endpoint.
        assert!(append_fixed_port_tcp_direct(vec![], Some(48555), Some(ip), false).is_empty());
        // No known egress IP → nothing to anchor the fixed port to.
        assert!(append_fixed_port_tcp_direct(vec![], Some(48555), None, true).is_empty());
        // Unset / 0 port → no-op (ephemeral/UPnP mode).
        assert!(append_fixed_port_tcp_direct(vec![], Some(0), Some(ip), true).is_empty());
        assert!(append_fixed_port_tcp_direct(vec![], None, Some(ip), true).is_empty());
        // Dedup: a UPnP-derived candidate already equal to reflexive:port is not duplicated.
        let base = vec!["198.51.100.20:48555".to_string()];
        assert_eq!(
            append_fixed_port_tcp_direct(base.clone(), Some(48555), Some(ip), true),
            base
        );
    }

    #[test]
    fn rendezvous_connect_threads_tcp_direct_front_doors_to_the_dialer() {
        // Task 3 — a peer's TCP-direct front doors (beacon `tcp_direct`) are parsed and handed to the
        // dialer alongside its UDP reflexive endpoints, AND a peer reachable ONLY over TCP-direct (no
        // UDP reflexive — a UDP-hostile carrier) is still dialed, not reported NoEndpoints. See
        // docs/decisions/tcp-direct-peer-fallback.md.
        let secret = [33u8; 32];
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];
        let my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Cone, None);

        // (1) peer advertises BOTH carriers → the dialer receives both.
        let courier = InMemoryCourier::default();
        let mut peer_beacon =
            build_self_beacon(peer_node, vec!["198.51.100.50:9000".to_string()], NatType::Cone, None);
        peer_beacon.tcp_direct = vec!["198.51.100.50:8443".to_string()];
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");
        let mut dialed: Vec<([u8; 20], Vec<std::net::SocketAddr>, Vec<std::net::SocketAddr>)> =
            Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |node, udp, tcp| {
                dialed.push((node, udp, tcp));
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert_eq!(dialed.len(), 1);
        assert_eq!(dialed[0].1, vec!["198.51.100.50:9000".parse::<std::net::SocketAddr>().unwrap()]);
        assert_eq!(
            dialed[0].2,
            vec!["198.51.100.50:8443".parse::<std::net::SocketAddr>().unwrap()],
            "the TCP-direct front door reached the dialer"
        );
        assert!(
            matches!(&outcome, RendezvousOutcome::Dialed { endpoints, .. }
                if endpoints.contains(&"198.51.100.50:8443".to_string())),
            "the trace lists the TCP-direct endpoint"
        );

        // (2) peer reachable ONLY over TCP-direct (UDP-hostile) → still dialed, not NoEndpoints.
        let courier2 = InMemoryCourier::default();
        let mut tcp_only = build_self_beacon(peer_node, vec![], NatType::Symmetric, None);
        tcp_only.tcp_direct = vec!["198.51.100.50:8443".to_string()];
        publish_beacon_via(&courier2, &secret, &tcp_only).expect("peer publishes");
        let mut dialed2: Vec<Vec<std::net::SocketAddr>> = Vec::new();
        let outcome2 = rendezvous_connect(
            &courier2,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _udp, tcp| {
                dialed2.push(tcp);
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert!(matches!(outcome2, RendezvousOutcome::Dialed { .. }), "TCP-only peer is dialed");
        assert_eq!(dialed2, vec![vec!["198.51.100.50:8443".parse::<std::net::SocketAddr>().unwrap()]]);
    }

    /// A revocation mark rides the sealed pair beacon and reaches the reader with the opened beacon,
    /// whatever the dial does (multi-device-equal-nodes.md, «Отзыв через маячок»).
    #[test]
    fn a_revocation_mark_reaches_the_reader_with_the_peer_beacon_even_when_the_dial_fails() {
        let secret = [22u8; 32];
        let mut peer_beacon = build_self_beacon([0xBBu8; 20], vec!["198.51.100.50:9000".to_string()], NatType::Cone, None);
        peer_beacon.revoked_through_ms = Some(1_791_000_000_000);
        let my_beacon = build_self_beacon([0xAAu8; 20], Vec::new(), NatType::Cone, None);
        let courier = InMemoryCourier::default();
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");
        let mut marks: Vec<u64> = Vec::new();
        let _ = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| Err(crate::OmniError::Transport("dial failed".to_string())),
            |peer: &omni_rendezvous::Beacon| marks.extend(peer.revoked_through_ms),
            true,
            |_, _, _| {},
        );
        assert_eq!(marks, vec![1_791_000_000_000]);
    }

    /// The slim copy exists only for a beacon that does not fit a DHT record, and is the same beacon
    /// without its relay bundle.
    #[test]
    fn a_beacon_too_big_for_the_dht_gets_a_slim_copy_without_the_bundle() {
        let secret = [23u8; 32];
        let topic = omni_rendezvous::rendezvous_topics(&secret, super::current_unix_ms() as u64)[0];
        let mut beacon = build_self_beacon([0xBBu8; 20], vec!["198.51.100.50:9000".to_string()], omni_rendezvous::NatType::Cone, None);
        let wire = super::seal_beacon(&secret, &topic, &beacon).to_wire();
        assert!(super::slim_beacon_wire(&secret, &topic.id(), &beacon, &wire).is_none(), "fits: no copy");
        beacon.relay_bundle = Some("x".repeat(900)); // not a token: rides as text, too big for the DHT
        let wire = super::seal_beacon(&secret, &topic, &beacon).to_wire();
        assert!(wire.len() > omni_rendezvous::DHT_MAX_BLOB);
        let slim = super::slim_beacon_wire(&secret, &topic.id(), &beacon, &wire).expect("a slim copy");
        assert!(slim.len() <= omni_rendezvous::DHT_MAX_BLOB);
        let opened = super::open_beacon_under_secret(&secret, &[slim], &[]).expect("opens");
        assert_eq!((opened.relay_bundle, opened.reflexive), (None, beacon.reflexive));
    }

    #[test]
    fn rendezvous_connect_consumes_the_peer_relay_bundle_even_when_the_dial_fails() {
        // R1c inc.3b: the propagated relay bundle is consumed straight from the OPENED beacon, BEFORE
        // and INDEPENDENT of the dial — so a friend's relay is learned precisely when we can't reach
        // them directly (a failed dial must NOT drop the bundle). See r1c-rotatable-white-relay.md.
        let secret = [21u8; 32];
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];

        let mut peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.50:9000".to_string()],
            NatType::Cone,
            None,
        );
        peer_beacon.relay_bundle = Some("om3relay:opaque-token".to_string());
        let my_beacon = build_self_beacon(
            my_node,
            vec!["203.0.113.1:1111".to_string()],
            NatType::Cone,
            None,
        );

        // The dial FAILS (NAT / unreachable) — the bundle must still be consumed.
        let courier = InMemoryCourier::default();
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");
        let mut consumed: Vec<([u8; 20], String)> = Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| Err(crate::OmniError::Transport("dial failed: no reachable address".to_string())),
            |peer: &omni_rendezvous::Beacon| consumed.extend(peer.relay_bundle.iter().map(|token| (peer.node, token.clone()))),
            true,
            |_, _, _| {},
        );
        assert!(outcome.is_err(), "the dial error still propagates as the outcome");
        assert_eq!(
            consumed,
            vec![(peer_node, "om3relay:opaque-token".to_string())],
            "the bundle is consumed from the opened beacon even though the dial failed"
        );

        // And a bare beacon (no bundle) consumes nothing.
        let courier2 = InMemoryCourier::default();
        publish_beacon_via(
            &courier2,
            &secret,
            &build_self_beacon(peer_node, vec!["198.51.100.50:9000".to_string()], NatType::Cone, None),
        )
        .expect("peer publishes");
        let mut consumed2: Vec<([u8; 20], String)> = Vec::new();
        let _ = rendezvous_connect(
            &courier2,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| Ok(()),
            |peer: &omni_rendezvous::Beacon| consumed2.extend(peer.relay_bundle.iter().map(|token| (peer.node, token.clone()))),
            true,
            |_, _, _| {},
        )
        .expect("connect");
        assert!(consumed2.is_empty(), "a bare beacon consumes nothing");
    }

    #[test]
    fn invite_bootstrap_nonce_lets_both_sides_rendezvous() {
        // FIRST contact: there is no X3DH session yet, so the durable pair secret doesn't exist.
        // The inviter mints a nonce (carried in the invite); BOTH sides derive the SAME bootstrap
        // secret from it, meet on the same blinded bootstrap topic via the shared courier, and the
        // importer dials the inviter's published endpoint — the exact durable-rendezvous machinery,
        // keyed off the invite nonce instead of a pair secret. See invite-bootstrap-rendezvous.md.
        let nonce = [0x42u8; 32];
        let inviter_secret = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce).unwrap();
        let importer_secret = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce).unwrap();
        assert_eq!(inviter_secret, importer_secret, "both sides derive the same bootstrap secret");

        let courier = InMemoryCourier::default();
        let inviter_node = [0xA1u8; 20];
        let importer_node = [0xB2u8; 20];

        // Inviter has published its bootstrap beacon (it just wants to be findable).
        let inviter_beacon =
            build_self_beacon(inviter_node, vec!["198.51.100.7:5000".to_string()], NatType::Public, None);
        publish_beacon_via(&courier, &inviter_secret, &inviter_beacon).expect("inviter publishes");

        // Importer connects: publishes its own, finds the inviter (skipping its own echo), dials it.
        let importer_beacon =
            build_self_beacon(importer_node, vec!["203.0.113.9:6000".to_string()], NatType::Cone, None);
        let mut dialed: Vec<([u8; 20], Vec<std::net::SocketAddr>)> = Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &importer_secret,
            &importer_beacon,
            &[],
            |node, eps, _tcp| {
                dialed.push((node, eps));
                Ok(())
            },
            |_| {},
            true,
            |_, _, _| {},
        )
        .expect("bootstrap connect");

        assert_eq!(
            outcome,
            RendezvousOutcome::Dialed {
                node: inviter_node,
                endpoints: vec!["198.51.100.7:5000".to_string()],
                relay_bundle: None,
            },
            "importer dialed the inviter's bootstrap endpoint"
        );
        assert_eq!(dialed.len(), 1, "dialed exactly once");
    }

    #[test]
    fn rendezvous_connect_relay_only_strips_my_endpoints_and_skips_the_dial() {
        // follow-up d — RelayOnly (allow_direct=false): the master direct-upgrade gate OFF must
        // (a) NOT direct-dial the peer and (b) advertise NO direct endpoints in MY published beacon —
        // the beacon is sealed to the contact, so leaking my reflexive / UPnP / TCP-direct there would
        // hand the peer my IP (exactly what this mode hides). The peer's relay bundle (R1c) is still
        // consumed — privacy-independent. The relay path carries the data. See connection-privacy-modes.md.
        let secret = [44u8; 32];
        let courier = InMemoryCourier::default();
        let my_node = [0xAAu8; 20];
        let peer_node = [0xBBu8; 20];

        // An Auto peer publishes a beacon WITH endpoints + a relay bundle: a non-gated dial WOULD fire.
        let mut peer_beacon = build_self_beacon(
            peer_node,
            vec!["198.51.100.50:9000".to_string()],
            NatType::Cone,
            Some("om3relay:opaque-token".to_string()),
        );
        peer_beacon.tcp_direct = vec!["198.51.100.50:8443".to_string()];
        publish_beacon_via(&courier, &secret, &peer_beacon).expect("peer publishes");

        // MY beacon advertises endpoints too — they MUST be stripped before publish under RelayOnly.
        let mut my_beacon =
            build_self_beacon(my_node, vec!["203.0.113.1:1111".to_string()], NatType::Cone, None);
        my_beacon.tcp_direct = vec!["203.0.113.1:8443".to_string()];
        // A forward is the most identifying endpoint of all (stable, not per-destination).
        my_beacon.forwarded = vec!["203.0.113.1:40000".to_string()];

        let mut dialed = false;
        let mut consumed: Vec<([u8; 20], String)> = Vec::new();
        let outcome = rendezvous_connect(
            &courier,
            None,
            &secret,
            &my_beacon,
            &[],
            |_, _, _| {
                dialed = true;
                Ok(())
            },
            |peer: &omni_rendezvous::Beacon| consumed.extend(peer.relay_bundle.iter().map(|token| (peer.node, token.clone()))),
            false, // RelayOnly: direct upgrade disabled
            |_, _, _| {},
        )
        .expect("connect");

        assert!(!dialed, "RelayOnly must NOT direct-dial the peer");
        assert_eq!(
            consumed,
            vec![(peer_node, "om3relay:opaque-token".to_string())],
            "the peer's relay bundle is still consumed (R1c is privacy-independent)"
        );
        assert!(
            matches!(&outcome, RendezvousOutcome::Dialed { node, endpoints, .. }
                if *node == peer_node && endpoints.is_empty()),
            "outcome reports the peer found but no endpoints dialed"
        );

        // MY published beacon must carry NO direct endpoints (open it back, excluding the peer's node).
        let mine = fetch_beacon_via(&courier, &secret, &[peer_node])
            .expect("fetch ok")
            .expect("my own beacon is on the topic");
        assert_eq!(mine.node, my_node, "fetched my own beacon");
        assert!(mine.forwarded.is_empty(), "RelayOnly strips my forwards (UPnP, global IPv6)");
        assert!(mine.reflexive.is_empty(), "RelayOnly strips my UDP reflexive endpoints");
        assert!(mine.tcp_direct.is_empty(), "RelayOnly strips my TCP-direct front doors");
    }
}
