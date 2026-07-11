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
}

impl HerdrEnv {
    fn herdr_bin(&self) -> &str {
        self.bin_path.as_deref().unwrap_or("herdr")
    }
}

/// `HERDR_ENV=1` plus a usable `HERDR_PANE_ID` — anything less isn't a
/// herdr pane worth acting on.
pub fn detect() -> Option<HerdrEnv> {
    if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
        return None;
    }
    let pane_id = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(HerdrEnv {
        pane_id,
        socket_path: std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .filter(|s| !s.is_empty()),
        bin_path: std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|s| !s.is_empty()),
    })
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

fn report_metadata_argv(pane_id: &str) -> Vec<String> {
    vec![
        "pane".into(),
        "report-metadata".into(),
        pane_id.into(),
        "--source".into(),
        "user:pall8t".into(),
        "--display-agent".into(),
        "claude (pall8t)".into(),
    ]
}

/// Best-effort sidebar identity: `herdr pane report-metadata <pane_id>
/// --source user:pall8t --display-agent "claude (pall8t)"`, so herdr's UI
/// makes it clear the agent is sandboxed. Deliberately omits `--agent`:
/// herdr's guard for showing `display_agent` requires it to match
/// `effective_agent_label()`, which herdr derives from the HOST's own
/// process tree (`identify_agent_in_job` in herdr's `detect` module) — and
/// the host only ever sees the `container` client process, never the
/// sandboxed `claude` running inside the VM, so that match can never
/// succeed. Confirmed live: with `--agent claude` set, `herdr pane get`
/// never surfaces `display_agent`; omitting it, the field shows up
/// immediately. Callers must not fail the run on `Err` — this is cosmetic,
/// not load-bearing.
pub fn report_metadata(env: &HerdrEnv) -> Result<()> {
    crate::util::run_ok(env.herdr_bin(), &report_metadata_argv(&env.pane_id))?;
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
}

impl DoctorSnapshot {
    pub fn from_process_env() -> Self {
        DoctorSnapshot {
            herdr_env: std::env::var("HERDR_ENV").ok(),
            pane_id: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            socket_path: std::env::var("HERDR_SOCKET_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
            bin_path: std::env::var("HERDR_BIN_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
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
        let argv = report_metadata_argv("p1");
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
             effective_agent_label(), which is host-process-name-derived and \
             can never be \"claude\" for a sandboxed pane (confirmed live)"
        );
    }

    #[test]
    fn doctor_checks_all_pass() {
        let snap = DoctorSnapshot {
            herdr_env: Some("1".to_string()),
            pane_id: Some("p1".to_string()),
            socket_path: Some("/tmp/herdr.sock".to_string()),
            bin_path: None,
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
            bin_path: None,
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
