//! Host-side relay bridging a sandboxed agent to the host herdr session
//! (ADR-0007). apple/container mounts cannot forward Unix-socket
//! connections (connect(2) fails with ENOTSUP through virtiofs), so the
//! herdr API socket can't simply be bind-mounted into the sandbox. What
//! does work is plain TCP from the container to the host's vmnet gateway
//! address. This relay is the host end of that path: `pall8t run` spawns
//! it just before exec'ing into the `container` client, it listens on an
//! ephemeral TCP port, and it forwards newline-delimited-JSON herdr
//! requests to the real `herdr.sock` — after a per-request policy check
//! (see [`classify`]) and with every accepted connection pinned to the
//! sandbox container's own IP. Inside the container, a `socat
//! UNIX-LISTEN:…,fork TCP:<gateway>:<port>` bridge (started by the
//! bootstrap in [`crate::herdr`]) turns the TCP endpoint back into the
//! Unix socket the stock herdr CLI expects at `HERDR_SOCKET_PATH`.
//!
//! Lifecycle: the relay watches its parent — the pall8t process that
//! becomes the `container` client via exec (same pid) — and exits as soon
//! as it is reparented, so it lives exactly as long as the sandbox
//! session and needs no cleanup protocol.
//!
//! Security posture, relative to herdr's own 0600-socket model: a TCP
//! listener is reachable by every process on the machine and every
//! container on the vmnet, so each connection's peer address must equal
//! the one sandbox container's address (resolved via `container inspect`)
//! or the connection is dropped. Method policy on top of that keeps the
//! sandbox from administering the host herdr installation itself; see
//! [`classify`] for the split.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

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

/// Which peers a connection may come from. `Container` pins to the named
/// sandbox container's address (the production path); `AllowAll` exists
/// for tests, where there is no container.
pub enum PeerPolicy {
    Container(String),
    AllowAll,
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

/// Methods that administer the host herdr installation (always denied).
const HOST_ADMIN: &[&str] = &[
    "server.stop",
    "server.live_handoff",
    "server.reload_config",
    "server.reload_agent_manifests",
    "integration.install",
    "integration.uninstall",
    "plugin.link",
    "plugin.unlink",
    "plugin.enable",
    "plugin.disable",
];

/// Pure-inspection methods (allowed even in `readonly`), from herdr 0.7.5's
/// method inventory. A read method a newer herdr adds is missing here until
/// this list is refreshed — it is then treated as [`Class::Mutate`], which
/// only ever errs toward denying, never toward leaking a mutation into
/// `readonly`.
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
    if HOST_ADMIN.contains(&method) {
        Class::HostAdmin
    } else if READ.contains(&method) {
        Class::Read
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

/// How the relay decides whether a connection's peer is the sandbox.
/// The container's address is resolved lazily (the container doesn't
/// exist yet when the relay starts) and cached once found.
struct PeerGate {
    policy: PeerPolicy,
    cached: Mutex<Option<IpAddr>>,
}

impl PeerGate {
    fn permits(&self, peer: IpAddr) -> bool {
        let name = match &self.policy {
            PeerPolicy::AllowAll => return true,
            PeerPolicy::Container(name) => name,
        };
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.is_none() {
            *cached = crate::container::ip_address(name).and_then(|s| s.parse::<IpAddr>().ok());
        }
        *cached == Some(peer)
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
/// never by hand). Binds an ephemeral port, prints it as the single stdout
/// line the parent reads, then serves until the parent exits.
pub fn run(socket: &Path, container: &str, mode: Mode, log_path: &Path) -> Result<()> {
    let listener =
        TcpListener::bind(("0.0.0.0", 0)).context("cannot bind the relay TCP listener")?;
    let port = listener.local_addr()?.port();
    println!("{port}");
    // The port line is the whole stdout contract; anything later would
    // land in a pipe nobody reads (the parent execs away).
    drop(std::io::stdout().flush());

    watch_parent();
    let gate = std::sync::Arc::new(PeerGate {
        policy: PeerPolicy::Container(container.to_string()),
        cached: Mutex::new(None),
    });
    audit(
        log_path,
        &serde_json::json!({
            "ts": epoch_secs(), "event": "start",
            "port": port, "mode": mode.as_str(), "container": container,
            "socket": socket.display().to_string(),
        }),
    );
    serve(&listener, socket, mode, &gate, log_path);
    Ok(())
}

/// Accept loop, factored from [`run`] so tests can drive it on a listener
/// and peer policy they control.
fn serve(
    listener: &TcpListener,
    socket: &Path,
    mode: Mode,
    gate: &std::sync::Arc<PeerGate>,
    log_path: &Path,
) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let socket = socket.to_path_buf();
        let gate = std::sync::Arc::clone(gate);
        let log_path = log_path.to_path_buf();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &socket, mode, &gate, &log_path) {
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

/// One connection: peer gate → read the first request line → policy →
/// forward to the herdr socket and pump bytes both ways until either side
/// closes (streaming methods like `events.subscribe` hold the connection).
fn handle(
    conn: TcpStream,
    socket: &Path,
    mode: Mode,
    gate: &PeerGate,
    log_path: &Path,
) -> Result<()> {
    let peer = conn.peer_addr().context("no peer address")?.ip();
    if !gate.permits(peer) {
        audit(
            log_path,
            &serde_json::json!({
                "ts": epoch_secs(), "event": "rejected_peer",
                "peer": peer.to_string(),
            }),
        );
        return Ok(());
    }

    let mut conn = conn;
    let mut reader = BufReader::new(conn.try_clone()?).take(MAX_REQUEST_LINE);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("cannot read the request line")?;
    if line.trim().is_empty() {
        return Ok(());
    }

    let parsed: Option<serde_json::Value> = serde_json::from_str(line.trim()).ok();
    let method = parsed
        .as_ref()
        .and_then(|v| v.get("method"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let id = parsed
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("");

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
            "peer": peer.to_string(), "method": method,
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
        assert_eq!(
            classify("future.method"),
            Class::Mutate,
            "unknown methods default to Mutate: transparent in full, safe in readonly"
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
    /// one side, a TCP client playing the in-container bridge on the
    /// other, `serve` in between.
    fn start_relay(mode: Mode, dir: &Path) -> (std::net::SocketAddr, PathBuf) {
        use std::os::unix::net::UnixListener;
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let gate = std::sync::Arc::new(PeerGate {
            policy: PeerPolicy::AllowAll,
            cached: Mutex::new(None),
        });
        let log_clone = log.clone();
        let sock_clone = sock.clone();
        std::thread::spawn(move || serve(&listener, &sock_clone, mode, &gate, &log_clone));
        (addr, log)
    }

    fn roundtrip(addr: std::net::SocketAddr, request: &str) -> serde_json::Value {
        let mut conn = TcpStream::connect(addr).unwrap();
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
        let (addr, log) = start_relay(Mode::Full, &dir);

        let resp = roundtrip(addr, r#"{"id":"r1","method":"pane.list","params":{}}"#);
        assert_eq!(
            resp["result"]["echo"]["method"], "pane.list",
            "an allowed request reaches the (fake) herdr socket and its reply comes back"
        );

        let resp = roundtrip(addr, r#"{"id":"r2","method":"server.stop","params":{}}"#);
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
    /// framing on loopback delivers one small write in one segment, which
    /// is what puts line 2 in the prefetch buffer; if the kernel ever did
    /// split it, the bytes still arrive via the pump and the test still
    /// passes — it can't false-fail.)
    #[test]
    fn relay_forwards_bytes_prefetched_past_the_first_line() {
        use std::os::unix::net::UnixListener;
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let gate = std::sync::Arc::new(PeerGate {
            policy: PeerPolicy::AllowAll,
            cached: Mutex::new(None),
        });
        let log_clone = log.clone();
        std::thread::spawn(move || serve(&listener, &sock, Mode::Full, &gate, &log_clone));

        let mut conn = TcpStream::connect(addr).unwrap();
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
        let (addr, _) = start_relay(Mode::Readonly, &dir);

        let resp = roundtrip(addr, r#"{"id":"r1","method":"pane.read","params":{}}"#);
        assert_eq!(resp["result"]["echo"]["method"], "pane.read");

        let resp = roundtrip(addr, r#"{"id":"r2","method":"pane.split","params":{}}"#);
        assert_eq!(resp["error"]["code"], "sandbox_denied");

        let resp = roundtrip(addr, "not json at all");
        assert_eq!(
            resp["error"]["code"], "sandbox_denied",
            "readonly cannot classify an unparseable request, so it must not forward it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
