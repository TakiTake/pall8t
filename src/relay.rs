//! Host-side relay bridging a sandboxed agent to the host herdr session
//! (ADR-0007). `pall8t run` spawns it just before exec'ing into the
//! `container` client; it listens on a private Unix socket of its own
//! under `~/.pall8t/run/`, and forwards newline-delimited-JSON herdr
//! requests to the real `herdr.sock` after a per-request policy check
//! (see [`classify`]). That listening socket is then mounted into the
//! sandbox at `HERDR_SOCKET_PATH`, where the stock herdr CLI finds it —
//! apple/container forwards a mount whose source is a Unix socket into
//! the guest as a live socket rather than a filesystem (verified on
//! 1.2.2; see the ADR-0007 amendment for the corrected premise this
//! replaced).
//!
//! Lifecycle: the relay watches its parent — the pall8t process that
//! becomes the `container` client via exec (same pid) — and exits as soon
//! as it is reparented, so it lives exactly as long as the sandbox
//! session. A socket file it leaves behind is reaped by the next run
//! ([`stale_sockets`]).
//!
//! Security posture, relative to herdr's own 0600-socket model: the
//! listening socket lives in a 0700 directory, so only this user reaches
//! it — and this user can already reach `herdr.sock` itself. The socket
//! file is mode 0666 because the guest presents it with the host's own
//! mode and the sandboxed agent runs as `dev`, not root; that permission
//! applies inside the VM, where the only other principal is the agent
//! itself. Each run gets its own socket, mounted into that one container.
//! Method policy on top keeps the sandbox from administering the host
//! herdr installation itself; see [`classify`] for the split.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Relay-side view of [`crate::config::HerdrSandbox`]: `Off` never reaches
/// the relay (no relay is spawned at all), so only the two serving modes
/// exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Readonly,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "full" => Ok(Mode::Full),
            "readonly" => Ok(Mode::Readonly),
            other => Err(anyhow!("unknown relay mode {other:?}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Readonly => "readonly",
        }
    }
}

/// What a herdr API method does, for policy purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Pure inspection: allowed in every mode.
    Read,
    /// Mutates the terminal workspace (panes, tabs, prompts, input):
    /// allowed in `full`, denied in `readonly`. Methods this module
    /// doesn't recognize (added by a newer herdr) land here too — the
    /// transparent default for `full`, the safe default for `readonly`.
    Mutate,
    /// Administers the host herdr installation itself (stop/handoff the
    /// server, rewrite its config, install integrations/plugins into host
    /// agent configs): always denied. herdr's own agent skill already
    /// forbids agents doing these, so blocking them is a guardrail, not a
    /// blocker.
    HostAdmin,
}

/// Namespaces whose methods administer the host herdr installation or its
/// session/server lifecycle rather than the terminal workspace
/// (`server.stop`, `integration.install`, `plugin.link`, …): denied by
/// namespace, not by an enumerated list, so an admin method a FUTURE herdr
/// adds in these namespaces is denied before this module ever hears of it
/// (review finding on PR #38 — an enumerated denylist silently allowed
/// new admin methods in `full` mode). The read-only exceptions that do
/// live in these namespaces (`server.agent_manifests`, `plugin.list`,
/// `session.snapshot`, …) are carved out by the exact-match [`READ`]
/// check running first.
const ADMIN_NAMESPACES: &[&str] = &["server.", "integration.", "plugin.", "session."];

/// Pure-inspection methods (allowed even in `readonly`). A read method a
/// newer herdr adds is missing here until this list is refreshed — it is
/// then treated as [`Class::Mutate`], which only ever errs toward denying,
/// never toward leaking a mutation into `readonly`.
///
/// Last reconciled against herdr 0.8 (protocol 19). `scripts/herdr-method-drift.py`
/// reports what a newer herdr serves that this list doesn't classify as a
/// read — run it by hand, or read the weekly job's summary. Note that
/// `pane.graphics.stream` stays here deliberately: herdr serves and
/// documents it, but it hijacks the connection instead of answering in
/// the request/response shape, so it is absent from herdr's own schema
/// and the drift report flags it every time.
const READ: &[&str] = &[
    "ping",
    "agent.explain",
    "agent.get",
    "agent.list",
    "agent.read",
    "agent.wait",
    "events.subscribe",
    "events.wait",
    "layout.export",
    "pane.current",
    "pane.edges",
    "pane.get",
    "pane.graphics.info",
    "pane.graphics.stream",
    "pane.layout",
    "pane.list",
    "pane.neighbor",
    "pane.process_info",
    "pane.read",
    "pane.wait_for_output",
    "plugin.action.list",
    "plugin.list",
    "plugin.log.list",
    "server.agent_manifests",
    "session.snapshot",
    "tab.get",
    "tab.list",
    "workspace.get",
    "workspace.list",
    "worktree.list",
];

pub fn classify(method: &str) -> Class {
    if READ.contains(&method) {
        Class::Read
    } else if ADMIN_NAMESPACES.iter().any(|ns| method.starts_with(ns)) {
        Class::HostAdmin
    } else {
        Class::Mutate
    }
}

/// The one policy decision: may `method` cross the bridge in `mode`?
pub fn allowed(mode: Mode, method: &str) -> bool {
    match classify(method) {
        Class::Read => true,
        Class::Mutate => mode == Mode::Full,
        Class::HostAdmin => false,
    }
}

/// herdr-shaped error reply for a denied request, so the in-container CLI
/// renders a real error instead of hanging or crashing:
/// `{"id":…,"error":{"code":"sandbox_denied","message":…}}`.
fn deny_response(id: &str, method: &str, mode: Mode) -> String {
    let msg = format!(
        "pall8t blocked `{method}` from the sandbox ([herdr] sandbox = \
         \"{}\"; see the pall8t config to change this)",
        mode.as_str()
    );
    serde_json::json!({
        "id": id,
        "error": { "code": "sandbox_denied", "message": msg }
    })
    .to_string()
}

/// herdr's own cap on the initial request line (1 MB); a first line longer
/// than this is not a herdr request.
const MAX_REQUEST_LINE: u64 = 1024 * 1024;

/// Just the two fields the relay's policy check reads out of a request
/// line. Deserializing only these skips materializing `params` (up to
/// [`MAX_REQUEST_LINE`]: agent.prompt bodies, graphics payloads) per
/// connection. `Cow` borrows straight out of the request line for the
/// common unescaped case and only allocates for a value needing
/// unescaping, so this parses any id/method the whole-`Value` path did.
#[derive(serde::Deserialize)]
struct ReqHead<'a> {
    #[serde(default, borrow)]
    method: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    id: Option<std::borrow::Cow<'a, str>>,
}

/// The one directory the relay may create, chmod, and sweep: its own run
/// root. Compared as paths, not prefixes — a nested directory under the
/// root is still not the root, and there is no reason to accept one.
fn check_run_socket_dir(dir: &Path, root: &Path) -> Result<()> {
    if dir != root {
        anyhow::bail!(
            "the relay socket must live in {} (got {}); `pall8t herdr relay` is \
             spawned by `pall8t run`, not run by hand",
            root.display(),
            dir.display()
        );
    }
    Ok(())
}

/// macOS `sun_path` holds 104 bytes including the NUL, and the kernel
/// rejects a longer address at bind time — which is why the socket name
/// is derived under a budget rather than taken verbatim.
const SUN_PATH_MAX: usize = 103;

/// Socket file for one run, under `dir` (`~/.pall8t/run`). The container
/// name is the readable default; when the full path wouldn't fit
/// [`SUN_PATH_MAX`], the name is truncated and disambiguated with a hash
/// of the *whole* name, so two long names that share a prefix still get
/// distinct sockets. Returns `None` when even the hashed form can't fit
/// — a caller that deep in a nested home has no socket path to offer.
pub fn socket_path(dir: &Path, container: &str) -> Option<PathBuf> {
    let full = dir.join(format!("{container}.sock"));
    if full.as_os_str().len() <= SUN_PATH_MAX {
        return Some(full);
    }
    let hash = crate::container::sha256_hex_prefix(container.as_bytes(), 4);
    // Everything the path spends on something other than the truncated
    // head of the name: "<dir>" + "/" + "-<hash>.sock" (the NUL is
    // already out of SUN_PATH_MAX's budget).
    let fixed = dir.as_os_str().len() + 1 + 1 + hash.len() + ".sock".len();
    let budget = SUN_PATH_MAX.checked_sub(fixed)?;
    let head = truncate_bytes(container, budget);
    if head.is_empty() {
        return None;
    }
    Some(dir.join(format!("{head}-{hash}.sock")))
}

/// Longest prefix of `s` that fits in `budget` **bytes** without
/// splitting a character. The budget is a byte budget because that is
/// what `sun_path` counts; container names are ASCII slugs today, so this
/// only matters for a caller passing something else — which must still
/// get a bindable path, not one that is short in characters and too long
/// in bytes.
fn truncate_bytes(s: &str, budget: usize) -> &str {
    if s.len() <= budget {
        return s;
    }
    let end = (0..=budget)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    &s[..end]
}

/// Grace window mirroring [`crate::herdr`]'s per-run binary reaping: a
/// socket younger than this is left alone whatever the liveness probe
/// says, so a run whose relay is momentarily not accepting (a saturated
/// backlog, a stalled accept loop) can't have its socket pulled out from
/// under it by a run starting seconds later.
const SOCKET_REAP_GRACE: std::time::Duration = std::time::Duration::from_mins(5);

/// Which of `candidates` are leftovers from an exited run: a socket file
/// nothing is listening on any more, old enough to be past
/// [`SOCKET_REAP_GRACE`]. `is_live` answers "does connecting to this path
/// succeed?" — the caller passes the real connect ([`socket_is_live`]),
/// tests pass their own. A live run's socket is never reaped, so a
/// concurrent sandbox keeps working; an unknown age never reaps, erring
/// toward keeping (same rule as `should_reap_run_bin`).
fn stale_sockets(
    candidates: Vec<(PathBuf, Option<std::time::Duration>)>,
    grace: std::time::Duration,
    is_live: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    candidates
        .into_iter()
        .filter(|(path, age)| age.is_some_and(|a| a > grace) && !is_live(path))
        .map(|(path, _)| path)
        .collect()
}

/// Whether a connect attempt says the socket is still served.
///
/// Only two errors mean "nothing is listening": the socket answered with
/// `ECONNREFUSED` (bound file, no listener — an exited run), or it is
/// gone. Every other error means the probe itself failed — `EMFILE` under
/// fd pressure, a permission problem, a timeout — and says nothing about
/// the peer. Treating those as death would unlink a *live* sandbox's
/// bridge socket, cutting its herdr calls mid-session with nothing logged
/// on either side. Same rule as the unknown-age case: when the answer
/// isn't known, keep.
fn connect_says_dead(err: Option<std::io::ErrorKind>) -> bool {
    matches!(
        err,
        Some(std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
    )
}

fn socket_is_live(path: &Path) -> bool {
    !connect_says_dead(UnixStream::connect(path).err().map(|e| e.kind()))
}

/// Best-effort reaping of sockets left behind by exited runs. Failure is
/// silent by design: an unreapable leftover costs a stale file, never a
/// launch. `grace` is a parameter so a test can drive the real filesystem
/// walk without waiting out [`SOCKET_REAP_GRACE`].
fn reap_stale_sockets(dir: &Path, grace: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let candidates: Vec<(PathBuf, Option<std::time::Duration>)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .map(|e| {
            let age = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok());
            (e.path(), age)
        })
        .collect();
    for path in stale_sockets(candidates, grace, socket_is_live) {
        let _ = std::fs::remove_file(path);
    }
}

/// Append-only audit log: one JSON line per decision. Best-effort — the
/// relay must keep serving even if the log can't be written.
fn audit(log_path: &Path, entry: &serde_json::Value) {
    let line = format!("{entry}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Exits the process the moment the parent changes — i.e. the exec'd
/// `container` client (which kept pall8t's pid) has exited and the relay
/// was reparented. Polling getppid is the portable way to observe this
/// without a supervision protocol.
fn watch_parent() {
    // SAFETY: getppid cannot fail and has no preconditions.
    let original = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // SAFETY: as above.
        if unsafe { libc::getppid() } != original {
            std::process::exit(0);
        }
    });
}

/// Serving loop for `pall8t herdr relay` (hidden; spawned by `pall8t run`,
/// never by hand). Binds `listen`, prints that path as the single stdout
/// line the parent reads — the parent needs the socket to exist before it
/// can hand it to `container run` as a mount source — then serves until
/// the parent exits.
pub fn run(socket: &Path, listen: &Path, mode: Mode, log_path: &Path) -> Result<()> {
    let dir = listen
        .parent()
        .context("the relay socket path has no parent directory")?;
    // Everything below this line is destructive — it chmods the directory
    // to 0700 and unlinks sockets in it — so the directory is checked
    // before any of it runs. `pall8t run` always passes the run root; a
    // hand-run `--listen /tmp/x.sock` (the subcommand is hidden, not
    // blocked) would otherwise chmod /tmp.
    check_run_socket_dir(dir, &crate::config::pall8t_root()?.join("run"))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create the relay socket directory {}", dir.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Host-side access control for every run's socket: this user
        // only. The sockets inside are necessarily 0666 (see the module
        // doc), so the directory is the whole thing keeping other users
        // out — a failure here must stop the bridge (the caller warns and
        // runs without it), not serve a world-reachable relay.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "cannot restrict the relay socket directory {} to this user",
                    dir.display()
                )
            },
        )?;
    }
    reap_stale_sockets(dir, SOCKET_REAP_GRACE);
    // Our own path may still hold a leftover if a previous run died and
    // its name was reused; bind(2) fails on an existing file either way.
    let _ = std::fs::remove_file(listen);
    let listener = UnixListener::bind(listen)
        .with_context(|| format!("cannot bind the relay socket {}", listen.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        // The guest sees this socket with the host's own mode, and the
        // sandboxed agent runs as `dev` (uid 501), not root — 0600 would
        // be unreachable from inside. Verified on container 1.2.2.
        std::fs::set_permissions(listen, std::fs::Permissions::from_mode(0o666))
            .with_context(|| format!("cannot chmod the relay socket {}", listen.display()))?;
    }
    println!("{}", listen.display());
    // The socket-path line is the whole stdout contract; anything later
    // would land in a pipe nobody reads (the parent execs away).
    drop(std::io::stdout().flush());

    watch_parent();
    audit(
        log_path,
        &serde_json::json!({
            "ts": epoch_secs(), "event": "start",
            "listen": listen.display().to_string(),
            "mode": mode.as_str(),
            "socket": socket.display().to_string(),
        }),
    );
    serve(&listener, socket, mode, log_path);
    Ok(())
}

/// Accept loop, factored from [`run`] so tests can drive it on a listener
/// they control.
fn serve(listener: &UnixListener, socket: &Path, mode: Mode, log_path: &Path) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let socket = socket.to_path_buf();
        let log_path = log_path.to_path_buf();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &socket, mode, &log_path) {
                audit(
                    &log_path,
                    &serde_json::json!({
                        "ts": epoch_secs(), "event": "error",
                        "detail": format!("{e:#}"),
                    }),
                );
            }
        });
    }
}

/// One connection: read the first request line → policy → forward to the
/// herdr socket and pump bytes both ways until either side closes
/// (streaming methods like `events.subscribe` hold the connection).
fn handle(mut conn: UnixStream, socket: &Path, mode: Mode, log_path: &Path) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?).take(MAX_REQUEST_LINE);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("cannot read the request line")?;
    if line.trim().is_empty() {
        return Ok(());
    }

    // Deserialize only the two fields policy needs (see [`ReqHead`]), not
    // the whole request — `params` can be up to MAX_REQUEST_LINE.
    let head: Option<ReqHead> = serde_json::from_str(line.trim()).ok();
    let method = head
        .as_ref()
        .and_then(|h| h.method.as_deref())
        .unwrap_or("");
    let id = head.as_ref().and_then(|h| h.id.as_deref()).unwrap_or("");

    // An unparseable line has no method to check: in full mode it is
    // forwarded (herdr answers `invalid_request` itself — staying out of
    // the way is the point of transparency); in readonly it is denied,
    // since a request policy can't classify must not cross.
    let allow = if method.is_empty() {
        mode == Mode::Full
    } else {
        allowed(mode, method)
    };
    audit(
        log_path,
        &serde_json::json!({
            "ts": epoch_secs(), "event": if allow { "allow" } else { "deny" },
            "method": method,
        }),
    );
    if !allow {
        conn.write_all(deny_response(id, method, mode).as_bytes())?;
        conn.write_all(b"\n")?;
        return Ok(());
    }

    let mut upstream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to herdr socket {}", socket.display()))?;
    upstream.write_all(line.as_bytes())?;

    // Pump the rest of the conversation. This `into_inner` unwraps only
    // the `Take` cap and yields the `BufReader` itself, so bytes
    // `read_line` prefetched past the first newline stay buffered and are
    // forwarded first by the copy below. (`BufReader::into_inner()` —
    // which WOULD discard them and lose pipelined requests — is never
    // called; pinned by `relay_forwards_bytes_prefetched_past_the_first_line`.)
    let mut upstream_write = upstream.try_clone()?;
    let mut reader = reader.into_inner();
    let to_upstream = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream, &mut conn);
    let _ = conn.shutdown(std::net::Shutdown::Write);
    let _ = to_upstream.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_table() {
        assert_eq!(classify("pane.list"), Class::Read);
        assert_eq!(classify("agent.wait"), Class::Read);
        assert_eq!(classify("pane.split"), Class::Mutate);
        assert_eq!(classify("agent.prompt"), Class::Mutate);
        assert_eq!(
            classify("pane.run"),
            Class::Mutate,
            "not a real method, but the unknown default must be Mutate"
        );
        assert_eq!(classify("server.stop"), Class::HostAdmin);
        assert_eq!(classify("integration.install"), Class::HostAdmin);
        assert_eq!(classify("plugin.link"), Class::HostAdmin);
        assert_eq!(
            classify("server.shutdown"),
            Class::HostAdmin,
            "an admin method this module has never heard of is denied by \
             its namespace, not allowed by omission (PR #38 review finding)"
        );
        assert_eq!(
            classify("plugin.action.invoke"),
            Class::HostAdmin,
            "plugin actions run arbitrary host-side plugin code — admin \
             namespace, deliberately no carve-out"
        );
        assert_eq!(
            classify("session.snapshot"),
            Class::Read,
            "exact READ matches carve through the admin namespaces"
        );
        assert_eq!(classify("server.agent_manifests"), Class::Read);
        assert_eq!(
            classify("future.method"),
            Class::Mutate,
            "unknown methods outside the admin namespaces default to \
             Mutate: transparent in full, safe in readonly"
        );
        assert_eq!(
            classify("pane.future_thing"),
            Class::Mutate,
            "a new workspace-surface method stays usable in full mode"
        );
    }

    #[test]
    fn allowed_table() {
        assert!(allowed(Mode::Full, "pane.split"));
        assert!(allowed(Mode::Full, "pane.read"));
        assert!(
            !allowed(Mode::Full, "server.stop"),
            "host admin is denied even in full"
        );
        assert!(allowed(Mode::Readonly, "pane.read"));
        assert!(!allowed(Mode::Readonly, "pane.split"));
        assert!(!allowed(Mode::Readonly, "server.stop"));
    }

    #[test]
    fn deny_response_is_herdr_shaped() {
        let resp: serde_json::Value =
            serde_json::from_str(&deny_response("req_9", "server.stop", Mode::Full)).unwrap();
        assert_eq!(resp["id"], "req_9", "the request id echoes back");
        assert_eq!(resp["error"]["code"], "sandbox_denied");
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("server.stop"));
        assert!(resp.get("result").is_none());
    }

    #[test]
    fn socket_path_uses_the_container_name_when_it_fits() {
        let dir = Path::new("/Users/me/.pall8t/run");
        assert_eq!(
            socket_path(dir, "pall8t-x-abc12345-99").unwrap(),
            dir.join("pall8t-x-abc12345-99.sock"),
            "the readable name is the default — `ls ~/.pall8t/run` should say \
             which run a socket belongs to"
        );
    }

    /// Container names run to 63 chars (apple/container's DNS-label cap),
    /// and a deep home eats the rest of the 104-byte `sun_path` budget.
    /// Binding a too-long path fails at bind(2), so the name is truncated
    /// *before* it can — and disambiguated by a hash of the whole name, so
    /// two runs sharing a prefix don't collide on one socket.
    #[test]
    fn socket_path_truncates_and_disambiguates_when_the_budget_is_tight() {
        let dir = Path::new("/Users/a-rather-long-user-name/nested/deeper/.pall8t/run");
        let a = "pall8t-a-very-long-workspace-basename-here-aaaaaaaa-11111";
        let b = "pall8t-a-very-long-workspace-basename-here-bbbbbbbb-22222";
        let pa = socket_path(dir, a).unwrap();
        let pb = socket_path(dir, b).unwrap();
        assert_eq!(
            pa.as_os_str().len(),
            SUN_PATH_MAX,
            "bindable, and no shorter than it has to be: the point of \
             truncating rather than hashing the whole name is that as much \
             of it as fits stays readable"
        );
        assert!(
            pa.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&a[..8]),
            "the surviving head must be the start of the container name, not \
             some other string of the right length"
        );
        assert_ne!(
            pa, pb,
            "two long names truncated to the same head must still get different \
             sockets — sharing one would cross two sandboxes' bridges"
        );
        assert!(
            pa.starts_with(dir) && pa.extension().is_some_and(|e| e == "sock"),
            "still a .sock under the run dir, so reaping finds it"
        );
    }

    #[test]
    fn socket_path_gives_up_rather_than_returning_an_unbindable_path() {
        let dir = PathBuf::from("/".to_string() + &"d".repeat(SUN_PATH_MAX));
        assert!(
            socket_path(&dir, "pall8t-x").is_none(),
            "no truncation can rescue a directory that already fills sun_path; \
             the caller must report that, not hand out a path bind(2) rejects"
        );
    }

    #[test]
    fn socket_path_truncates_on_a_character_boundary() {
        let dir = Path::new("/Users/me/.pall8t/run");
        // Long enough to force truncation, and multibyte so a char-count
        // budget would overshoot the byte budget sun_path actually counts.
        let name = "pall8t-".to_string() + &"日".repeat(60);
        let path = socket_path(dir, &name).unwrap();
        assert!(
            path.as_os_str().len() <= SUN_PATH_MAX,
            "a char-counted truncation would exceed the byte budget: {} bytes",
            path.as_os_str().len()
        );
        assert!(
            path.to_str().is_some(),
            "truncation must not split a character and produce invalid UTF-8"
        );
    }

    /// Reaping unlinks files, so the probe's verdict has to distinguish
    /// "the peer answered: nothing here" from "the probe itself failed".
    /// Under fd pressure (`EMFILE`) a healthy sandbox's socket would
    /// otherwise be judged dead and unlinked, cutting its bridge
    /// mid-session with nothing logged on either side.
    #[test]
    fn only_a_real_answer_counts_as_death() {
        use std::io::ErrorKind;
        assert!(
            connect_says_dead(Some(ErrorKind::ConnectionRefused)),
            "a bound file with no listener is exactly what an exited run leaves"
        );
        assert!(
            connect_says_dead(Some(ErrorKind::NotFound)),
            "already gone counts as gone"
        );
        assert!(
            !connect_says_dead(None),
            "a successful connect is a live peer"
        );
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
            ErrorKind::Other,
        ] {
            assert!(
                !connect_says_dead(Some(kind)),
                "{kind:?} says the probe failed, not that the peer is gone — \
                 err toward keeping, like an unknown age does"
            );
        }
    }

    #[test]
    fn the_relay_only_touches_its_own_run_directory() {
        let root = PathBuf::from("/Users/me/.pall8t/run");
        assert!(
            check_run_socket_dir(&root, &root).is_ok(),
            "the directory `pall8t run` passes is the whole point"
        );
        let rejected = check_run_socket_dir(Path::new("/tmp"), &root).unwrap_err();
        assert!(
            rejected.to_string().contains("/Users/me/.pall8t/run"),
            "a hand-run `--listen /tmp/x.sock` must not chmod /tmp to 0700 — \
             the relay creates, chmods, and sweeps this directory — and the \
             refusal has to say where the socket belongs: {rejected}"
        );
        assert!(
            check_run_socket_dir(Path::new("/Users/me/.pall8t/run/nested"), &root).is_err(),
            "a directory under the root is still not the root"
        );
        assert!(
            check_run_socket_dir(Path::new("/Users/me/.pall8t"), &root).is_err(),
            "nor is its parent, which holds the config and the container home"
        );
    }

    #[test]
    fn stale_sockets_reaps_only_the_dead_ones() {
        let grace = std::time::Duration::from_mins(5);
        let old = Some(std::time::Duration::from_mins(10));
        let young = Some(std::time::Duration::from_secs(10));
        let live = PathBuf::from("/run/live.sock");
        let dead = PathBuf::from("/run/dead.sock");
        let fresh = PathBuf::from("/run/fresh.sock");
        let ageless = PathBuf::from("/run/ageless.sock");
        let reaped = stale_sockets(
            vec![
                (live.clone(), old),
                (dead.clone(), old),
                (fresh.clone(), young),
                (ageless.clone(), None),
            ],
            grace,
            |p| p == live,
        );
        assert_eq!(
            reaped,
            vec![dead],
            "only the dead-and-old socket is reaped: a live one belongs to a \
             running sandbox, a fresh one may belong to a relay that just \
             bound, and an unreadable age errs toward keeping"
        );
        assert!(
            stale_sockets(
                vec![(PathBuf::from("/run/exactly.sock"), Some(grace))],
                grace,
                |_| false,
            )
            .is_empty(),
            "the grace window is inclusive: a socket exactly at the boundary \
             is still protected"
        );
    }

    /// The reaping walk itself, on a real directory: it deletes files, so
    /// "which ones" is worth proving against the filesystem rather than
    /// only through the pure decision. No container and no herdr involved
    /// — just Unix sockets, like the forwarding tests above.
    #[test]
    fn reaping_removes_dead_sockets_and_leaves_everything_else() {
        let dir = test_dir("reap");
        let live_path = dir.join("live.sock");
        let live = UnixListener::bind(&live_path).unwrap();
        let dead_path = dir.join("dead.sock");
        // Binding and dropping leaves the file behind with nothing
        // listening — exactly what an exited run leaves.
        drop(UnixListener::bind(&dead_path).unwrap());
        let other_path = dir.join("relay.log");
        std::fs::write(&other_path, b"audit").unwrap();
        // Age is read from mtime against the wall clock, and a
        // just-written file can measure as zero-aged (or, with the
        // filesystem's timestamp granularity, as fractionally in the
        // future, which reads as "age unknown" and never reaps). A beat
        // puts every file safely past a zero grace.
        std::thread::sleep(std::time::Duration::from_millis(20));

        reap_stale_sockets(&dir, std::time::Duration::ZERO);

        assert!(
            !dead_path.exists(),
            "a socket nothing is listening on is a leftover and must go"
        );
        assert!(
            live_path.exists(),
            "a socket a concurrent run is still serving must survive — \
             unlinking it would cut that sandbox's bridge"
        );
        assert!(
            other_path.exists(),
            "only .sock files are candidates; the audit log lives here too"
        );
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_parse_roundtrip() {
        assert_eq!(Mode::parse("full").unwrap(), Mode::Full);
        assert_eq!(Mode::parse("readonly").unwrap(), Mode::Readonly);
        assert!(Mode::parse("off").is_err(), "off never reaches the relay");
        assert_eq!(
            Mode::parse(Mode::Readonly.as_str()).unwrap(),
            Mode::Readonly
        );
    }

    /// Full stack minus the container: a fake herdr (Unix echo server) on
    /// one side, a client playing the sandboxed herdr CLI on the other,
    /// `serve` in between.
    fn start_relay(mode: Mode, dir: &Path) -> (PathBuf, PathBuf) {
        let sock = dir.join("h.sock");
        let log = dir.join("relay.log");
        let upstream = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for conn in upstream.incoming() {
                let Ok(mut conn) = conn else { continue };
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    if r.read_line(&mut line).is_ok() {
                        let _ = conn.write_all(
                            format!("{{\"id\":\"x\",\"result\":{{\"echo\":{}}}}}\n", line.trim())
                                .as_bytes(),
                        );
                    }
                });
            }
        });
        let listen = dir.join("relay.sock");
        let listener = UnixListener::bind(&listen).unwrap();
        let log_clone = log.clone();
        let sock_clone = sock.clone();
        std::thread::spawn(move || serve(&listener, &sock_clone, mode, &log_clone));
        (listen, log)
    }

    fn roundtrip(listen: &Path, request: &str) -> serde_json::Value {
        let mut conn = UnixStream::connect(listen).unwrap();
        conn.write_all(request.as_bytes()).unwrap();
        conn.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn test_dir(name: &str) -> PathBuf {
        // Short base (std::env::temp_dir may exceed the 104-byte sun_path
        // limit once the socket name is appended on macOS).
        let dir = PathBuf::from("/tmp").join(format!("p8t-relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn relay_forwards_allowed_and_denies_blocked() {
        let dir = test_dir("fwd");
        let (listen, log) = start_relay(Mode::Full, &dir);

        let resp = roundtrip(&listen, r#"{"id":"r1","method":"pane.list","params":{}}"#);
        assert_eq!(
            resp["result"]["echo"]["method"], "pane.list",
            "an allowed request reaches the (fake) herdr socket and its reply comes back"
        );

        let resp = roundtrip(&listen, r#"{"id":"r2","method":"server.stop","params":{}}"#);
        assert_eq!(resp["id"], "r2");
        assert_eq!(
            resp["error"]["code"], "sandbox_denied",
            "host-admin never reaches the socket"
        );

        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("\"allow\"") && logged.contains("\"deny\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression pin for a review question: after `read_line` takes the
    /// first request, anything it prefetched past that newline sits in the
    /// `BufReader`'s internal buffer — the pump must forward those bytes,
    /// not drop them (which `BufReader::into_inner()` would do; the code
    /// unwraps only the `Take` cap). The client sends two NDJSON lines in
    /// one write and half-closes; the fake upstream echoes everything it
    /// received, so the reply proves both lines crossed the bridge. (TCP
    /// framing delivers one small write in one datagram-sized chunk, which
    /// is what puts line 2 in the prefetch buffer; if the kernel ever did
    /// split it, the bytes still arrive via the pump and the test still
    /// passes — it can't false-fail.)
    #[test]
    fn relay_forwards_bytes_prefetched_past_the_first_line() {
        let dir = test_dir("pipeline");
        let sock = dir.join("h.sock");
        let log = dir.join("relay.log");
        let upstream = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for conn in upstream.incoming() {
                let Ok(mut conn) = conn else { continue };
                std::thread::spawn(move || {
                    let mut all = String::new();
                    let mut r = conn.try_clone().unwrap();
                    if r.read_to_string(&mut all).is_ok() {
                        let _ = conn.write_all(
                            serde_json::json!({"id":"x","result":{"received": all}})
                                .to_string()
                                .as_bytes(),
                        );
                        let _ = conn.write_all(b"\n");
                    }
                });
            }
        });
        let listen = dir.join("relay.sock");
        let listener = UnixListener::bind(&listen).unwrap();
        let log_clone = log.clone();
        std::thread::spawn(move || serve(&listener, &sock, Mode::Full, &log_clone));

        let mut conn = UnixStream::connect(&listen).unwrap();
        conn.write_all(
            b"{\"id\":\"r1\",\"method\":\"pane.list\",\"params\":{}}\n{\"id\":\"r2\",\"method\":\"pane.get\",\"params\":{}}\n",
        )
        .unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let received = resp["result"]["received"].as_str().unwrap();
        assert!(
            received.contains("\"id\":\"r1\"") && received.contains("\"id\":\"r2\""),
            "both pipelined requests must reach the upstream socket, got: {received}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relay_readonly_denies_mutations() {
        let dir = test_dir("ro");
        let (listen, _) = start_relay(Mode::Readonly, &dir);

        let resp = roundtrip(&listen, r#"{"id":"r1","method":"pane.read","params":{}}"#);
        assert_eq!(resp["result"]["echo"]["method"], "pane.read");

        let resp = roundtrip(&listen, r#"{"id":"r2","method":"pane.split","params":{}}"#);
        assert_eq!(resp["error"]["code"], "sandbox_denied");

        let resp = roundtrip(&listen, "not json at all");
        assert_eq!(
            resp["error"]["code"], "sandbox_denied",
            "readonly cannot classify an unparseable request, so it must not forward it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
