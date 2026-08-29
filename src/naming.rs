//! Names the herdr tab and agent that one `pall8t run` launches in, with
//! the same string (issue #71), so the name a human reads off the tab is
//! the name they can type as a target:
//!
//! ```sh
//! herdr agent prompt foo-2 "run the tests" --wait --until idle
//! ```
//!
//! Without it the only working target is the pane id (`w13:p3`), which
//! changes every run and reads like nothing: tab labels do not resolve as
//! agent targets, and neither do tab ids — only a pane id or a name set
//! by `agent.rename` does (verified on herdr 0.8.2).
//!
//! Opt-in, per `[herdr] auto_rename` — with it unset pall8t renames
//! nothing at all, which is what every version before this one did.
//!
//! # The two halves land in different places
//!
//! **The tab is renamed here**, before the exec: `tab.rename` takes a tab
//! id and needs nothing else, so the human sees the right label from the
//! moment the run starts — even when the pane has no detected agent yet.
//!
//! **The agent cannot be.** `agent.rename` requires the pane to have a
//! *detected* agent, and at this point the pane's agent is still pall8t
//! itself: herdr only recognizes the sandboxed agent once the argv0 hint
//! takes effect (see [`crate::herdr::agent_hint`]), which is *after*
//! pall8t has exec'd into `container run` and can no longer act. So the
//! agent half is handed to a small detached child — the hidden `pall8t
//! herdr name-agent`, [`run_agent_namer`] — which outlives the exec,
//! waits for herdr to detect the agent, and renames it then.
//!
//! Not the relay (the other pall8t-owned process that outlives the exec):
//! no relay exists when `[herdr] sandbox = "off"`, and naming is about
//! herdr's view of the pane, not about the bridge — it has to work in
//! every sandbox mode. The child also keeps the relay a pure forwarder.
//!
//! The two halves are independent: a failing agent rename never stops the
//! tab from being named.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// herdr's own cap on an agent name (`valid_agent_name`, herdr 0.8.2): a
/// leading lowercase letter, then `[a-z0-9-_]`, at most 32 bytes. A name
/// outside it is refused with `invalid_agent_name`, so pall8t derives one
/// that already fits rather than discovering the rule at rename time.
const AGENT_NAME_MAX: usize = 32;

/// How far the collision counter runs before pall8t stops looking for a
/// free name. A bound rather than a loop: every candidate being taken
/// means something is very wrong, and spinning through thousands of
/// `agent.rename` calls to prove it would be worse than one warning.
const COLLISION_TRIES: usize = 20;

/// How long [`run_agent_namer`] waits for herdr to detect an agent in the
/// pane. herdr re-probes an unidentified pane about twice a second, so
/// this is generous by two orders of magnitude — it is sized for the exec
/// being delayed (a first bridged run downloads the Linux herdr CLI on
/// the way there), not for the detection itself.
const AGENT_WAIT: Duration = Duration::from_secs(45);

/// Gap between `agent.get` probes while waiting.
const AGENT_POLL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Deriving the name (pure)
// ---------------------------------------------------------------------------

/// The name before the tab-number suffix: `[herdr] agent_name` if set,
/// else the workspace directory's basename — slugged either way, the same
/// treatment `image::project_base` gives it.
///
/// The configured name is slugged too: herdr refuses anything outside
/// [`AGENT_NAME_MAX`]'s alphabet with `invalid_agent_name`, so passing
/// `agent_name = "API server"` through verbatim would turn a setting the
/// user typed into a rename that silently never happens.
pub fn base_name(configured: Option<&str>, workspace_dir: &Path) -> String {
    let raw = configured.map_or_else(
        || {
            workspace_dir.file_name().map_or_else(
                || "workspace".to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        },
        str::to_string,
    );
    let slug = crate::util::slug(&raw);
    // herdr wants a lowercase letter first. Trimming the leading digits
    // instead would mangle a real name (`3d-engine` -> `d-engine`) and
    // erase a numeric one entirely (`2048`), so they are kept and the
    // prefix is what makes the name legal.
    if slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        slug
    } else {
        format!("p-{slug}")
    }
}

/// The tab's `number` out of a tab id — `w13:t3` -> `3`.
///
/// This is deliberately *not* the tab's position: `number` is assigned
/// from a monotonic counter when the tab is created and encoded straight
/// into the tab id (`public_tab_id_for_number` is
/// `format!("{workspace_id}:t{number}")`), so closing or moving tabs never
/// renumbers it and a name fixed at launch keeps matching its tab. The
/// position is a different number entirely, and only
/// [`tab_is_auto_named`] wants that one.
pub fn tab_number(tab_id: &str) -> Option<usize> {
    tab_id.rsplit_once(':')?.1.strip_prefix('t')?.parse().ok()
}

/// `name`, cut to `budget` characters and re-trimmed of trailing dashes
/// (a cut landing inside a run of them would leave `foo--2`). Counted in
/// `char`s the way [`crate::util::slug`] produces them — ASCII, so the
/// byte count herdr checks agrees. `"p"` when nothing survives: a name
/// herdr refuses outright is worse than a placeholder letter.
fn capped_to(name: &str, budget: usize) -> String {
    let cut = name
        .chars()
        .take(budget)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string();
    if cut.is_empty() {
        "p".to_string()
    } else {
        cut
    }
}

/// `<name>-<counter>`, with the *name* shortened if the pair would exceed
/// [`AGENT_NAME_MAX`] — the counter is what makes the name unique, so it
/// is never the part that gets cut.
pub fn with_counter(name: &str, counter: usize) -> String {
    let tail = format!("-{counter}");
    let head = capped_to(name, AGENT_NAME_MAX.saturating_sub(tail.len()));
    format!("{head}{tail}")
}

/// The names to try, in order: `<base>-<tab number>` first, then that
/// same name extended with a further counter (`foo-2`, `foo-2-2`,
/// `foo-2-3`, …) for as long as [`COLLISION_TRIES`] allows.
///
/// A pane whose tab id pall8t can't read (no `HERDR_TAB_ID`, or a shape
/// this version doesn't parse) gets the bare base — a name without the
/// suffix still beats a pane id, and the collision counter still makes it
/// unique.
pub fn candidates(base: &str, tab_number: Option<usize>) -> Vec<String> {
    let first = match tab_number {
        Some(n) => with_counter(base, n),
        None => capped_to(base, AGENT_NAME_MAX),
    };
    let mut out = vec![first.clone()];
    out.extend((2..=COLLISION_TRIES).map(|k| with_counter(&first, k)));
    out
}

/// The first of [`candidates`] no live agent already answers to, so
/// `herdr agent prompt <name>` always resolves to exactly one agent.
/// With every candidate taken, the last one is returned anyway: the
/// rename then fails with herdr's own `agent_name_taken`, which says more
/// than a name pall8t declined to try.
pub fn first_free(base: &str, tab_number: Option<usize>, taken: &BTreeSet<String>) -> String {
    let names = candidates(base, tab_number);
    let fallback = names
        .last()
        .cloned()
        .unwrap_or_else(|| capped_to(base, AGENT_NAME_MAX));
    names
        .into_iter()
        .find(|n| !taken.contains(n))
        .unwrap_or(fallback)
}

// ---------------------------------------------------------------------------
// Reading herdr's answers (pure)
// ---------------------------------------------------------------------------

/// The fields of herdr's `TabInfo` this module reads. Unknown fields are
/// ignored (serde's default), so a newer herdr adding to the shape can't
/// break the parse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct TabRow {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
}

#[derive(serde::Deserialize)]
struct Envelope<T> {
    result: T,
}

#[derive(serde::Deserialize)]
struct TabListResult {
    tabs: Vec<TabRow>,
}

#[derive(serde::Deserialize)]
struct AgentListResult {
    agents: Vec<AgentRow>,
}

#[derive(serde::Deserialize)]
struct AgentRow {
    #[serde(default)]
    name: Option<String>,
}

/// `herdr tab list` -> the tabs it reported, or `None` when the output
/// isn't the shape this version knows.
pub fn parse_tabs(stdout: &str) -> Option<Vec<TabRow>> {
    serde_json::from_str::<Envelope<TabListResult>>(stdout)
        .ok()
        .map(|e| e.result.tabs)
}

/// `herdr agent list` -> the names live agents already answer to.
/// Unparseable output yields no names: the worst case is picking one that
/// is taken, which the rename itself then rejects and retries past.
pub fn parse_agent_names(stdout: &str) -> BTreeSet<String> {
    serde_json::from_str::<Envelope<AgentListResult>>(stdout)
        .ok()
        .map(|e| e.result.agents.into_iter().filter_map(|a| a.name).collect())
        .unwrap_or_default()
}

/// Whether the tab still carries the label herdr gave it, and may
/// therefore be renamed — a label a human chose must never be clobbered.
///
/// herdr's auto label is *not* the tab number: `tab_display_name` is
/// `custom_name.unwrap_or_else(|| (tab_idx + 1).to_string())`, i.e. the
/// tab's **position** in its workspace, which shifts when tabs are closed
/// or moved. herdr knows the answer exactly (`Tab::is_auto_named`) but
/// does not serialize it — `TabInfo` carries `label` and `number` only —
/// so a client has to infer it, and the test is `label == position`, not
/// `label == number`. Conflating the two is the easy bug here: `w13:t2`
/// can sit at position 1, where its auto label reads `"1"`.
///
/// `None` when the tab isn't in the list at all; the one false positive
/// is a human who renamed a tab to exactly its own position string.
pub fn tab_is_auto_named(tabs: &[TabRow], tab_id: &str) -> Option<bool> {
    let row = tabs.iter().find(|t| t.tab_id == tab_id)?;
    let position = tabs
        .iter()
        .filter(|t| t.workspace_id == row.workspace_id)
        .position(|t| t.tab_id == tab_id)?
        + 1;
    Some(row.label == position.to_string())
}

/// Whether a failed `herdr agent rename` failed *because the name is
/// taken* — the one error worth retrying under another name.
///
/// Read out of the error text rather than a parsed body because
/// [`crate::util::run_ok`] is this crate's one contract for a CLI call,
/// and it embeds herdr's stderr (the error JSON, verbatim) in its message.
/// A second CLI-call contract would buy nothing else here.
pub fn is_name_taken(err: &str) -> bool {
    err.contains(r#""code":"agent_name_taken""#)
}

/// The one line `pall8t run` prints about naming, or `None` when it named
/// nothing. Says which halves actually happened: a run that names the
/// agent but leaves a human-labeled tab alone must not claim the tab.
pub fn announcement(name: &str, tab: bool, agent: bool) -> Option<String> {
    match (tab, agent) {
        (false, false) => None,
        (true, true) => Some(format!(
            "pall8t: herdr: naming this tab and its agent {name:?}"
        )),
        (true, false) => Some(format!(
            "pall8t: herdr: naming this tab {name:?} — no agent name was derived \
             from the run command, so herdr will have no agent here to name"
        )),
        // Either a label a human chose, or a pane whose tab id pall8t
        // never saw — in both cases the label is left as it stands, and
        // in neither is it pall8t's to claim.
        (false, true) => Some(format!(
            "pall8t: herdr: naming this agent {name:?} — leaving the tab's \
             label as it is"
        )),
    }
}

// ---------------------------------------------------------------------------
// Acting on it
// ---------------------------------------------------------------------------

/// Everything [`name_pane`] needs about one `pall8t run`.
pub struct Request<'a> {
    /// The herdr CLI to call — the host's own, via `HERDR_BIN_PATH`.
    pub herdr_bin: &'a str,
    pub pane_id: &'a str,
    /// `HERDR_TAB_ID`. Absent means no tab to rename and no suffix.
    pub tab_id: Option<&'a str>,
    /// The workspace directory, whose basename is the default name.
    pub workspace_dir: &'a Path,
    pub cfg: &'a crate::config::HerdrConfig,
    /// Whether an agent hint was derived for this run
    /// ([`crate::herdr::agent_hint`]). Without one herdr never recognizes
    /// an agent in the pane, so there would be nothing for the agent half
    /// to rename and it isn't started.
    pub expect_agent: bool,
}

/// Names the pane's tab, and arranges for its agent to be named once
/// herdr detects it. Best-effort throughout, like the rest of the herdr
/// integration: every failure warns and the run continues.
pub fn name_pane(req: &Request<'_>) {
    if !req.cfg.auto_rename {
        return;
    }
    let base = base_name(req.cfg.agent_name.as_deref(), req.workspace_dir);
    let name = first_free(
        &base,
        req.tab_id.and_then(tab_number),
        &live_agent_names(req.herdr_bin),
    );

    let mut renamed_tab = None;
    if let Some(tab_id) = req
        .tab_id
        .filter(|id| tab_is_pall8t_to_name(req.herdr_bin, id))
    {
        match rename_tab(req.herdr_bin, tab_id, &name) {
            Ok(()) => renamed_tab = Some(tab_id),
            Err(e) => eprintln!("pall8t: warning: could not name the herdr tab: {e:#}"),
        }
    }

    let named_agent = req.expect_agent
        && match spawn_agent_namer(req, &name, renamed_tab) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("pall8t: warning: could not start the herdr agent namer: {e:#}");
                false
            }
        };

    if let Some(msg) = announcement(&name, renamed_tab.is_some(), named_agent) {
        eprintln!("{msg}");
    }
}

/// Names live agents already answer to. A failed call yields none rather
/// than blocking the run: picking a taken name only costs the agent half
/// one rejected rename, which [`run_agent_namer`] retries past.
fn live_agent_names(bin: &str) -> BTreeSet<String> {
    crate::util::run_ok(bin, &["agent".to_string(), "list".to_string()])
        .map(|out| parse_agent_names(&out))
        .unwrap_or_default()
}

/// Whether this tab is pall8t's to label — the only `tab.list` call the
/// feature makes. A list pall8t can't read leaves the label alone: not
/// knowing whether a human named the tab is a reason to keep their name,
/// not to overwrite it.
fn tab_is_pall8t_to_name(bin: &str, tab_id: &str) -> bool {
    match crate::util::run_ok(bin, &["tab".to_string(), "list".to_string()]) {
        Ok(out) => parse_tabs(&out)
            .and_then(|tabs| tab_is_auto_named(&tabs, tab_id))
            .unwrap_or(false),
        Err(e) => {
            eprintln!(
                "pall8t: warning: could not read the herdr tab list ({e:#}) — \
                 leaving this tab's label alone"
            );
            false
        }
    }
}

fn rename_tab(bin: &str, tab_id: &str, label: &str) -> Result<()> {
    crate::util::run_ok(
        bin,
        &[
            "tab".to_string(),
            "rename".to_string(),
            tab_id.to_string(),
            label.to_string(),
        ],
    )?;
    Ok(())
}

fn rename_agent(bin: &str, pane_id: &str, name: &str) -> Result<()> {
    crate::util::run_ok(
        bin,
        &[
            "agent".to_string(),
            "rename".to_string(),
            pane_id.to_string(),
            name.to_string(),
        ],
    )?;
    Ok(())
}

/// Where [`run_agent_namer`] reports what it did — it runs after the exec,
/// where the pane's terminal belongs to the agent's UI and a stray
/// `eprintln!` would land in the middle of it.
fn log_path() -> Result<PathBuf> {
    let dir = crate::config::pall8t_root()?.join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("herdr-naming.log"))
}

/// Spawns the detached `pall8t herdr name-agent` child that owns the
/// agent half. It inherits this process's environment on purpose: that is
/// where `HERDR_SOCKET_PATH` lives, which is how the herdr CLI it calls
/// finds the session.
fn spawn_agent_namer(req: &Request<'_>, name: &str, tab_id: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the pall8t binary")?;
    let log = log_path()?;
    let mut argv = vec![
        "herdr".to_string(),
        "name-agent".to_string(),
        "--pane".to_string(),
        req.pane_id.to_string(),
        "--name".to_string(),
        name.to_string(),
        "--herdr-bin".to_string(),
        req.herdr_bin.to_string(),
        "--log".to_string(),
        log.to_string_lossy().into_owned(),
    ];
    // Only when pall8t owns the label: if a collision forces a different
    // name, the child re-labels the tab so the two still match.
    if let Some(tab) = tab_id {
        argv.push("--tab".to_string());
        argv.push(tab.to_string());
    }
    std::process::Command::new(exe)
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot spawn the herdr agent namer")?;
    Ok(())
}

/// Body of the hidden `pall8t herdr name-agent`: wait for herdr to detect
/// the sandboxed agent in `pane_id`, then name it.
///
/// Always `Ok`: this is the process that runs *after* `pall8t run` has
/// exec'd away, so there is no caller left to report to — every outcome
/// goes to the log instead.
///
/// Hidden, not blocked, and unlike `pall8t herdr relay` it validates no
/// paths: everything it does — appending to a log the caller named,
/// renaming an agent and a tab — the caller can already do directly, with
/// a shell redirect and `herdr agent rename`. The relay checks its
/// `--listen` directory because it *chmods and unlinks* one; there is no
/// such action here to guard.
pub fn run_agent_namer(
    bin: &str,
    pane_id: &str,
    name: &str,
    tab_id: Option<&str>,
    log: &Path,
) -> Result<()> {
    // The parent is the `pall8t run` that spawned this child — the same
    // pid, once it has exec'd into the `container` client. Sampled first
    // thing, for the reason `util::spawning_run_alive` explains.
    // SAFETY: getppid cannot fail and has no preconditions.
    let parent = unsafe { libc::getppid() };
    name_agent(bin, pane_id, name, tab_id, log, parent);
    // Outlive the run rather than exiting the moment the work is done.
    // This process is a child of a `pall8t run` that has since become the
    // `container` client, and that client reaps no children of ours — an
    // early exit would leave a `<defunct>` entry for the whole session.
    // Waiting the run out instead means init reaps this process the
    // moment the run ends, which is also how the relay behaves.
    crate::util::wait_out_spawning_run(parent);
    Ok(())
}

/// The naming itself, split out so [`run_agent_namer`] is left holding
/// only the process-lifetime concern.
fn name_agent(
    bin: &str,
    pane_id: &str,
    name: &str,
    tab_id: Option<&str>,
    log: &Path,
    parent: libc::pid_t,
) {
    match wait_for_agent(
        || agent_detected(bin, pane_id),
        || crate::util::spawning_run_alive(parent),
        Instant::now() + AGENT_WAIT,
        AGENT_POLL,
    ) {
        Wait::RunEnded => {
            note(log, pane_id, "the run ended before herdr detected an agent");
            return;
        }
        Wait::TimedOut => {
            note(
                log,
                pane_id,
                &format!(
                    "gave up after {}s: herdr never detected an agent in this pane",
                    AGENT_WAIT.as_secs()
                ),
            );
            return;
        }
        Wait::Detected => {}
    }

    match rename_with_retries(name, |attempt| rename_agent(bin, pane_id, attempt)) {
        Named::Ok(got) => {
            note(log, pane_id, &format!("named the agent {got:?}"));
            relabel_tab(bin, tab_id, name, &got, log);
        }
        Named::Refused(detail) => {
            note(log, pane_id, &format!("could not name the agent: {detail}"));
        }
        Named::AllTaken => note(
            log,
            pane_id,
            &format!("every name from {name:?} on was already taken"),
        ),
    }
}

/// How the wait for herdr to detect an agent ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Detected,
    /// The `pall8t run` that spawned the wait is gone, so no agent will
    /// ever appear in this pane.
    RunEnded,
    TimedOut,
}

/// Polls `detected` until it says yes, the run ends, or the deadline
/// passes. The probes are arguments because the real ones are IO — a
/// `herdr agent get` spawn and a `getppid` — and the loop's decisions are
/// the part worth pinning: an agent herdr hasn't recognized *yet* must be
/// waited for, not treated as absent (that is the whole reason this runs
/// after the exec at all).
fn wait_for_agent(
    mut detected: impl FnMut() -> bool,
    mut run_alive: impl FnMut() -> bool,
    deadline: Instant,
    poll: Duration,
) -> Wait {
    loop {
        if detected() {
            return Wait::Detected;
        }
        // Checked after `detected`, so an agent that appeared in the same
        // breath as the run ending is still named.
        if !run_alive() {
            return Wait::RunEnded;
        }
        if Instant::now() >= deadline {
            return Wait::TimedOut;
        }
        std::thread::sleep(poll);
    }
}

/// What became of the agent rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// The name the agent ended up with — `name` itself, or an extension
    /// of it if a collision forced one.
    Ok(String),
    /// herdr refused for a reason another name would not fix.
    Refused(String),
    /// Every name from the requested one on is taken.
    AllTaken,
}

/// Renames the agent, walking the collision counter when herdr says the
/// name is taken.
///
/// The scan `pall8t run` did before the exec already skipped every name
/// that was taken *then*; this covers the window since — another run that
/// picked the same free name in the meantime. Any other error stops
/// immediately: retrying `agent_not_found` under a different name would
/// only fail nineteen more times.
fn rename_with_retries(name: &str, mut rename: impl FnMut(&str) -> Result<()>) -> Named {
    let mut attempt = name.to_string();
    for counter in 2..=COLLISION_TRIES {
        match rename(&attempt) {
            Ok(()) => return Named::Ok(attempt),
            Err(e) if is_name_taken(&format!("{e:#}")) => attempt = with_counter(name, counter),
            Err(e) => return Named::Refused(format!("{e:#}")),
        }
    }
    Named::AllTaken
}

/// Keeps the tab label equal to the name the agent actually got, when a
/// collision forced a different one. Only ever called with a `tab_id`
/// pall8t itself labeled (see [`spawn_agent_namer`]), so this never
/// touches a label a human chose.
fn relabel_tab(bin: &str, tab_id: Option<&str>, wanted: &str, got: &str, log: &Path) {
    let Some(tab_id) = tab_id.filter(|_| got != wanted) else {
        return;
    };
    match rename_tab(bin, tab_id, got) {
        Ok(()) => note(log, tab_id, &format!("relabeled the tab {got:?}")),
        Err(e) => note(log, tab_id, &format!("could not relabel the tab: {e:#}")),
    }
}

fn agent_detected(bin: &str, pane_id: &str) -> bool {
    crate::util::run_ok(
        bin,
        &["agent".to_string(), "get".to_string(), pane_id.to_string()],
    )
    .is_ok()
}

/// One line per outcome, appended. Best-effort: a log that can't be
/// written must not turn into a failed rename.
fn note(log: &Path, subject: &str, message: &str) {
    use std::io::Write;
    let line = format!("{} {subject} {message}\n", crate::util::epoch_secs());
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `herdr tab list` on herdr 0.8.2, for a workspace whose tab
    /// **numbers and positions disagree** — the case the whole auto-label
    /// inference turns on (issue #71): `w13:t2` was created second but
    /// sits first, so its auto label reads `"1"`, not `"2"`.
    ///
    /// Shape derived from herdr 0.8.2's own types rather than captured
    /// from a live server (this suite never runs one, per
    /// docs/testing.md): `ResponseResult::TabList { tabs: Vec<TabInfo> }`,
    /// internally tagged `type`, re-serialized by the CLI through
    /// `serde_json::Value` — which sorts keys alphabetically, as the
    /// captured `tab rename` output in issue #71 shows.
    const TAB_LIST: &str = r#"{"id":"cli:tab:list","result":{"tabs":[{"agent_status":"unknown","focused":false,"label":"1","number":2,"pane_count":1,"tab_id":"w13:t2","workspace_id":"w13"},{"agent_status":"idle","focused":true,"label":"api","number":3,"pane_count":2,"tab_id":"w13:t3","workspace_id":"w13"},{"agent_status":"unknown","focused":false,"label":"1","number":9,"pane_count":1,"tab_id":"w14:t9","workspace_id":"w14"}],"type":"tab_list"}}"#;

    /// `herdr agent list`, same provenance: `AgentInfo::name` is optional
    /// and omitted when the agent has none (`skip_serializing_if`), which
    /// is the common case this parse must not trip over.
    const AGENT_LIST: &str = r#"{"id":"cli:agent:list","result":{"agents":[{"agent":"claude","agent_status":"idle","focused":false,"name":"foo-2","pane_id":"w13:p3","tab_id":"w13:t3","terminal_id":"t1","workspace_id":"w13"},{"agent":"codex","agent_status":"working","focused":false,"pane_id":"w13:p5","tab_id":"w13:t2","terminal_id":"t2","workspace_id":"w13"}],"type":"agent_list"}}"#;

    #[test]
    fn base_name_slugs_the_basename_and_the_override_alike() {
        let dir = std::path::Path::new("/Users/x/src/My Project");
        assert_eq!(
            base_name(None, dir),
            "my-project",
            "the default is the workspace directory's basename, slugged the \
             way image tags already slug it"
        );
        assert_eq!(
            base_name(Some("api"), dir),
            "api",
            "[herdr] agent_name overrides the basename"
        );
        assert_eq!(
            base_name(Some("API Server"), dir),
            "api-server",
            "the override is slugged too — herdr refuses anything else with \
             invalid_agent_name, which would make the setting silently do nothing"
        );
        assert_eq!(
            base_name(None, std::path::Path::new("/Users/x/src/3d-engine")),
            "p-3d-engine",
            "herdr requires a leading lowercase letter; prefixing keeps the \
             name, where trimming the digits would leave \"d-engine\""
        );
        assert_eq!(
            base_name(None, std::path::Path::new("/")),
            "workspace",
            "a directory with no basename still has to produce a legal name"
        );
    }

    #[test]
    fn tab_number_reads_the_number_out_of_the_id_not_the_position() {
        assert_eq!(tab_number("w13:t3"), Some(3));
        assert_eq!(
            tab_number("w13:t2"),
            Some(2),
            "the id carries `number`, which never changes when tabs are \
             closed or moved — that is why the suffix is taken from here"
        );
        assert_eq!(tab_number("w13"), None, "no tab part at all");
        assert_eq!(
            tab_number("w13:p3"),
            None,
            "a pane id is not a tab id: `p3` must not read as tab 3"
        );
        assert_eq!(
            tab_number("tab_7"),
            None,
            "an id shape this version doesn't know yields no suffix rather \
             than a wrong one"
        );
    }

    #[test]
    fn a_name_never_outgrows_herdrs_32_byte_cap() {
        assert_eq!(with_counter("foo", 2), "foo-2");
        let long = "a".repeat(40);
        let named = with_counter(&long, 12);
        assert_eq!(
            named.len(),
            AGENT_NAME_MAX,
            "herdr refuses a longer name outright ({named})"
        );
        assert!(
            named.ends_with("-12"),
            "the counter is what makes the name unique, so the *name* is what \
             gets cut: {named}"
        );
        assert_eq!(
            with_counter("foo---", 2),
            "foo-2",
            "a cut landing inside a run of dashes must not leave foo---2"
        );
        assert_eq!(
            with_counter("-", 2),
            "p-2",
            "even a name that cuts away to nothing has to stay legal"
        );
    }

    #[test]
    fn candidates_suffix_the_tab_number_then_extend_that_whole_name() {
        let names = candidates("foo", Some(2));
        assert_eq!(
            &names[..3],
            ["foo-2", "foo-2-2", "foo-2-3"],
            "the collision counter extends the *suffixed* name, so the tab \
             number stays readable in every candidate"
        );
        assert_eq!(
            candidates("foo", None)[0],
            "foo",
            "with no tab id there is no suffix — a bare name still beats a \
             pane id"
        );
        assert_eq!(
            candidates("foo", Some(3))[0],
            "foo-3",
            "a different tab number is a different first candidate"
        );
    }

    #[test]
    fn first_free_skips_the_names_live_agents_already_answer_to() {
        let taken: BTreeSet<String> = ["foo-2".to_string(), "foo-2-2".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            first_free("foo", Some(2), &taken),
            "foo-2-3",
            "two runs whose names would collide must end up addressable \
             separately"
        );
        assert_eq!(
            first_free("foo", Some(3), &taken),
            "foo-3",
            "a taken name in another tab says nothing about this one"
        );
        assert_eq!(
            first_free("foo", Some(2), &BTreeSet::new()),
            "foo-2",
            "with nothing taken the plain name is used"
        );
    }

    #[test]
    fn tab_list_parses_and_the_auto_label_test_uses_position_not_number() {
        let tabs = parse_tabs(TAB_LIST).expect("herdr 0.8.2's tab.list shape");
        assert_eq!(tabs.len(), 3);
        assert_eq!(
            tab_is_auto_named(&tabs, "w13:t2"),
            Some(true),
            "tab number 2 sits at position 1 and is labeled \"1\" — still \
             herdr's own label, so pall8t may rename it. Comparing the label \
             against the *number* would call this tab human-named"
        );
        assert_eq!(
            tab_is_auto_named(&tabs, "w13:t3"),
            Some(false),
            "a label a human chose is never clobbered"
        );
        assert_eq!(
            tab_is_auto_named(&tabs, "w14:t9"),
            Some(true),
            "position is counted within the tab's own workspace: w14:t9 is \
             first in w14 and labeled \"1\", though it is third in the list \
             as a whole — counting across workspaces would expect \"3\" \
             here and call the tab human-named"
        );
        assert_eq!(
            tab_is_auto_named(&tabs, "w99:t1"),
            None,
            "a tab herdr didn't list is not a tab to rename"
        );
        assert!(
            parse_tabs("not json").is_none() && parse_tabs(r#"{"error":{}}"#).is_none(),
            "an error reply or garbage is not a tab list"
        );
    }

    #[test]
    fn agent_list_yields_the_names_that_are_taken() {
        let names = parse_agent_names(AGENT_LIST);
        assert_eq!(
            names,
            ["foo-2".to_string()].into_iter().collect::<BTreeSet<_>>(),
            "only agents that actually have a name occupy one — the unnamed \
             one is addressable by pane id alone and blocks nothing"
        );
        assert!(
            parse_agent_names("not json").is_empty(),
            "unreadable output must not look like a name that is taken"
        );
    }

    #[test]
    fn only_a_taken_name_is_worth_another_try() {
        // The literal herdr error, as `util::run_ok` hands it on: its
        // message embeds the CLI's stderr verbatim.
        let taken = r#"`herdr agent rename w13:p3 foo-2` failed: {"id":"cli:agent:rename","error":{"code":"agent_name_taken","message":"agent name foo-2 is already used; candidates: terminal_id=t1 pane_id=w13:p3 workspace_id=w13 tab_id=w13:t3 cwd=/x status=Idle"}}"#;
        let missing = r#"`herdr agent rename w13:p3 foo-2` failed: {"id":"cli:agent:rename","error":{"code":"agent_not_found","message":"agent target w13:p3 not found"}}"#;
        assert!(is_name_taken(taken));
        assert!(
            !is_name_taken(missing),
            "a pane with no agent yet is not a name collision — retrying \
             under a different name would never help"
        );
    }

    #[test]
    fn an_agent_herdr_has_not_recognized_yet_is_waited_for_not_dropped() {
        let mut probes = 0;
        let wait = wait_for_agent(
            || {
                probes += 1;
                // herdr only recognizes the sandboxed agent once the argv0
                // hint takes effect, which is after `pall8t run` has exec'd
                // away — so the first probes legitimately say "no agent".
                probes >= 3
            },
            || true,
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(1),
        );
        assert_eq!(wait, Wait::Detected);
        assert_eq!(
            probes, 3,
            "it kept asking rather than giving up on the first no"
        );

        assert_eq!(
            wait_for_agent(
                || false,
                || false,
                Instant::now() + Duration::from_mins(1),
                Duration::from_millis(1),
            ),
            Wait::RunEnded,
            "a run that is over will never produce an agent — waiting out the \
             full deadline would leave a process polling a dead pane"
        );
        assert_eq!(
            // A deadline already reached: `>=` makes now itself past due.
            wait_for_agent(|| false, || true, Instant::now(), Duration::from_millis(1),),
            Wait::TimedOut,
            "and the wait is bounded even while the run is alive"
        );
    }

    #[test]
    fn a_name_taken_since_the_scan_is_extended_but_another_error_is_not() {
        let taken = |name: &str| {
            format!(
                r#"`herdr agent rename w13:p3 {name}` failed: {{"id":"cli:agent:rename","error":{{"code":"agent_name_taken","message":"agent name {name} is already used; candidates: terminal_id=t1"}}}}"#
            )
        };
        let mut tried = Vec::new();
        let got = rename_with_retries("foo-2", |name| {
            tried.push(name.to_string());
            if tried.len() < 3 {
                anyhow::bail!("{}", taken(name))
            }
            Ok(())
        });
        assert_eq!(
            got,
            Named::Ok("foo-2-3".to_string()),
            "another run taking the name between the pre-exec scan and now \
             must still leave this pane addressable"
        );
        assert_eq!(
            tried,
            ["foo-2", "foo-2-2", "foo-2-3"],
            "and the counter extends the whole name, so the tab number stays \
             readable: {tried:?}"
        );

        let mut calls = 0;
        let got = rename_with_retries("foo-2", |_| {
            calls += 1;
            anyhow::bail!(
                r#"failed: {{"id":"cli:agent:rename","error":{{"code":"agent_not_found","message":"agent target w13:p3 not found"}}}}"#
            )
        });
        assert_eq!(
            calls, 1,
            "a pane with no agent is not a name collision — \
             retrying under another name would only fail again"
        );
        assert!(matches!(got, Named::Refused(_)), "{got:?}");

        let all_taken = rename_with_retries("foo-2", |name| anyhow::bail!("{}", taken(name)));
        assert_eq!(
            all_taken,
            Named::AllTaken,
            "and the walk is bounded: every candidate being taken is reported, \
             not looped on"
        );
    }

    #[test]
    fn the_announcement_claims_only_the_halves_that_happened() {
        assert!(
            announcement("foo-2", false, false).is_none(),
            "nothing to say"
        );
        let both = announcement("foo-2", true, true).unwrap();
        assert!(both.contains("tab") && both.contains("agent") && both.contains("foo-2"));
        let agent_only = announcement("foo-2", false, true).unwrap();
        assert!(
            !agent_only.contains("naming this tab") && agent_only.contains("leaving the tab"),
            "a run that leaves the tab's label alone — a human's, or one on a \
             tab pall8t never saw the id of — must not claim to have named \
             it: {agent_only}"
        );
        assert!(
            announcement("foo-2", true, false)
                .unwrap()
                .contains("no agent"),
            "and one with no derivable agent must say why only the tab was named"
        );
    }
}
