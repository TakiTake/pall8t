//! Bridges the host↔container boundary for [herdr](https://github.com/ogulcancelik/herdr).
//! herdr injects `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`/
//! `HERDR_BIN_PATH` into the pane process it spawns — which is `pall8t`
//! itself, on the host. None of that is visible to `claude` once `pall8t
//! run` execs into the sandboxed container, so any herdr-facing action has
//! to happen here, before the exec (see `main.rs::exec_container`).

use anyhow::Result;

/// A herdr pane's identity, as seen by the host-side `pall8t` process.
pub struct HerdrEnv {
    pub pane_id: String,
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
pub fn announce_pane_identity(env: &HerdrEnv, command: &[String]) -> Option<String> {
    let agent = agent_hint(env, command)?;
    // The command wins over HERDR_AGENT (see agent_hint); say so when they
    // disagree, or the shadowed env var is undebuggable.
    if let Some(env_agent) = env.agent.as_deref().filter(|a| *a != agent) {
        eprintln!(
            "pall8t: note: herdr pane agent is {agent:?} (from the run \
             command); HERDR_AGENT={env_agent:?} ignored"
        );
    }
    if let Err(e) = report_metadata(env, &agent) {
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

fn report_metadata_argv(pane_id: &str, agent: &str) -> Vec<String> {
    vec![
        "pane".into(),
        "report-metadata".into(),
        pane_id.into(),
        "--source".into(),
        "user:pall8t".into(),
        "--display-agent".into(),
        format!("{agent} (pall8t)"),
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
fn report_metadata(env: &HerdrEnv, agent: &str) -> Result<()> {
    let argv = report_metadata_argv(&env.pane_id, agent);
    crate::util::run_ok(env.herdr_bin(), &argv)?;
    Ok(())
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

    #[test]
    fn report_metadata_argv_shape() {
        assert!(
            report_metadata_argv("p1", "codex").contains(&"codex (pall8t)".to_string()),
            "the agent name is interpolated, not hardcoded"
        );
        let argv = report_metadata_argv("p1", "claude");
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
            ],
            "no --agent: herdr's display_agent guard requires it to match \
             effective_agent_label(), which is host-process-name-derived — \
             true only after the argv0 hint kicks in, and this report must \
             not depend on it (confirmed live)"
        );
    }

    fn env_with_agent(agent: Option<&str>) -> HerdrEnv {
        HerdrEnv {
            pane_id: "p1".to_string(),
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
