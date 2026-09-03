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
/// being delayed, not for the detection itself.
///
/// The window opens early on purpose: [`name_pane`] runs *ahead* of
/// [`crate::herdr::prepare_bridge`] (naming must work with the bridge
/// off), so the child is already waiting while a first bridged run
/// downloads the Linux herdr CLI and `container run` boots the VM. Those
/// two, not detection, are what this has to cover.
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
fn base_name(configured: Option<&str>, workspace_dir: &Path) -> String {
    // No basename (`/`) leaves `slug` an empty string, whose documented
    // answer is already "workspace" — restating that default here would
    // give it a second home.
    let raw = configured.map_or_else(
        || {
            workspace_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
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

/// The tab's 1-based position within its own workspace's tab list — the
/// same number herdr's own default label already shows on an unrenamed
/// tab (`tab_display_name` is `custom_name.unwrap_or_else(|| (tab_idx +
/// 1).to_string())`), so pall8t's suffix now reads the same number a human
/// would see if pall8t had done nothing here (issue #76: the id-encoded
/// counter used before this could start anywhere above 1 — whichever count
/// the workspace's tabs had already reached — which read as unintuitive
/// next to herdr's own numbering).
///
/// Recomputed fresh on every call: closing or moving an earlier tab in the
/// workspace changes this number for every tab after it, which is exactly
/// why [`label_is_pall8t_own`] no longer trusts a single computed value
/// alone — see its doc comment.
fn tab_position(tabs: &[TabRow], tab_id: &str) -> Option<usize> {
    let row = tabs.iter().find(|t| t.tab_id == tab_id)?;
    tabs.iter()
        .filter(|t| t.workspace_id == row.workspace_id)
        .position(|t| t.tab_id == tab_id)
        .map(|i| i + 1)
}

/// [`crate::util::cut_at`] plus herdr's own requirement: `"p"` when
/// nothing survives, because a name herdr refuses outright is worse than
/// a placeholder letter.
fn capped_to(name: &str, budget: usize) -> String {
    let cut = crate::util::cut_at(name, budget);
    if cut.is_empty() {
        "p".to_string()
    } else {
        cut
    }
}

/// `<name>-<counter>`, with the *name* shortened if the pair would exceed
/// [`AGENT_NAME_MAX`] — the counter is what makes the name unique, so it
/// is never the part that gets cut.
fn with_counter(name: &str, counter: usize) -> String {
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
fn candidates(base: &str, tab_number: Option<usize>) -> impl Iterator<Item = String> {
    let first = first_candidate(base, tab_number);
    std::iter::once(first.clone())
        .chain((2..=COLLISION_TRIES).map(move |k| with_counter(&first, k)))
}

/// The name before any collision counter — the one every other candidate
/// extends.
fn first_candidate(base: &str, tab_number: Option<usize>) -> String {
    match tab_number {
        Some(n) => with_counter(base, n),
        None => capped_to(base, AGENT_NAME_MAX),
    }
}

/// How far past the current position [`label_is_pall8t_own`] searches for
/// a number this label could have been written under before some earlier
/// tab in the workspace closed and shifted every later position down.
/// Pure string building, no IO — a few thousand short-lived allocations at
/// most, negligible next to the `tab.list`/`agent.list` calls this run
/// already makes — but not padded past that: a wider search also widens
/// the one false positive this function accepts (a human's label that
/// happens to look like pall8t's own scheme, at *some* number rather than
/// only the exact one), so the bound stays close to what a real herdr
/// workspace's tab count could plausibly have reached, not a much bigger
/// margin the risk doesn't need.
const OWN_LABEL_SEARCH_BOUND: usize = 50;

/// Whether `label` is one pall8t itself could have written in this tab —
/// at its current position, or at any position up to
/// [`OWN_LABEL_SEARCH_BOUND`].
///
/// Recognising its own past label is what keeps the tab and the agent from
/// drifting apart on a *second* run in the same tab. After run 1 the label
/// reads `foo-2`, which is no longer herdr's auto label, so
/// [`tab_is_auto_named`] alone would call it a human's and leave it — while
/// run 2, finding `foo-2` taken by run 1's successor, names its agent
/// `foo-2-2`. The tab would then advertise a name that reaches somebody
/// else's agent.
///
/// Checking only the *current* position isn't enough once the suffix is
/// [`tab_position`] rather than a stable id-derived counter (issue #76):
/// closing an earlier tab in the workspace shifts this tab's position, so
/// a label written at position 3 must still be recognized once the tab
/// becomes position 2 — otherwise every such reorder reintroduces the same
/// drift this function exists to prevent.
///
/// The one false positive is a human who typed exactly a name pall8t would
/// have picked here, at some position; overwriting `foo-2` with `foo-2-2`
/// in that case is benign, since it stays inside the same name family.
///
/// The direct check on `tab_number` only earns its keep for `None` (the
/// bare, unsuffixed candidates the sweep below never visits, since it
/// always supplies a number) or a position beyond the bound — any `Some(n)`
/// with `n <= OWN_LABEL_SEARCH_BOUND` is already one of the sweep's own
/// iterations, so running it twice would just duplicate that work.
///
/// `n > OWN_LABEL_SEARCH_BOUND` vs. `n >= OWN_LABEL_SEARCH_BOUND` is one
/// case `cargo mutants` will keep flagging and no test here can close:
/// at exactly `n == OWN_LABEL_SEARCH_BOUND` the sweep already answers for
/// this label either way, so which side of `>`/`>=` the boundary falls on
/// changes nothing about what the function returns — a genuinely
/// equivalent mutant, not a coverage gap.
fn label_is_pall8t_own(label: &str, base: &str, tab_number: Option<usize>) -> bool {
    if tab_number.is_none_or(|n| n > OWN_LABEL_SEARCH_BOUND)
        && candidates(base, tab_number).any(|c| c == label)
    {
        return true;
    }
    (1..=OWN_LABEL_SEARCH_BOUND).any(|n| candidates(base, Some(n)).any(|c| c == label))
}

/// The first of [`candidates`] no live agent already answers to, so
/// `herdr agent prompt <name>` always resolves to exactly one agent.
/// With every candidate taken, the last one is returned anyway: the
/// rename then fails with herdr's own `agent_name_taken`, which says more
/// than a name pall8t declined to try.
fn first_free(base: &str, tab_number: Option<usize>, taken: &BTreeSet<String>) -> String {
    let mut last = first_candidate(base, tab_number);
    for name in candidates(base, tab_number) {
        if !taken.contains(&name) {
            return name;
        }
        last = name;
    }
    last
}

// ---------------------------------------------------------------------------
// Reading herdr's answers (pure)
// ---------------------------------------------------------------------------

/// The fields of herdr's `TabInfo` this module reads. Unknown fields are
/// ignored (serde's default), so a newer herdr adding to the shape can't
/// break the parse.
#[derive(serde::Deserialize)]
struct TabRow {
    tab_id: String,
    workspace_id: String,
    label: String,
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
fn parse_tabs(stdout: &str) -> Option<Vec<TabRow>> {
    serde_json::from_str::<Envelope<TabListResult>>(stdout)
        .ok()
        .map(|e| e.result.tabs)
}

/// `herdr agent list` -> the names live agents already answer to.
/// Unparseable output yields no names: the worst case is picking one that
/// is taken, which the rename itself then rejects and retries past.
fn parse_agent_names(stdout: &str) -> BTreeSet<String> {
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
fn tab_is_auto_named(tabs: &[TabRow], tab_id: &str) -> Option<bool> {
    let row = tabs.iter().find(|t| t.tab_id == tab_id)?;
    let position = tab_position(tabs, tab_id)?;
    Some(row.label == position.to_string())
}

/// What this run's tab label is, as far as `tab.list` can tell.
enum TabLabel {
    /// pall8t's to overwrite: herdr's own auto label, or one pall8t wrote
    /// here itself.
    Ours,
    /// Somebody else's. Carried so the announcement can name it.
    Theirs(String),
    /// No usable answer: the call failed, the reply's shape changed, or
    /// this tab isn't in the list.
    Unknown,
}

/// Whether a failed `herdr agent rename` failed *because the name is
/// taken* — the one error worth retrying under another name.
///
/// Read out of the error text rather than a parsed body because
/// [`crate::util::run_ok`] is this crate's one contract for a CLI call,
/// and it embeds herdr's stderr (the error JSON, verbatim) in its message.
/// A second CLI-call contract would buy nothing else here.
fn is_name_taken(err: &str) -> bool {
    err.contains(r#""code":"agent_name_taken""#)
}

/// Whether this tab's label is pall8t's to overwrite: herdr's own auto
/// label ([`tab_is_auto_named`], which computes its own copy of the tab's
/// position for that one self-contained check), or one pall8t wrote here
/// itself ([`label_is_pall8t_own`], given `number` — the position
/// [`inspect_tab`] already computed, passed in rather than recomputed a
/// second time here).
///
/// [`TabLabel::Unknown`] when the tab isn't in the list at all — not
/// knowing is a reason to keep the label, not to overwrite it.
fn tab_label_of(tabs: &[TabRow], tab_id: &str, base: &str, number: Option<usize>) -> TabLabel {
    let Some(row) = tabs.iter().find(|t| t.tab_id == tab_id) else {
        return TabLabel::Unknown;
    };
    if tab_is_auto_named(tabs, tab_id) == Some(true)
        || label_is_pall8t_own(&row.label, base, number)
    {
        TabLabel::Ours
    } else {
        TabLabel::Theirs(row.label.clone())
    }
}

/// The one line `pall8t run` prints about naming, or `None` when it named
/// nothing. Says which halves actually happened: a run that names the
/// agent but leaves a human-labeled tab alone must not claim the tab.
///
/// The agent half is only ever *arranged* here — a detached child does the
/// rename once herdr detects the agent, and can still fail (the pane never
/// becomes an agent pane, every candidate is taken). So the wording promises
/// a request, not an outcome, and points at the log that records which it
/// was. `kept_label` is the foreign label being left in place, when there is
/// one: naming it is what lets a human see that the tab and the agent no
/// longer match.
fn announcement(name: &str, tab: bool, agent: bool, kept_label: Option<&str>) -> Option<String> {
    let log = "~/.pall8t/logs/herdr-naming.log";
    match (tab, agent) {
        (false, false) => None,
        (true, true) => Some(format!(
            "pall8t: herdr: naming this tab {name:?}; its agent takes the same \
             name once herdr detects it ({log})"
        )),
        (true, false) => Some(format!(
            "pall8t: herdr: naming this tab {name:?} — no agent name was derived \
             from the run command, so herdr will have no agent here to name"
        )),
        // Either a label a human chose, or a pane whose tab id pall8t
        // never saw — in both cases the label is left as it stands, and
        // in neither is it pall8t's to claim. Say what it stays as: the
        // tab and the agent then read differently, and a human addressing
        // this agent has to type the agent's name, not the tab's.
        (false, true) => Some(match kept_label {
            Some(label) => format!(
                "pall8t: herdr: naming this agent {name:?} once herdr detects it \
                 ({log}) — this tab keeps its label {label:?}, so address the \
                 agent as {name:?}, not {label:?}"
            ),
            None => format!(
                "pall8t: herdr: naming this agent {name:?} once herdr detects it \
                 ({log}) — leaving the tab's label as it is"
            ),
        }),
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

    // Whose the label is, and this tab's current position-based number,
    // both come from the one `tab.list` call — a run with no tab of its
    // own to name and no agent coming has nothing to spend that round
    // trip on.
    let insight = req
        .tab_id
        .map(|tab_id| (tab_id, inspect_tab(req.herdr_bin, tab_id, &base)));
    // Position needs a live `tab.list` answer; a transient failure there
    // must not also cost the agent its numbered name, which the old
    // id-parsed number never depended on any herdr call for (review
    // finding on this diff) — so a missing position still falls back to
    // parsing the id directly.
    let number = resolved_number(insight.as_ref().and_then(|(_, i)| i.number), req.tab_id);
    let mut kept_label = None;
    let to_label = match insight {
        Some((
            tab_id,
            TabInsight {
                label: TabLabel::Ours,
                ..
            },
        )) => Some(tab_id),
        Some((
            _,
            TabInsight {
                label: TabLabel::Theirs(label),
                ..
            },
        )) => {
            kept_label = Some(label);
            None
        }
        _ => None,
    };
    if to_label.is_none() && !req.expect_agent {
        return;
    }

    let name = first_free(&base, number, &live_agent_names(req.herdr_bin));
    let mut renamed_tab = None;
    if let Some(tab_id) = to_label {
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

    if let Some(msg) = announcement(
        &name,
        renamed_tab.is_some(),
        named_agent,
        kept_label.as_deref(),
    ) {
        eprintln!("{msg}");
    }
}

/// [`crate::util::run_ok`] bound to the herdr CLI, so the call sites read
/// as the commands they are. `container::run_ok` is the same adapter for
/// the other program this crate drives.
fn herdr<const N: usize>(bin: &str, args: [&str; N]) -> Result<String> {
    crate::util::run_ok(bin, &args.map(str::to_string))
}

/// Names live agents already answer to. A failed call yields none rather
/// than blocking the run: picking a taken name only costs the agent half
/// one rejected rename, which [`run_agent_namer`] retries past.
fn live_agent_names(bin: &str) -> BTreeSet<String> {
    herdr(bin, ["agent", "list"])
        .map(|out| parse_agent_names(&out))
        .unwrap_or_default()
}

/// What the one `tab.list` call the feature makes tells [`name_pane`]: this
/// tab's current position-based number ([`tab_position`]) and whose the
/// label is ([`tab_label_of`]). A failed or unparseable call yields neither
/// (see [`fetch_tabs`]) — [`resolved_number`] is what falls back for the
/// suffix when that happens.
struct TabInsight {
    number: Option<usize>,
    label: TabLabel,
}

fn inspect_tab(bin: &str, tab_id: &str, base: &str) -> TabInsight {
    let Some(tabs) = fetch_tabs(bin) else {
        return TabInsight {
            number: None,
            label: TabLabel::Unknown,
        };
    };
    // Computed once and threaded into `tab_label_of` rather than left for
    // it to recompute — both read the same tab's position from the same
    // `tabs` snapshot, so there is nothing a second lookup would learn.
    let number = tab_position(&tabs, tab_id);
    TabInsight {
        number,
        label: tab_label_of(&tabs, tab_id, base, number),
    }
}

/// The number encoded in a herdr tab id (`w13:t3` -> `3`) — the id's own
/// creation-order counter, not its position. Used only as a fallback for
/// [`resolved_number`]: unlike [`tab_position`], it needs no `tab.list`
/// call at all, so it is what keeps a transient herdr failure from costing
/// the agent its numbered name entirely.
fn tab_number_from_id(tab_id: &str) -> Option<usize> {
    tab_id.rsplit_once(':')?.1.strip_prefix('t')?.parse().ok()
}

/// The number this run's suffix uses: `position` (herdr's own
/// position-based numbering, issue #76) when it's known, else whatever
/// [`tab_number_from_id`] can parse straight out of the tab id.
///
/// Review finding on this diff: before the fallback, a `tab.list` call
/// that failed or simply hadn't caught up to a brand-new tab yet (both
/// transient) left `position` `None` and so dropped the suffix from the
/// agent's name entirely — a regression, since the old id-derived number
/// needed no herdr call and so survived exactly this failure. The
/// fallback only ever fires here, for the *numbering*; whether the tab
/// itself gets relabeled still depends on a successful `tab.list` call
/// either way ([`TabLabel::Unknown`] leaves it untouched, unaffected by
/// this function).
fn resolved_number(position: Option<usize>, tab_id: Option<&str>) -> Option<usize> {
    position.or_else(|| tab_id.and_then(tab_number_from_id))
}

/// `herdr tab list`, parsed. `None` (with a warning already printed) when
/// the call failed or the reply's shape changed — a reply that parsed as
/// JSON but not as tabs means herdr's `TabInfo` moved under this version.
/// Silence there would turn tab naming off while the run still told the
/// user their own label was respected, so it warns, unlike a tab simply
/// not being in the list (which the announcement already covers: pall8t
/// never saw this tab's id).
fn fetch_tabs(bin: &str) -> Option<Vec<TabRow>> {
    let out = match herdr(bin, ["tab", "list"]) {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "pall8t: warning: could not read the herdr tab list ({e:#}) — \
                 leaving this tab's label alone"
            );
            return None;
        }
    };
    let tabs = parse_tabs(&out);
    if tabs.is_none() {
        eprintln!(
            "pall8t: warning: could not make sense of the herdr tab list — \
             leaving this tab's label alone"
        );
    }
    tabs
}

fn rename_tab(bin: &str, tab_id: &str, label: &str) -> Result<()> {
    herdr(bin, ["tab", "rename", tab_id, label]).map(drop)
}

fn rename_agent(bin: &str, pane_id: &str, name: &str) -> Result<()> {
    herdr(bin, ["agent", "rename", pane_id, name]).map(drop)
}

/// Where [`run_agent_namer`] reports what it did — it runs after the exec,
/// where the pane's terminal belongs to the agent's UI and a stray
/// `eprintln!` would land in the middle of it.
fn log_path() -> Result<PathBuf> {
    Ok(crate::config::logs_dir()?.join("herdr-naming.log"))
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
    // thing, for the reason `util::spawning_run` explains.
    let parent = crate::util::spawning_run();
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
enum Wait {
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
enum Named {
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
    // The same walk the pre-exec scan offered, from the same builder —
    // `name` arrives from `first_free` already capped and dash-trimmed, so
    // it is `candidates`' own first element. Sharing it is what keeps the
    // two from drifting: restating the range here once tried one name
    // fewer than `candidates` offers.
    for attempt in candidates(name, None) {
        match rename(&attempt) {
            Ok(()) => return Named::Ok(attempt),
            Err(e) if is_name_taken(&format!("{e:#}")) => {}
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
    crate::util::append_line(
        log,
        &format!("{} {subject} {message}\n", crate::util::epoch_secs()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `herdr tab list` on herdr 0.8.2, for a workspace whose tab
    /// **numbers and positions disagree** — the case the whole auto-label
    /// inference turns on (issue #71): `w13:t2` was created second but
    /// sits first, so its auto label reads `"1"`, not `"2"`.
    ///
    /// Field for field the envelope a live herdr 0.8.2 session answers
    /// `herdr tab list` with — `ResponseResult::TabList { tabs:
    /// Vec<TabInfo> }`, internally tagged `type`, re-serialized by the CLI
    /// through `serde_json::Value`, which is what sorts the keys
    /// alphabetically. The *arrangement* is constructed rather than
    /// captured, because one session rarely holds every case at once: two
    /// workspaces, so per-workspace position counting is actually
    /// exercised. The disagreement itself is not hypothetical — a live
    /// session produced tab `w17:t4` with `number = 4` sitting at position
    /// 3, auto-labeled `"3"`.
    const TAB_LIST: &str = r#"{"id":"cli:tab:list","result":{"tabs":[{"agent_status":"unknown","focused":false,"label":"1","number":2,"pane_count":1,"tab_id":"w13:t2","workspace_id":"w13"},{"agent_status":"idle","focused":true,"label":"api","number":3,"pane_count":2,"tab_id":"w13:t3","workspace_id":"w13"},{"agent_status":"unknown","focused":false,"label":"1","number":9,"pane_count":1,"tab_id":"w14:t9","workspace_id":"w14"}],"type":"tab_list"}}"#;

    /// `herdr agent list`, same provenance: `AgentInfo::name` is optional
    /// and omitted when the agent has none (`skip_serializing_if`), which
    /// is the common case this parse must not trip over — confirmed on a
    /// live session, where an unnamed agent's object carries no `name` key
    /// at all rather than a null.
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
    fn tab_position_reads_the_position_not_the_id_derived_number() {
        let tabs = parse_tabs(TAB_LIST).expect("herdr 0.8.2's tab.list shape");
        assert_eq!(
            tab_position(&tabs, "w13:t2"),
            Some(1),
            "tab id number 2 sits first in w13 — pall8t's suffix now reads \
             the same 1-based position herdr's own default label already \
             shows (issue #76), not the id-encoded creation counter"
        );
        assert_eq!(
            tab_position(&tabs, "w13:t3"),
            Some(2),
            "second in w13 despite carrying id number 3"
        );
        assert_eq!(
            tab_position(&tabs, "w14:t9"),
            Some(1),
            "positions are counted within the tab's own workspace, so a \
             high id-encoded number (9) doesn't stop this from being the \
             first tab in w14"
        );
        assert_eq!(
            tab_position(&tabs, "w99:t1"),
            None,
            "a tab herdr didn't list has no position"
        );
    }

    #[test]
    fn tab_number_from_id_reads_the_id_encoded_counter() {
        assert_eq!(tab_number_from_id("w13:t3"), Some(3));
        assert_eq!(tab_number_from_id("w13"), None, "no tab part at all");
        assert_eq!(
            tab_number_from_id("w13:p3"),
            None,
            "a pane id is not a tab id: `p3` must not read as tab 3"
        );
        assert_eq!(
            tab_number_from_id("tab_7"),
            None,
            "an id shape this version doesn't know yields no number rather \
             than a wrong one"
        );
    }

    /// Regression pin (review finding on this diff): before `resolved_number`
    /// existed, a `tab.list` failure left `number` at `None` outright,
    /// dropping the agent's numbered name entirely — a regression from the
    /// old id-parsed number, which needed no herdr call and so survived
    /// exactly this failure.
    #[test]
    fn resolved_number_falls_back_to_the_id_when_position_is_unknown() {
        assert_eq!(
            resolved_number(Some(1), Some("w13:t9")),
            Some(1),
            "a known position always wins — it's what herdr's own default \
             label already shows (issue #76)"
        );
        assert_eq!(
            resolved_number(None, Some("w13:t9")),
            Some(9),
            "no position (a `tab.list` failure, or the tab missing from its \
             reply) falls back to the id, rather than dropping the suffix"
        );
        assert_eq!(
            resolved_number(None, None),
            None,
            "no tab id at all leaves nothing to fall back to, same as before"
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
        let names: Vec<String> = candidates("foo", Some(2)).collect();
        assert_eq!(
            &names[..3],
            ["foo-2", "foo-2-2", "foo-2-3"],
            "the collision counter extends the *suffixed* name, so the tab \
             number stays readable in every candidate"
        );
        assert_eq!(
            first_candidate("foo", None),
            "foo",
            "with no tab id there is no suffix — a bare name still beats a \
             pane id"
        );
        assert_eq!(
            first_candidate("foo", Some(3)),
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

    /// Regression pin for the review finding that only the *first* run in a
    /// tab was protected from misrouting. Run 1 leaves `foo-2` behind; the
    /// label is then no longer herdr's auto label, so before the fix run 2
    /// read it as a human's and kept it — while naming its own agent
    /// `foo-2-2`, leaving the tab pointing at run 1's successor.
    #[test]
    fn a_tab_pall8t_labeled_itself_is_still_pall8ts_to_relabel() {
        // w13:t1 sits before it, so w13:t2 is at position 2 — herdr's own
        // auto label there would read "2", not the "foo-2" it actually
        // carries.
        let tabs = vec![
            TabRow {
                tab_id: "w13:t1".into(),
                workspace_id: "w13".into(),
                label: "other".into(),
            },
            TabRow {
                tab_id: "w13:t2".into(),
                workspace_id: "w13".into(),
                label: "foo-2".into(),
            },
        ];
        assert_eq!(
            tab_is_auto_named(&tabs, "w13:t2"),
            Some(false),
            "it genuinely no longer carries herdr's auto label — that is what \
             made this look like a human's name"
        );
        assert!(
            matches!(
                tab_label_of(&tabs, "w13:t2", "foo", tab_position(&tabs, "w13:t2")),
                TabLabel::Ours
            ),
            "but `foo-2` is exactly what pall8t itself writes in tab 2 of a \
             `foo` workspace, so it is pall8t's to move to `foo-2-2` rather \
             than a label that must be preserved"
        );
        assert!(
            matches!(
                tab_label_of(&tabs, "w13:t2", "web", tab_position(&tabs, "w13:t2")),
                TabLabel::Theirs(ref l) if l == "foo-2"
            ),
            "after `[herdr] agent_name` changes to `web`, the old `foo-2` \
             label is indistinguishable from a human's — kept, and reported \
             by `announcement`'s kept-label arm instead"
        );
        assert!(
            matches!(
                tab_label_of(&tabs, "w13:t9", "foo", None),
                TabLabel::Unknown
            ),
            "a tab that isn't in the list at all stays unknown, never `Ours`"
        );
    }

    /// Regression pin for issue #76: the suffix is now [`tab_position`],
    /// which — unlike the old id-encoded counter — shifts whenever an
    /// earlier tab in the workspace closes. A label pall8t wrote at one
    /// position must still be recognized as its own after the position
    /// changes, or every such reorder reintroduces the exact misrouting bug
    /// the test above pins.
    #[test]
    fn a_pall8t_label_survives_the_tabs_position_shifting() {
        // Only one tab left in w13 — the ones that used to sit before it
        // (making it position 3 when pall8t wrote this label) have since
        // closed.
        let tabs = vec![TabRow {
            tab_id: "w13:t9".into(),
            workspace_id: "w13".into(),
            label: "foo-3".into(),
        }];
        assert_eq!(
            tab_position(&tabs, "w13:t9"),
            Some(1),
            "the tab's position today, after the reorder"
        );
        assert!(
            matches!(
                tab_label_of(&tabs, "w13:t9", "foo", tab_position(&tabs, "w13:t9")),
                TabLabel::Ours
            ),
            "`foo-3` no longer matches this run's own candidate (`foo-1`), \
             but it is still exactly what pall8t would have written here at \
             an earlier position — recognized, not mistaken for a human's \
             rename"
        );
        assert!(
            matches!(
                tab_label_of(&tabs, "w13:t9", "web", tab_position(&tabs, "w13:t9")),
                TabLabel::Theirs(ref l) if l == "foo-3"
            ),
            "the search widens the *position* it checks, never the *base* — \
             a genuinely different project's label is still left alone"
        );
    }

    /// [`OWN_LABEL_SEARCH_BOUND`] is generous but not infinite: a label
    /// that merely resembles pall8t's naming scheme, at a position far past
    /// any real herdr workspace, is still a human's to keep.
    #[test]
    fn the_own_label_search_has_a_bound() {
        assert!(
            !label_is_pall8t_own("foo-99999", "foo", Some(1)),
            "well past the bound — not recognized"
        );
        assert!(
            label_is_pall8t_own(&format!("foo-{OWN_LABEL_SEARCH_BOUND}"), "foo", Some(1)),
            "right at the bound — still recognized"
        );
    }

    /// `mutation testing pin`: the sweep in [`label_is_pall8t_own`] only
    /// ever tries `1..=OWN_LABEL_SEARCH_BOUND`, so a position *beyond* the
    /// bound is reachable only through the function's direct check on
    /// `tab_number` itself — every other test here uses an in-bound
    /// position, where that guard is redundant with the sweep and so
    /// invisible to `cargo mutants` (a mutated `>`/`&&`/`==` there changes
    /// no test's outcome). This is the one case that actually exercises it.
    #[test]
    fn a_position_beyond_the_bound_is_recognized_only_by_exact_match() {
        let far = OWN_LABEL_SEARCH_BOUND + 5;
        assert!(
            label_is_pall8t_own(&format!("foo-{far}"), "foo", Some(far)),
            "the sweep never reaches this far, so only the direct check on \
             `tab_number` itself can recognize it"
        );
        assert!(
            !label_is_pall8t_own(&format!("foo-{}", far + 1), "foo", Some(far)),
            "and it really is an exact match, not another sweep: a label \
             for a different far-out position isn't recognized just \
             because both are past the bound"
        );
    }

    /// Regression pin: the retry walk and [`candidates`] both claim
    /// [`COLLISION_TRIES`] names, and must actually agree. The old loop
    /// (`for counter in 2..=COLLISION_TRIES` around a mutable `attempt`)
    /// tried 19 and gave up before the 20th name `first_free` would accept.
    #[test]
    fn the_retry_walk_covers_exactly_the_names_candidates_offers() {
        let mut tried: Vec<String> = Vec::new();
        let taken = anyhow::anyhow!(r#"{{"code":"agent_name_taken"}}"#);
        let outcome = rename_with_retries("foo-2", |n| {
            tried.push(n.to_string());
            Err(anyhow::anyhow!("{taken:#}"))
        });
        assert!(matches!(outcome, Named::AllTaken), "every name was refused");
        assert_eq!(
            tried.len(),
            candidates("foo", Some(2)).count(),
            "the walk gives up only after trying as many names as the \
             pre-exec scan would have offered: {tried:?}"
        );
        assert_eq!(
            tried.first().map(String::as_str),
            Some("foo-2"),
            "starting with the name itself"
        );
        assert_eq!(
            tried.last().map(String::as_str),
            Some(with_counter("foo-2", COLLISION_TRIES).as_str()),
            "and ending on the last counter the bound allows, not one short \
             of it: {tried:?}"
        );
    }

    #[test]
    fn the_announcement_claims_only_the_halves_that_happened() {
        assert!(
            announcement("foo-2", false, false, None).is_none(),
            "nothing to say"
        );
        let both = announcement("foo-2", true, true, None).unwrap();
        assert!(both.contains("tab") && both.contains("agent") && both.contains("foo-2"));
        assert!(
            both.contains("once herdr detects it") && both.contains("herdr-naming.log"),
            "the agent half is arranged, not done — a detached child still has \
             to win the rename, so the line must promise the request and point \
             at the log that records the outcome: {both}"
        );
        let agent_only = announcement("foo-2", false, true, None).unwrap();
        assert!(
            !agent_only.contains("naming this tab") && agent_only.contains("leaving the tab"),
            "a run that leaves the tab's label alone — a human's, or one on a \
             tab pall8t never saw the id of — must not claim to have named \
             it: {agent_only}"
        );
        let diverged = announcement("web-2", false, true, Some("api-2")).unwrap();
        assert!(
            diverged.contains("web-2") && diverged.contains("api-2"),
            "when the kept label differs from the agent's name, the human is \
             about to read one and type the other — the line has to show both \
             so that mismatch is visible at the moment it is created: {diverged}"
        );
        assert!(
            announcement("foo-2", true, false, None)
                .unwrap()
                .contains("no agent"),
            "and one with no derivable agent must say why only the tab was named"
        );
    }
}
