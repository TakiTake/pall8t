//! Bridges the host↔container boundary for [herdr](https://github.com/ogulcancelik/herdr).
//! herdr injects `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`/
//! `HERDR_BIN_PATH` into the pane process it spawns — which is `pall8t`
//! itself, on the host. None of that is visible to `claude` once `pall8t
//! run` execs into the sandboxed container, so any herdr-facing action has
//! to happen here, before the exec (see `main.rs::exec_container`).

use anyhow::{Context, Result};

/// A herdr pane's identity, as seen by the host-side `pall8t` process.
pub struct HerdrEnv {
    pub pane_id: String,
    /// `HERDR_WORKSPACE_ID`/`HERDR_TAB_ID` — forwarded into the sandbox so
    /// the herdr CLI's `--current`/caller-context conventions keep working
    /// there (see [`crate::relay`]); nothing host-side reads them.
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub socket_path: Option<String>,
    pub bin_path: Option<String>,
    /// `HERDR_AGENT` — herdr's own agent-hint convention. herdr itself only
    /// reads it from `/proc/<pid>/environ`, so it does nothing on macOS;
    /// pall8t honors it instead via [`agent_hint`].
    pub agent: Option<String>,
}

impl HerdrEnv {
    fn herdr_bin(&self) -> &str {
        self.bin_path.as_deref().unwrap_or("herdr")
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// `HERDR_ENV=1` plus a usable `HERDR_PANE_ID` — anything less isn't a
/// herdr pane worth acting on.
pub fn detect() -> Option<HerdrEnv> {
    if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
        return None;
    }
    Some(HerdrEnv {
        pane_id: non_empty_env("HERDR_PANE_ID")?,
        workspace_id: non_empty_env("HERDR_WORKSPACE_ID"),
        tab_id: non_empty_env("HERDR_TAB_ID"),
        socket_path: non_empty_env("HERDR_SOCKET_PATH"),
        bin_path: non_empty_env("HERDR_BIN_PATH"),
        agent: non_empty_env("HERDR_AGENT"),
    })
}

/// The agent name this pane should identify as: the first name in the
/// sandboxed command that herdr recognizes (see [`agent_from_command`]),
/// falling back to `HERDR_AGENT` when the command contains none — a
/// wrapper script, `sh -c '…'`. The command wins over the env var because
/// it is what actually runs, while `HERDR_AGENT` is often ambient
/// (hardcoded in a shell wrapper) — env-first would mislabel an explicit
/// `pall8t run -- codex` as `claude` for anyone who baked
/// `HERDR_AGENT=claude` into their launcher. `main.rs` passes the hint as
/// `argv[0]` of the exec'd `container` client: herdr assigns pane identity
/// from the HOST process tree only — on macOS the argv0 basename via
/// `sysctl(KERN_PROCARGS2)` — and that identity is what gates its
/// screen-content state detection (idle/working/blocked). With argv0 left
/// as `container`, herdr never recognizes the pane and the agent's state
/// is never tracked; with the agent's name there, herdr matches it and
/// reads the state straight off the pane's terminal, which shows the
/// sandboxed agent's real UI.
pub fn agent_hint(env: &HerdrEnv, command: &[String]) -> Option<String> {
    agent_from_command(command).or_else(|| env.agent.clone())
}

/// The one herdr-pane identity entry point for `pall8t run`: derives the
/// agent hint, explains a shadowed `HERDR_AGENT`, reports the sidebar
/// display name, and returns the hint for the exec's argv0. Everything
/// here is best-effort chrome — failures warn and the run continues — and
/// with no derivable name it does nothing at all: better to leave the
/// pane's herdr-side identity alone than to assert a guess.
pub fn announce_pane_identity(
    env: &HerdrEnv,
    command: &[String],
    provenance: &Provenance,
) -> Option<String> {
    let agent = agent_hint(env, command)?;
    // The command wins over HERDR_AGENT (see agent_hint); say so when they
    // disagree, or the shadowed env var is undebuggable.
    if let Some(env_agent) = env.agent.as_deref().filter(|a| *a != agent) {
        eprintln!(
            "pall8t: note: herdr pane agent is {agent:?} (from the run \
             command); HERDR_AGENT={env_agent:?} ignored"
        );
    }
    if let Err(e) = report_metadata(env, &agent, provenance) {
        eprintln!("pall8t: warning: could not report herdr pane metadata: {e:#}");
    }
    Some(agent)
}

/// The process names herdr's own `identify_agent` (its `detect` module)
/// recognizes, including aliases. Kept in sync manually; drift is safe —
/// an agent missing here just means no derived hint, and `HERDR_AGENT`
/// takes over.
const KNOWN_AGENTS: &[&str] = &[
    "pi",
    "claude",
    "claude-code",
    "codex",
    "gemini",
    "cursor",
    "cursor-agent",
    "devin",
    "devin-cli",
    "agy",
    "antigravity",
    "antigravity-cli",
    "cline",
    "omp",
    "mastracode",
    "mastra-code",
    "opencode",
    "open-code",
    "copilot",
    "github-copilot",
    "ghcs",
    "kimi",
    "kimi-code",
    "kiro",
    "kiro-cli",
    "droid",
    "amp",
    "amp-local",
    "grok",
    "grok-build",
    "hermes",
    "hermes-agent",
    "kilo",
    "kilo-code",
    "qodercli",
    "qoder",
    "maki",
];

/// First token of the command that names an agent herdr recognizes.
/// Scanning every token instead of locating "the program" looks through
/// arbitrary launchers (`env FOO=1 claude`, `npx -y codex`, `uv run
/// claude`) without modeling their flag grammars. Only the first token
/// may be a path (`/usr/local/bin/claude`): reducing *later* tokens to a
/// basename would let argument paths and assignments mislabel the pane
/// (`env HOME=/home/claude codex` must not derive `claude`), so they
/// only match as bare names or npm package specs (`claude@latest`,
/// `@anthropic-ai/claude-code`). A name not in [`KNOWN_AGENTS`] is never
/// worth asserting — herdr wouldn't match it, and a confident wrong
/// answer would also shadow the caller's `HERDR_AGENT` fallback.
/// Guess-averse by design; the residual wrong match is a bare
/// agent-named *argument* before the real program, which has no
/// syntactic tell.
fn agent_from_command(command: &[String]) -> Option<String> {
    command
        .iter()
        .enumerate()
        .find_map(|(i, token)| agent_name_token(token, i == 0))
}

fn agent_name_token(token: &str, allow_path: bool) -> Option<String> {
    let name = if allow_path {
        std::path::Path::new(token).file_name()?.to_str()?
    } else if let Some(scoped) = token.strip_prefix('@') {
        // npm scoped spec: @scope/name[@version]
        scoped.split_once('/')?.1
    } else {
        token
    };
    let name = name.split_once('@').map_or(name, |(base, _)| base);
    KNOWN_AGENTS.contains(&name).then(|| name.to_string())
}

/// If the resolved run command is the opt-in Claude-Code agent-teams tmux
/// wrapper (README: `command = ["tmux", "new", "-A", "-s", "claude",
/// "claude"]`) and we're inside a herdr pane, skip it in favor of plain
/// `claude` — herdr already supplies persistence/multiplexing, and the
/// wrapper is redundant chrome herdr doesn't need. Any other configured
/// command is left untouched: only this one documented shape is known to be
/// a multiplexer wrapper.
pub fn maybe_override_for_herdr(command: Vec<String>, herdr_active: bool) -> Vec<String> {
    if herdr_active && command.first().map(String::as_str) == Some("tmux") {
        vec!["claude".to_string()]
    } else {
        command
    }
}

/// What this run is, for the herdr sidebar: the image it booted and the
/// container it runs as. Both are things a user comparing several
/// sandboxed panes actually needs — which image tag is this one on, and
/// which container do I `pall8t exec` into.
pub struct Provenance {
    pub image: String,
    pub container: String,
}

/// herdr caps a token value at 80 characters and drops the report if a
/// value carries control characters, so long image tags are trimmed here
/// rather than silently costing the whole metadata report.
const TOKEN_VALUE_MAX: usize = 80;

fn token_value(value: &str) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() <= TOKEN_VALUE_MAX {
        return clean;
    }
    // Keep the tail: an image tag's distinguishing part (the content hash)
    // is at the end, and the shared `pall8t-<project>` head is what every
    // pane in a project has in common anyway.
    let skip = clean.chars().count() - (TOKEN_VALUE_MAX - 1);
    format!("…{}", clean.chars().skip(skip).collect::<String>())
}

fn report_metadata_argv(pane_id: &str, agent: &str, provenance: &Provenance) -> Vec<String> {
    vec![
        "pane".into(),
        "report-metadata".into(),
        pane_id.into(),
        "--source".into(),
        "user:pall8t".into(),
        "--display-agent".into(),
        format!("{agent} (pall8t)"),
        "--token".into(),
        format!("pall8t_image={}", token_value(&provenance.image)),
        "--token".into(),
        format!("pall8t_container={}", token_value(&provenance.container)),
    ]
}

/// Sidebar identity: `herdr pane report-metadata <pane_id> --source
/// user:pall8t --display-agent "<agent> (pall8t)"`, so herdr's UI makes it
/// clear the agent is sandboxed. Deliberately omits `--agent`: herdr's
/// guard for showing `display_agent` requires it to match
/// `effective_agent_label()`, which herdr derives from the HOST's own
/// process tree (`identify_agent_in_job` in herdr's `detect` module) — a
/// match that only holds once the argv0 hint (see [`agent_hint`]) has
/// taken effect, and the report must not depend on that. Confirmed live:
/// with `--agent claude` set and no argv0 hint, `herdr pane get` never
/// surfaces `display_agent`; omitting it, the field shows up immediately.
/// The tokens ride along in the same report: herdr has accepted
/// `--token` since 0.7.5 (the oldest release pall8t's bridge targets), so
/// a second call would only buy protection against a version nothing
/// supports. The whole report is best-effort anyway — a failure warns and
/// the run continues without a sidebar name.
fn report_metadata(env: &HerdrEnv, agent: &str, provenance: &Provenance) -> Result<()> {
    let argv = report_metadata_argv(&env.pane_id, agent, provenance);
    crate::util::run_ok(env.herdr_bin(), &argv)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sandbox bridge (ADR-0007): make the herdr CLI work *inside* the container.
// ---------------------------------------------------------------------------

/// Where the in-container Unix socket for the herdr CLI lives. Under /tmp
/// (container-local tmpfs): per-container, never persisted, writable by
/// `dev`, and short enough for `sun_path`.
pub const CONTAINER_SOCKET_PATH: &str = "/tmp/pall8t/herdr.sock";

/// Mount destination for the version-matched Linux `herdr` binary; the
/// bootstrap prepends it to `PATH`.
pub const CONTAINER_BIN_DIR: &str = "/opt/pall8t/bin";

/// In-container bootstrap, run as the command's `sh -c` prologue: puts
/// the mounted Linux herdr CLI on `PATH`, then execs the real command.
///
/// The socket the CLI talks to needs nothing from this script — the relay
/// socket is mounted straight in at `HERDR_SOCKET_PATH` (see
/// [`crate::relay`]). Before that, this prologue also ran a `socat
/// UNIX-LISTEN:… TCP:<gateway>:<port>` bridge, which is why custom
/// Containerfiles used to need `socat`; they no longer do.
const BOOTSTRAP: &str = r#"
if [ -d /opt/pall8t/bin ]; then
  PATH="/opt/pall8t/bin:$PATH"
  export PATH
fi
exec "$@"
"#;

/// Env + mounts the bridge adds to the `container run` invocation.
pub struct SandboxBridge {
    pub env: Vec<(String, String)>,
    pub mounts: Vec<crate::container::Mount>,
}

/// Assembles the herdr bridge for one `pall8t run`: spawns the host-side
/// relay (see [`crate::relay`]), provisions the version-matched Linux
/// `herdr` binary (best-effort), and returns the env/mounts to add.
/// `Ok(None)` means the bridge is off by configuration; `Err` means it was
/// wanted but couldn't be set up (callers warn and run without it —
/// the bridge must never break a sandbox launch).
pub fn prepare_bridge(
    env: &HerdrEnv,
    mode: crate::config::HerdrSandbox,
    container_name: &str,
) -> Result<Option<SandboxBridge>> {
    use crate::config::HerdrSandbox;
    let relay_mode = match mode {
        HerdrSandbox::Off => return Ok(None),
        HerdrSandbox::Full => crate::relay::Mode::Full,
        HerdrSandbox::Readonly => crate::relay::Mode::Readonly,
    };
    let socket = env
        .socket_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("herdr did not provide HERDR_SOCKET_PATH for this pane"))?;

    let mut mounts = Vec::new();
    match ensure_linux_herdr(env.herdr_bin())
        .and_then(|source_dir| stage_run_local_herdr(&source_dir, container_name))
    {
        // Still writable, and still a per-run copy. ADR-0007 built that
        // copy *because* nothing could be mounted read-only; now that
        // something can (ADR-0009), the shared verified cache could be
        // mounted directly and the copy-plus-pruning machinery retired.
        // That is a change to the bridge's threat model and wants its own
        // change with a live herdr session to test against, so this stays
        // as it was rather than being altered in passing.
        Ok(dir) => mounts.push(crate::container::Mount::rw(dir, CONTAINER_BIN_DIR.into())),
        // Env + relay still work without the CLI (raw socket clients, e.g.
        // herdr's own agent-state integration hooks) — degrade, don't fail.
        Err(e) => eprintln!(
            "pall8t: warning: no Linux herdr binary for the sandbox ({e:#}) — \
             the bridge is up, but the `herdr` CLI won't be on PATH inside"
        ),
    }

    let listen = spawn_relay(socket, container_name, relay_mode)?;
    // The relay's own socket, forwarded into the sandbox as a socket
    // (Mount::socket) — this is the bridge's whole transport.
    mounts.push(crate::container::Mount::socket(
        listen,
        CONTAINER_SOCKET_PATH.into(),
    )?);
    let mut vars = vec![
        ("HERDR_ENV".to_string(), "1".to_string()),
        ("HERDR_PANE_ID".to_string(), env.pane_id.clone()),
        (
            "HERDR_SOCKET_PATH".to_string(),
            CONTAINER_SOCKET_PATH.to_string(),
        ),
        (
            "HERDR_BIN_PATH".to_string(),
            format!("{CONTAINER_BIN_DIR}/herdr"),
        ),
    ];
    if let Some(w) = &env.workspace_id {
        vars.push(("HERDR_WORKSPACE_ID".to_string(), w.clone()));
    }
    if let Some(t) = &env.tab_id {
        vars.push(("HERDR_TAB_ID".to_string(), t.clone()));
    }
    Ok(Some(SandboxBridge { env: vars, mounts }))
}

/// Wraps the run command in the [`BOOTSTRAP`] prologue. Applied after
/// [`agent_hint`] derivation (the hint must see the user's own command);
/// the original tokens survive as `sh` arguments, so herdr's token scan
/// would still find the agent name either way.
pub fn wrap_command_for_bridge(command: Vec<String>) -> Vec<String> {
    let mut argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        BOOTSTRAP.to_string(),
        "pall8t-bootstrap".to_string(), // $0 for the -c script
    ];
    argv.extend(command);
    argv
}

/// Runtime directory for the per-run relay sockets (`~/.pall8t/run`).
/// Deliberately not under `tools/`: those are cached artifacts, these are
/// live endpoints whose lifetime is one `pall8t run`.
fn run_socket_root() -> Result<std::path::PathBuf> {
    Ok(crate::config::pall8t_root()?.join("run"))
}

/// Spawns `pall8t herdr relay …` (the hidden serving loop) and returns
/// the socket it listens on, once bound: the relay prints the path as its
/// readiness line, and `container run` needs the socket to exist before
/// it can take it as a mount source. The child outlives the coming exec
/// and watches its parent to exit with the session.
fn spawn_relay(
    socket: &str,
    container_name: &str,
    mode: crate::relay::Mode,
) -> Result<std::path::PathBuf> {
    use std::io::BufRead;
    let exe = std::env::current_exe().context("cannot locate the pall8t binary")?;
    let log_dir = crate::config::pall8t_root()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log = log_dir.join(format!("herdr-relay-{container_name}.log"));
    let root = run_socket_root()?;
    let listen = crate::relay::socket_path(&root, container_name)
        .with_context(|| format!("no relay socket path fits under {}", root.display()))?;
    let mut child = std::process::Command::new(exe)
        .args([
            "herdr",
            "relay",
            "--socket",
            socket,
            "--listen",
            &listen.to_string_lossy(),
            "--mode",
            mode.as_str(),
            "--log",
            &log.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot spawn the herdr relay")?;
    let stdout = child.stdout.take().context("relay stdout not piped")?;
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .context("cannot read the relay readiness line")?;
    if line.trim().is_empty() {
        // No line at all: the relay exited before binding (its own error
        // went to its stderr, which is dropped — this is the pall8t-side
        // symptom, and the caller warns and runs without the bridge).
        // Reap it rather than leaving a zombie under the `container`
        // client this process is about to become.
        let _ = child.wait();
        anyhow::bail!("the herdr relay exited before binding {}", listen.display());
    }
    let bound = std::path::PathBuf::from(line.trim());
    if bound != listen {
        // It bound *something* — a path this process won't mount — so it
        // would sit there serving a policy-checked socket for the whole
        // session with no reader. Stop it.
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("unexpected relay readiness line {line:?}");
    }
    Ok(bound)
}

/// `herdr --version` → `"0.7.5"`. The version pin matters: herdr's CLI
/// refuses to talk across any protocol-version difference, so the Linux
/// binary in the sandbox must come from exactly the host's release.
/// Goes through [`crate::util::run_ok`] like every other parsed-output CLI
/// call in the crate; `herdr --version` exits 0, and a nonzero exit
/// wouldn't yield a `parse_herdr_version`-acceptable token anyway.
pub fn host_herdr_version(bin: &str) -> Option<String> {
    let out = crate::util::run_ok(bin, &["--version".to_string()]).ok()?;
    parse_herdr_version(&out)
}

/// The cache layout for the host's herdr `version`: `(dir, bin, sidecar)`.
/// The sidecar sits in the parent of `dir` on purpose — `dir` is what gets
/// mounted rw into sandboxes, so the integrity record must live outside it
/// (see [`cache_verified`]). One definition so [`ensure_linux_herdr`] and
/// [`cached_linux_herdr`] can never disagree on where "verified" is
/// recorded.
fn cache_paths(
    version: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let root = crate::config::pall8t_root()?.join("tools").join("herdr");
    let dir = root.join(version);
    let bin = dir.join("herdr");
    let sidecar = root.join(format!("{version}.sha256"));
    Ok((dir, bin, sidecar))
}

fn parse_herdr_version(stdout: &str) -> Option<String> {
    let token = stdout.split_whitespace().nth(1)?;
    // Guard against surprising output shapes: accept only digits-and-dots.
    token
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.')
        .then(|| token.to_string())
}

/// The herdr release asset for a Linux container of the host's CPU
/// architecture (apple/container runs native-arch VMs; the musl-static
/// builds have no libc coupling to worry about).
fn linux_asset_name() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("herdr-linux-aarch64"),
        "x86_64" => Ok("herdr-linux-x86_64"),
        other => Err(anyhow::anyhow!("no herdr Linux build for {other}")),
    }
}

/// Full sha256 of a file, lowercase hex.
fn sha256_file(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(crate::container::sha256_hex_prefix(&bytes, 32))
}

/// True if `bin`'s content matches the hash recorded in `sidecar`.
/// The sidecar lives OUTSIDE the directory that gets mounted into
/// sandboxes, so a sandboxed process (which shares the host uid and gets
/// the mount rw — apple/container has no read-only mounts) can tamper
/// with the binary but not with the record used to check it.
fn cache_verified(bin: &std::path::Path, sidecar: &std::path::Path) -> bool {
    let Ok(recorded) = std::fs::read_to_string(sidecar) else {
        return false;
    };
    sha256_file(bin).is_ok_and(|actual| actual == recorded.trim())
}

/// Ensures a verified `~/.pall8t/tools/herdr/<version>/herdr` exists and
/// returns its directory — the mount source for [`CONTAINER_BIN_DIR`].
///
/// Integrity model (review findings on PR #38): herdr's releases publish
/// no checksums, so the first download trusts TLS to github.com —
/// trust-on-first-use, recorded as a sha256 sidecar stored outside the
/// mounted directory. Every later run re-verifies the cached binary
/// against that record before mounting it: a sandbox that tampered with
/// the binary through the rw mount poisons only its own already-running
/// session, and the next run detects the mismatch and re-downloads. The
/// download uses a per-pid temp name so two cold-cache runs racing each
/// other both publish complete files via atomic rename (same asset, so
/// whichever rename lands last is equivalent).
fn ensure_linux_herdr(host_bin: &str) -> Result<std::path::PathBuf> {
    let version = host_herdr_version(host_bin)
        .ok_or_else(|| anyhow::anyhow!("cannot determine the host herdr version"))?;
    let (dir, bin, sidecar) = cache_paths(&version)?;
    if bin.exists() {
        if cache_verified(&bin, &sidecar) {
            return Ok(dir);
        }
        eprintln!(
            "pall8t: cached herdr binary failed integrity verification — re-downloading \
             (a sandbox may have modified it through the mount)"
        );
        std::fs::remove_file(&bin).ok();
    }
    let asset = linux_asset_name()?;
    let url = format!("https://github.com/ogulcancelik/herdr/releases/download/v{version}/{asset}");
    eprintln!("pall8t: downloading {asset} v{version} for the sandbox…");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".herdr.partial.{}", std::process::id()));
    crate::util::run_ok(
        "curl",
        &[
            "-fsSL".to_string(),
            "--retry".to_string(),
            "2".to_string(),
            "-o".to_string(),
            tmp.to_string_lossy().into_owned(),
            url.clone(),
        ],
    )
    .with_context(|| format!("download failed: {url}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // Sidecar first, then the binary: a crash between the two leaves a
    // sidecar without a matching binary, which the next run treats as a
    // verification failure and re-downloads — never a trusted-but-wrong
    // state.
    let digest = sha256_file(&tmp)?;
    // Temp beside the sidecar (same dir) so the publish is an atomic rename.
    let sidecar_tmp = sidecar.with_extension(format!("sha256.{}", std::process::id()));
    std::fs::write(&sidecar_tmp, &digest)?;
    std::fs::rename(&sidecar_tmp, &sidecar)?;
    std::fs::rename(&tmp, &bin)?;
    Ok(dir)
}

/// Root for per-run private copies of the herdr CLI (see
/// [`stage_run_local_herdr`]).
fn run_bin_root() -> Result<std::path::PathBuf> {
    Ok(crate::config::pall8t_root()?
        .join("tools")
        .join("herdr-run"))
}

/// Copies the verified herdr binary from the shared cache into a private
/// per-run directory and returns that directory — the mount source, in
/// place of the shared cache dir itself.
///
/// Why the copy (review finding on PR #38): apple/container has no
/// read-only mounts, so whatever directory is mounted is writable by the
/// sandbox (same host uid). Mounting the shared cache directly let one
/// sandbox overwrite the binary that a *concurrently running* sandbox
/// executes. With a per-run copy, the verified source in
/// [`ensure_linux_herdr`]'s cache is NEVER mounted — a sandbox can only
/// corrupt its own throwaway copy, which breaks nothing but its own herdr
/// CLI. Stale copies from exited (`--rm`'d) runs are pruned best-effort.
fn stage_run_local_herdr(
    source_dir: &std::path::Path,
    container_name: &str,
) -> Result<std::path::PathBuf> {
    let root = run_bin_root()?;
    prune_stale_run_bins(&root, container_name);
    let dir = root.join(container_name);
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join("herdr");
    std::fs::copy(source_dir.join("herdr"), &dst)
        .with_context(|| format!("cannot stage herdr binary into {}", dir.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// Grace window shielding a per-run copy from being reaped between its
/// creation and its container appearing in `container list` — a
/// concurrently launching run's copy is fresh, so the age guard keeps it.
const RUN_BIN_REAP_GRACE: std::time::Duration = std::time::Duration::from_mins(5);

/// Best-effort removal of per-run herdr copies left by exited runs. Only
/// the pure decision is factored out (see [`should_reap_run_bin`]); this
/// walks the directory, reads live container names once, and applies it.
fn prune_stale_run_bins(root: &std::path::Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let live: std::collections::HashSet<String> = crate::container::list_pall8t()
        .map(|cs| cs.into_iter().map(|c| c.name).collect())
        .unwrap_or_default();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok());
        if should_reap_run_bin(&name, current, &live, age, RUN_BIN_REAP_GRACE) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether a per-run herdr copy directory named `name` should be removed:
/// it is neither the current run nor a live container, and it is older
/// than `grace` (an unknown age never reaps — err toward keeping).
fn should_reap_run_bin(
    name: &str,
    current: &str,
    live: &std::collections::HashSet<String>,
    age: Option<std::time::Duration>,
    grace: std::time::Duration,
) -> bool {
    name != current && !live.contains(name) && age.is_some_and(|a| a > grace)
}

/// Raw env-var snapshot for `pall8t herdr doctor`, kept separate from the
/// live probes (socket connect, binary resolution) so the check logic below
/// is pure and testable without touching the real process env (tests run in
/// parallel; mutating `std::env` per-test is racy).
#[derive(Debug, Default)]
pub struct DoctorSnapshot {
    pub herdr_env: Option<String>,
    pub pane_id: Option<String>,
    pub socket_path: Option<String>,
    pub bin_path: Option<String>,
    pub agent: Option<String>,
}

impl DoctorSnapshot {
    pub fn from_process_env() -> Self {
        DoctorSnapshot {
            herdr_env: std::env::var("HERDR_ENV").ok(),
            pane_id: non_empty_env("HERDR_PANE_ID"),
            socket_path: non_empty_env("HERDR_SOCKET_PATH"),
            bin_path: non_empty_env("HERDR_BIN_PATH"),
            agent: non_empty_env("HERDR_AGENT"),
        }
    }

    pub fn herdr_bin(&self) -> &str {
        self.bin_path.as_deref().unwrap_or("herdr")
    }
}

/// The cached Linux herdr CLI matching the host's version, if it has been
/// downloaded already AND still passes integrity verification (see
/// [`ensure_linux_herdr`]) — for `doctor`.
pub fn cached_linux_herdr(host_bin: &str) -> Option<std::path::PathBuf> {
    let version = host_herdr_version(host_bin)?;
    let (_, bin, sidecar) = cache_paths(&version).ok()?;
    // No separate `bin.exists()`: cache_verified already returns false when
    // the binary can't be read (see `cache_verified_table`).
    cache_verified(&bin, &sidecar).then_some(bin)
}

/// Bridge-prerequisite lines appended to `doctor`'s report. Both are
/// informational (`ok: true` regardless): a mode is never wrong, and a
/// missing cached CLI just downloads on the next bridged run.
pub fn bridge_checks(mode: &str, cached: Option<&std::path::Path>) -> Vec<DoctorCheck> {
    vec![
        DoctorCheck {
            name: "bridge mode",
            ok: true,
            detail: format!("[herdr] sandbox = \"{mode}\" (full | readonly | off)"),
        },
        DoctorCheck {
            name: "linux herdr CLI",
            ok: true,
            detail: match cached {
                Some(p) => format!("cached at {}", p.display()),
                None => "not cached yet (downloads on the first bridged run)".to_string(),
            },
        },
    ]
}

/// True if `bin` can be spawned at all (`--version`), regardless of exit
/// code — resolvability is the question, not whether `--version` succeeds.
pub fn bin_resolvable(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .is_ok()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// One diagnostic line per herdr precondition. `socket_reachable`/
/// `bin_resolvable` are pre-computed by the caller (real IO: a Unix socket
/// connect attempt, a `herdr --version` probe) so this function stays pure.
pub fn doctor_checks(
    snap: &DoctorSnapshot,
    socket_reachable: bool,
    bin_resolvable: bool,
) -> Vec<DoctorCheck> {
    vec![
        DoctorCheck {
            name: "HERDR_ENV",
            ok: snap.herdr_env.as_deref() == Some("1"),
            detail: match snap.herdr_env.as_deref() {
                Some("1") => "set to 1".to_string(),
                Some(v) => format!("set to {v:?} (expected \"1\") — not a herdr pane"),
                None => "not set — not running inside a herdr pane".to_string(),
            },
        },
        DoctorCheck {
            name: "HERDR_PANE_ID",
            ok: snap.pane_id.is_some(),
            detail: match &snap.pane_id {
                Some(id) => format!("pane {id}"),
                None => "not set".to_string(),
            },
        },
        DoctorCheck {
            name: "HERDR_SOCKET_PATH",
            ok: snap.socket_path.is_some(),
            detail: snap
                .socket_path
                .clone()
                .unwrap_or_else(|| "not set".to_string()),
        },
        DoctorCheck {
            name: "socket reachable",
            ok: snap.socket_path.is_some() && socket_reachable,
            detail: match &snap.socket_path {
                Some(_) if socket_reachable => "connected".to_string(),
                Some(p) => format!("could not connect to {p}"),
                None => "no HERDR_SOCKET_PATH to test".to_string(),
            },
        },
        DoctorCheck {
            name: "herdr binary",
            ok: bin_resolvable,
            detail: format!(
                "{} ({})",
                snap.herdr_bin(),
                if bin_resolvable {
                    "resolvable"
                } else {
                    "not found"
                }
            ),
        },
        // Informational, never failing: HERDR_AGENT is optional — without
        // it the argv0 agent hint is derived from the run command.
        DoctorCheck {
            name: "HERDR_AGENT",
            ok: true,
            detail: match &snap.agent {
                Some(agent) => format!("set to {agent:?} (argv0 agent hint)"),
                None => "not set (optional — agent hint derived from the run command)".to_string(),
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prologue used to start a `socat UNIX-LISTEN:… TCP:<gateway>:…`
    /// bridge, which is why custom Containerfiles needed `socat` and why a
    /// missing one degraded the sandbox to "no herdr CLI here". The relay
    /// socket is mounted straight in now; if this ever reappears, the
    /// README's Containerfile contract silently changed with it.
    #[test]
    fn bootstrap_has_no_socat_bridge() {
        assert!(
            !BOOTSTRAP.contains("socat"),
            "the bridge no longer runs socat in the guest"
        );
        assert!(
            !BOOTSTRAP.contains("PALL8T_HERDR_PORT") && !BOOTSTRAP.contains("/proc/net/route"),
            "no TCP port and no gateway lookup: the transport is the mounted socket"
        );
        assert!(
            BOOTSTRAP.contains("/opt/pall8t/bin") && BOOTSTRAP.trim().ends_with("exec \"$@\""),
            "what remains is the PATH prepend and the exec into the real command"
        );
    }

    #[test]
    fn maybe_override_for_herdr_table() {
        let tmux_cmd = vec![
            "tmux".to_string(),
            "new".to_string(),
            "-A".to_string(),
            "-s".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        ];
        assert_eq!(
            maybe_override_for_herdr(tmux_cmd.clone(), true),
            vec!["claude".to_string()],
            "tmux-wrapped + herdr active -> overridden to plain claude"
        );
        assert_eq!(
            maybe_override_for_herdr(tmux_cmd.clone(), false),
            tmux_cmd,
            "tmux-wrapped + herdr inactive -> unchanged"
        );

        let plain = vec!["codex".to_string()];
        assert_eq!(
            maybe_override_for_herdr(plain.clone(), true),
            plain,
            "non-tmux command + herdr active -> unchanged"
        );

        assert_eq!(
            maybe_override_for_herdr(Vec::new(), true),
            Vec::<String>::new(),
            "empty command + herdr active -> unchanged (no first element to check)"
        );
    }

    fn provenance() -> Provenance {
        Provenance {
            image: "pall8t-x:501-20-abc123456789".to_string(),
            container: "pall8t-x-abc12345-99".to_string(),
        }
    }

    #[test]
    fn report_metadata_argv_shape() {
        assert!(
            report_metadata_argv("p1", "codex", &provenance())
                .contains(&"codex (pall8t)".to_string()),
            "the agent name is interpolated, not hardcoded"
        );
        let argv = report_metadata_argv("p1", "claude", &provenance());
        assert_eq!(
            argv,
            vec![
                "pane",
                "report-metadata",
                "p1",
                "--source",
                "user:pall8t",
                "--display-agent",
                "claude (pall8t)",
                "--token",
                "pall8t_image=pall8t-x:501-20-abc123456789",
                "--token",
                "pall8t_container=pall8t-x-abc12345-99",
            ],
            "no --agent: herdr's display_agent guard requires it to match \
             effective_agent_label(), which is host-process-name-derived — \
             true only after the argv0 hint kicks in, and this report must \
             not depend on it (confirmed live). The tokens render as \
             $pall8t_image / $pall8t_container in a configured sidebar row"
        );
    }

    #[test]
    fn token_values_survive_herdr_s_limits() {
        assert_eq!(
            token_value("pall8t-x:501-20-abc123456789"),
            "pall8t-x:501-20-abc123456789",
            "an ordinary tag passes through untouched"
        );
        let long = format!("pall8t-{}:501-20-deadbeefcafe", "x".repeat(120));
        let cut = token_value(&long);
        assert_eq!(
            cut.chars().count(),
            80,
            "herdr caps a token value at 80 characters; sending more costs the \
             whole metadata report, sidebar name included"
        );
        assert!(
            cut.ends_with("501-20-deadbeefcafe"),
            "the tail is what distinguishes two panes of the same project — \
             trimming must keep the content hash, not the shared prefix"
        );
        assert!(
            !token_value("tag\nwith\tcontrols").contains('\n'),
            "control characters would make herdr reject the report"
        );
    }

    fn env_with_agent(agent: Option<&str>) -> HerdrEnv {
        HerdrEnv {
            pane_id: "p1".to_string(),
            workspace_id: None,
            tab_id: None,
            socket_path: None,
            bin_path: None,
            agent: agent.map(str::to_string),
        }
    }

    fn cmd(toks: &[&str]) -> Vec<String> {
        toks.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn agent_hint_precedence_table() {
        assert_eq!(
            agent_hint(&env_with_agent(Some("claude")), &cmd(&["codex"])),
            Some("codex".to_string()),
            "the command wins over an ambient HERDR_AGENT: what runs is \
             what the pane is"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(Some("claude")),
                &cmd(&["sh", "-c", "claude --continue"])
            ),
            Some("claude".to_string()),
            "HERDR_AGENT rescues commands the parser gives up on"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(Some("claude")),
                &cmd(&["./claude-wrapper.sh"])
            ),
            Some("claude".to_string()),
            "an unrecognized wrapper name doesn't shadow the explicit \
             HERDR_AGENT escape hatch"
        );
    }

    #[test]
    fn agent_hint_derivation_table() {
        assert_eq!(
            agent_hint(&env_with_agent(None), &cmd(&["claude"])),
            Some("claude".to_string())
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["/usr/local/bin/claude", "--continue"])
            ),
            Some("claude".to_string()),
            "path is reduced to its basename"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["env", "FOO=1", "codex", "--yolo"])
            ),
            Some("codex".to_string()),
            "env and VAR=VAL prefixes are looked through"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["env", "-u", "NODE_OPTIONS", "claude"])
            ),
            Some("claude".to_string()),
            "flag values (NODE_OPTIONS) can't be mistaken for the agent — \
             only KNOWN_AGENTS names ever match"
        );
        assert_eq!(
            agent_hint(&env_with_agent(None), &cmd(&["uv", "run", "claude"])),
            Some("claude".to_string()),
            "arbitrary launchers are looked through without modeling them"
        );
        assert_eq!(
            agent_hint(&env_with_agent(None), &cmd(&["npx", "claude@latest"])),
            Some("claude".to_string()),
            "@version package-spec suffixes are stripped"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["npx", "@anthropic-ai/claude-code"])
            ),
            Some("claude-code".to_string()),
            "scoped package specs reduce to the package basename, which \
             herdr knows as a claude alias"
        );
    }

    #[test]
    fn agent_hint_no_guess_table() {
        assert_eq!(
            agent_hint(&env_with_agent(None), &cmd(&["python3.11", "agent.py"])),
            None,
            "no recognized agent name anywhere -> no hint, no guess"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["env", "HOME=/home/claude", "codex"])
            ),
            Some("codex".to_string()),
            "an argument path whose basename is an agent name must not \
             mislabel the pane — later tokens never reduce to a basename"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["env", "FOO=1", "/usr/local/bin/claude"])
            ),
            None,
            "the cost of the rule above: a path-shaped program after a \
             launcher is not derived either — HERDR_AGENT covers this shape"
        );
        assert_eq!(
            agent_hint(
                &env_with_agent(None),
                &cmd(&["sh", "-c", "claude --continue"])
            ),
            None,
            "a shell's script is a single opaque token, not a name — \
             \"sh\" must not become the pane's agent (HERDR_AGENT rescues \
             this shape, see the precedence table)"
        );
        assert_eq!(
            agent_hint(&env_with_agent(None), &[]),
            None,
            "empty command -> no hint"
        );
        assert_eq!(
            agent_hint(&env_with_agent(None), &cmd(&["env", "FOO=1"])),
            None,
            "no recognized token -> no hint"
        );
    }

    #[test]
    fn cache_verified_table() {
        let dir = std::env::temp_dir().join(format!("pall8t-test-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("herdr");
        let sidecar = dir.join("v.sha256");
        std::fs::write(&bin, b"binary-content").unwrap();

        assert!(
            !cache_verified(&bin, &sidecar),
            "no sidecar record -> unverified (a bare cached file is never trusted)"
        );

        std::fs::write(&sidecar, sha256_file(&bin).unwrap()).unwrap();
        assert!(cache_verified(&bin, &sidecar), "matching record verifies");

        std::fs::write(&sidecar, format!("{}\n", sha256_file(&bin).unwrap())).unwrap();
        assert!(
            cache_verified(&bin, &sidecar),
            "a trailing newline in the record must not break verification"
        );

        std::fs::write(&bin, b"tampered-by-sandbox").unwrap();
        assert!(
            !cache_verified(&bin, &sidecar),
            "content drift (e.g. tampering through the rw mount) is detected"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_reap_run_bin_table() {
        use std::collections::HashSet;
        let grace = std::time::Duration::from_mins(5);
        let old = Some(std::time::Duration::from_mins(10));
        let young = Some(std::time::Duration::from_secs(10));
        let mut live = HashSet::new();
        live.insert("pall8t-x-1".to_string());

        assert!(
            should_reap_run_bin("pall8t-x-2", "pall8t-x-9", &live, old, grace),
            "an old copy for a container no longer running is reaped"
        );
        assert!(
            !should_reap_run_bin("pall8t-x-9", "pall8t-x-9", &live, old, grace),
            "the current run's own copy is never reaped"
        );
        assert!(
            !should_reap_run_bin("pall8t-x-1", "pall8t-x-9", &live, old, grace),
            "a live container's copy is never reaped even if old"
        );
        assert!(
            !should_reap_run_bin("pall8t-x-2", "pall8t-x-9", &live, young, grace),
            "a fresh copy (concurrently launching run, not yet in the \
             container list) is protected by the grace window"
        );
        assert!(
            !should_reap_run_bin("pall8t-x-2", "pall8t-x-9", &live, None, grace),
            "an unreadable age never reaps — err toward keeping"
        );
    }

    #[test]
    fn doctor_checks_report_optional_agent_hint() {
        let checks = doctor_checks(&DoctorSnapshot::default(), false, false);
        let agent = checks.iter().find(|c| c.name == "HERDR_AGENT").unwrap();
        assert!(agent.ok, "HERDR_AGENT is optional and must never fail");
        assert!(agent.detail.contains("not set"));

        let snap = DoctorSnapshot {
            agent: Some("codex".to_string()),
            ..Default::default()
        };
        let checks = doctor_checks(&snap, false, false);
        let agent = checks.iter().find(|c| c.name == "HERDR_AGENT").unwrap();
        assert!(agent.detail.contains("codex"));
    }

    #[test]
    fn doctor_checks_all_pass() {
        let snap = DoctorSnapshot {
            herdr_env: Some("1".to_string()),
            pane_id: Some("p1".to_string()),
            socket_path: Some("/tmp/herdr.sock".to_string()),
            ..Default::default()
        };
        let checks = doctor_checks(&snap, true, true);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
    }

    #[test]
    fn doctor_checks_flags_missing_env() {
        let snap = DoctorSnapshot::default();
        let checks = doctor_checks(&snap, false, false);
        assert!(!checks.iter().any(|c| c.name == "HERDR_ENV" && c.ok));
        assert!(!checks.iter().any(|c| c.name == "HERDR_PANE_ID" && c.ok));
        assert!(!checks.iter().any(|c| c.name == "socket reachable" && c.ok));
        assert!(!checks.iter().any(|c| c.name == "herdr binary" && c.ok));
    }

    #[test]
    fn doctor_checks_unreachable_socket_with_path_set() {
        let snap = DoctorSnapshot {
            herdr_env: Some("1".to_string()),
            pane_id: Some("p1".to_string()),
            socket_path: Some("/tmp/does-not-exist.sock".to_string()),
            ..Default::default()
        };
        let checks = doctor_checks(&snap, false, true);
        let sock = checks
            .iter()
            .find(|c| c.name == "socket reachable")
            .unwrap();
        assert!(!sock.ok);
        assert!(sock.detail.contains("does-not-exist.sock"));
    }

    #[test]
    fn doctor_checks_reports_custom_bin_path() {
        let snap = DoctorSnapshot {
            bin_path: Some("/opt/homebrew/bin/herdr".to_string()),
            ..Default::default()
        };
        let checks = doctor_checks(&snap, false, true);
        let bin = checks.iter().find(|c| c.name == "herdr binary").unwrap();
        assert!(bin.detail.contains("/opt/homebrew/bin/herdr"));
    }
}
