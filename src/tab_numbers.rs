//! The number in `<base>-<number>`, the name [`crate::naming`] gives a
//! herdr tab and its agent (issue #76 follow-up).
//!
//! # Why pall8t keeps this itself
//!
//! Two numbers herdr already has were tried first, and neither works:
//!
//! - **The tab's position in its workspace.** It is the number herdr's own
//!   default label shows, which is what made it tempting — but it is a
//!   property of the *list*, not of the tab. Closing an earlier tab shifts
//!   every later one, so a name written yesterday stops meaning this tab,
//!   and a new tab lands on a name an older tab is still wearing (both
//!   observed live).
//! - **The counter in the tab id** (`w1E:t3`). Stable per tab, but herdr
//!   persists it in `~/.config/herdr/session.json` and never decrements it,
//!   so it does not restart with the server and a fresh workspace opens at
//!   whatever count the previous ones reached — issue #76 itself.
//!
//! So the number is pall8t's own: a counter per base name, handed out once
//! and never reused while one herdr server run lasts, reset when a new
//! server takes over. A tab's name is then fixed for the tab's whole life
//! and never moves because some other tab closed.
//!
//! # What identifies "one server run"
//!
//! herdr exposes no session id, pid or start time anywhere — `ping` answers
//! version, protocol and capabilities, and there is no `server.info`. But it
//! unlinks and re-binds its API socket on every start, and keys its own
//! shutdown ownership on that socket's `(dev, ino)`. So the socket's
//! identity *is* the server run's, and pall8t already knows the path: herdr
//! sets `HERDR_SOCKET_PATH` on every pane process it spawns.
//!
//! # Reset is the destructive direction
//!
//! [`keeps_counting`] answers "unknown" with *keep counting*, which is the
//! opposite of what the shape suggests — the crate's other unknown-state
//! rule, [`crate::util::entry_age`], reads unknown as "don't reap". Both are
//! the same principle applied to different branches: don't take the
//! destructive one on a guess. Here resetting is destructive. Keeping a
//! counter that should have restarted costs a cosmetically large number;
//! restarting one that should have kept going hands out a name a live tab
//! already wears, and if the socket stays unreadable it does that on every
//! single run — `foo-1`, `foo-1-2`, `foo-1-3` — which is the failure this
//! module exists to remove.
//!
//! # herdr restores labels, so a reset is not a blank slate
//!
//! `session.json` persists each tab's `custom_name` and restores it
//! verbatim, so after a restart the counter starts over while `foo-3` is
//! still on screen. Two rules keep that coherent: [`Alloc::adopt`] lets a
//! tab keep the number its own label already carries, and
//! [`seed_from_labels`] starts a reset counter past the labels still
//! visible. A genuinely empty session still begins at 1.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Bumped only when the shape changes in a way an older pall8t would
/// misread. A file carrying a *higher* number is never read and never
/// written — see [`Load::Frozen`].
const SCHEMA: u32 = 1;

/// How many herdr sessions' counters are kept. A user alternating between
/// named sessions (`herdr --session foo`) gets one entry each; without a
/// bound the map would grow one entry per socket path ever seen.
const MAX_SESSIONS: usize = 8;

/// Tries at the lock before giving up, and the gap between them — 500 ms
/// in total. Bounded rather than blocking: naming is best-effort
/// everywhere in this crate, and a `pall8t run` that hung behind a lock
/// nobody will release would be the one place it isn't.
const LOCK_TRIES: usize = 20;
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(25);

// ---------------------------------------------------------------------------
// The state file
// ---------------------------------------------------------------------------

/// `~/.pall8t/state/herdr-naming.json`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    version: u32,
    /// One entry per herdr API socket path. Two named herdr sessions are
    /// two independent servers; sharing one entry would make every run
    /// look like a restart of the other session and reset the count on
    /// each alternation.
    sessions: BTreeMap<String, Session>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
struct Session {
    /// Which server run these numbers belong to, or `None` when the socket
    /// could not be read at the time they were written.
    server: Option<ServerRun>,
    /// [`crate::util::epoch_secs`] at the last allocation. Used only to decide which
    /// entry [`evict`] drops.
    last_used: u64,
    /// The next number to hand out, per base name. Never decreases while a
    /// server run lasts: that is the whole promise.
    next: BTreeMap<String, usize>,
    /// The number a tab was already given, so a *second* run in the same
    /// tab keeps the name that tab already advertises.
    tabs: BTreeMap<String, Assigned>,
}

/// The base is recorded next to the number because it qualifies it: when
/// `[herdr] agent_name` changes between two runs in one tab, the recorded
/// number belongs to the old base's sequence and must not be reused under
/// the new one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct Assigned {
    base: String,
    number: usize,
}

/// The identity of one herdr server *run*, read off its API socket.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct ServerRun {
    dev: u64,
    ino: u64,
    /// `st_birthtime`, which tells a re-bound socket apart from one whose
    /// inode the filesystem happened to reuse. `None` where the platform or
    /// filesystem has none — then `(dev, ino)` alone decides, which can only
    /// ever *miss* a restart, never invent one, and missing is the safe
    /// direction (see the module doc). Verified present for a Unix-socket
    /// inode on APFS, macOS 15.
    birth_secs: Option<u64>,
    birth_nanos: Option<u32>,
}

impl ServerRun {
    fn of(meta: &std::fs::Metadata) -> ServerRun {
        use std::os::unix::fs::MetadataExt;
        let birth = meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        ServerRun {
            dev: meta.dev(),
            ino: meta.ino(),
            birth_secs: birth.map(|d| d.as_secs()),
            birth_nanos: birth.map(|d| d.subsec_nanos()),
        }
    }
}

/// The identity of the herdr server listening on `socket`, or `None` when
/// it cannot be read at all — a path herdr never told us, a socket already
/// unlinked, a stat that failed.
fn server_run(socket: &Path) -> Option<ServerRun> {
    std::fs::metadata(socket).ok().map(|m| ServerRun::of(&m))
}

// ---------------------------------------------------------------------------
// The decision (pure)
// ---------------------------------------------------------------------------

/// Everything one allocation needs. Every piece of IO is done by the caller
/// and passed in — the socket stat, the `tab.list` snapshot, the clock — so
/// the decision below is a pure function testable without a herdr, a
/// `$HOME`, or a wall clock.
pub(crate) struct Alloc<'a> {
    /// `HERDR_SOCKET_PATH`, as herdr set it on this pane.
    pub socket_path: Option<&'a str>,
    pub base: &'a str,
    pub tab_id: &'a str,
    /// This tab's current label, when `tab.list` reported one. A label
    /// pall8t could have written itself carries a number this tab may keep
    /// rather than being renumbered — see [`Alloc::adopt`].
    pub own_label: Option<&'a str>,
    /// The tab ids herdr listed, or `None` when that call failed or its
    /// shape changed. Pruning is then skipped, never guessed.
    pub live_tabs: Option<&'a BTreeSet<String>>,
    /// The labels herdr listed, for seeding a counter past what is already
    /// on screen.
    pub live_labels: Option<&'a BTreeSet<String>>,
    pub now: u64,
}

impl Alloc<'_> {
    /// The number this tab's own label already carries, when that label is
    /// one pall8t could have written for this base.
    ///
    /// This is what carries a tab through a server restart intact: herdr
    /// restores `foo-3` from `session.json` while pall8t's counter starts
    /// over, and without this the tab would be renamed to whatever the
    /// fresh counter says. Adopting instead leaves the label alone and
    /// teaches the counter about it.
    fn adopt(&self) -> Option<usize> {
        number_in_label(self.own_label?, self.base)
    }
}

/// Whether the recorded numbers still belong to the server listening now.
///
/// Unknown keeps counting, in both directions — see the module doc for why
/// that is the non-destructive branch here even though the crate's other
/// unknown-state rule reads the opposite way.
fn keeps_counting(recorded: Option<&ServerRun>, live: Option<&ServerRun>) -> bool {
    match (recorded, live) {
        // Two ways of knowing nothing, and the same answer to both. Either
        // the socket cannot be read now — so nothing was learned and nothing
        // is thrown away — or it could not be read when these numbers were
        // written, in which case they are assumed to belong to the run that
        // is listening, rather than a restart invented out of an old
        // ignorance.
        (_, None) | (None, Some(_)) => true,
        (Some(r), Some(l)) => r == l,
    }
}

/// Drops the records of tabs herdr no longer lists.
///
/// `keep` — this run's own tab — always survives. A tab opened moments ago
/// may not be in a snapshot taken since, and forgetting it would hand one
/// tab two different numbers on two runs.
///
/// `next` is deliberately untouched. A base whose every tab has closed keeps
/// its counter: that one omission *is* the promise never to reuse a number
/// while the server run lasts.
fn prune_tabs(session: &mut Session, live: Option<&BTreeSet<String>>, keep: &str) {
    let Some(live) = live else {
        return;
    };
    session
        .tabs
        .retain(|tab_id, _| tab_id == keep || live.contains(tab_id));
}

/// The lowest number a counter may sit at, given what is on screen: one
/// past the highest number the labels already there use for this base, or
/// 1 when none do.
///
/// herdr restores tab labels across a server stop, so a counter that reset
/// blindly to 1 would hand out a name a restored tab is still wearing. This
/// keeps the reset — an empty session matches nothing and starts at 1 —
/// while a restored one continues the sequence a human can see.
///
/// The labels are the *other* tabs' ([`crate::naming::labels_of_other_tabs`]);
/// this tab's own is excluded there so its name stays reusable, and its
/// number reaches the counter through [`Alloc::adopt`] instead.
///
/// Matching is deliberately narrow: [`number_in_label`] rebuilds through the
/// same code that writes these names. A label whose base was shortened to
/// fit herdr's 32-byte cap will not match, and the seed simply comes out
/// lower — which the collision walk in [`crate::naming`] then absorbs, as it
/// does today.
fn seed_from_labels(labels: Option<&BTreeSet<String>>, base: &str) -> usize {
    labels.map_or(1, |labels| {
        labels
            .iter()
            .filter_map(|l| number_in_label(l, base))
            .max()
            .map_or(1, |n| n + 1)
    })
}

/// The number in a name pall8t wrote for `base`, or `None` when the label
/// is not one of those names.
///
/// Rebuilt through [`crate::naming::with_counter`] rather than parsed, so
/// what counts as "a number pall8t put here" can never drift from what
/// pall8t actually writes — including the base shortening that a long name
/// plus its suffix triggers.
fn number_in_label(label: &str, base: &str) -> Option<usize> {
    use crate::naming::{split_counter, with_counter};
    // `<base>-<n>`, the name a run writes when nothing else holds it.
    if let Some((_, n)) = split_counter(label) {
        if with_counter(base, n) == label {
            return Some(n);
        }
    }
    // `<base>-<n>-<k>`, the same name after the collision walk bumped it.
    // The number that identifies the tab is still the first one.
    let (head, _) = split_counter(label)?;
    let (_, n) = split_counter(head)?;
    (with_counter(base, n) == head).then_some(n)
}

/// Keeps `sessions` bounded, dropping the least recently used — never the
/// one being written.
fn evict(state: &mut State, keep: &str) {
    while state.sessions.len() > MAX_SESSIONS {
        let Some(oldest) = state
            .sessions
            .iter()
            .filter(|(k, _)| k.as_str() != keep)
            .min_by_key(|(k, s)| (s.last_used, (*k).clone()))
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        state.sessions.remove(&oldest);
    }
}

/// The bucket a pane's counters belong to. A pane herdr told us no socket
/// path for gets the empty key: its counters are still monotonic, they
/// simply cannot be attributed to a server run, so they never reset.
fn session_key(socket_path: Option<&str>) -> String {
    socket_path.unwrap_or_default().to_string()
}

/// The number this run's name takes, and the state to persist.
///
/// What is recorded is the *number*, never the finished name: a collision
/// walk that turns `foo-3` into `foo-3-2` is a fact about this moment, so
/// the next run in this tab asks for `foo-3` again and converges as soon as
/// whatever held it is gone.
fn allocate(mut state: State, server: Option<&ServerRun>, req: &Alloc<'_>) -> (usize, State) {
    let key = session_key(req.socket_path);
    // A server run that is not the one these numbers were counted under
    // takes `tabs` with it, not just `next`: a surviving tab's old number
    // belongs to a sequence that no longer exists, and keeping it would let
    // the fresh counter hand the same number out a second time.
    let mut session = state
        .sessions
        .remove(&key)
        .filter(|s| keeps_counting(s.server.as_ref(), server))
        .unwrap_or_default();

    prune_tabs(&mut session, req.live_tabs, req.tab_id);

    let number = match session.tabs.get(req.tab_id) {
        // This tab already has a number under this base. Reusing it is what
        // keeps a second run in one tab on the name the tab advertises.
        Some(a) if a.base == req.base => a.number,
        _ => {
            // The counter never goes backwards and never sits on a number
            // the labels on screen already use. Applying the seed on every
            // allocation, not only the first, is what keeps that true after
            // a reset: an adopted number would otherwise leave the counter
            // below a *later* tab's restored label and walk onto it a few
            // tabs from now.
            let seed = seed_from_labels(req.live_labels, req.base);
            let counter = session.next.entry(req.base.to_string()).or_insert(seed);
            *counter = (*counter).max(seed);
            let n = req.adopt().unwrap_or(*counter);
            // Past whatever was handed out, however it was chosen — an
            // adopted number that left the counter alone would be handed
            // out again to the next tab.
            *counter = (*counter).max(n + 1);
            session.tabs.insert(
                req.tab_id.to_string(),
                Assigned {
                    base: req.base.to_string(),
                    number: n,
                },
            );
            n
        }
    };

    session.server = server.cloned().or(session.server);
    session.last_used = req.now;
    state.sessions.insert(key.clone(), session);
    state.version = SCHEMA;
    evict(&mut state, &key);
    (number, state)
}

// ---------------------------------------------------------------------------
// Reading and writing the file
// ---------------------------------------------------------------------------

/// What was on disk.
#[derive(Debug, PartialEq, Eq)]
enum Load {
    /// Nothing usable: absent, unreadable, or not this shape. Numbering
    /// starts over, and the next write replaces whatever is there.
    Fresh,
    Have(State),
    /// Written by a newer pall8t. Carried so the warning can name the
    /// version, and so the writer knows to keep its hands off.
    Frozen(u32),
}

/// Just the field that decides whether today's `State` is the right
/// reader at all. Separate from [`State`] because it has to survive a
/// body that `State` cannot deserialize — which is the whole point of
/// [`Load::Frozen`].
#[derive(Deserialize)]
struct Envelope {
    version: u32,
}

fn parse(text: &str) -> Load {
    // The version comes out of an envelope, before the body. Deciding it
    // from a fully-deserialized `State` only worked while a newer file
    // still happened to fit today's shape: `State`'s fields are required,
    // so a v2 that renamed `sessions`, dropped a field or changed a type
    // deserialized as `Err` → `Fresh` → overwritten. The guard stopped
    // guarding in exactly the case it exists for (issue #89).
    //
    // A file that fails even at the envelope — truncated, not JSON, no
    // `version` — is still legitimately `Fresh`: unreadable is not the
    // same claim as "written by someone newer", and only the second one
    // earns the file protection from our writer.
    match serde_json::from_str::<Envelope>(text) {
        Ok(env) if env.version > SCHEMA => Load::Frozen(env.version),
        Ok(_) => match serde_json::from_str::<State>(text) {
            Ok(s) => Load::Have(s),
            Err(_) => Load::Fresh,
        },
        Err(_) => Load::Fresh,
    }
}

/// [`parse`] plus the one thing a pure function cannot do: tell the user
/// that numbering just started over.
///
/// An absent file is the ordinary first run and says nothing. Anything else
/// — unreadable, truncated, not this shape — produces the same [`Load::Fresh`]
/// but is worth a word, because the visible effect is every name in the
/// project jumping back down the count, and a silent reset would look like
/// pall8t inventing it.
fn read_state(path: &Path) -> Load {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Load::Fresh,
        Err(e) => {
            eprintln!(
                "pall8t: warning: cannot read {} ({e}); starting the herdr tab \
                 numbering over",
                path.display()
            );
            return Load::Fresh;
        }
    };
    let load = parse(&text);
    if matches!(load, Load::Fresh) {
        eprintln!(
            "pall8t: warning: {} is not readable as herdr tab numbering state; \
             starting the numbering over",
            path.display()
        );
    }
    load
}

/// Publishes `state` by the same route as the herdr binary cache
/// (`crate::herdr`): a per-pid temp beside the target, then a rename, so a
/// crash or a full disk leaves the previous complete file rather than a
/// truncated one that would read as [`Load::Fresh`] and reset every counter.
fn write_state(path: &Path, state: &State) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".herdr-naming.{}.json", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// An exclusive `flock(2)` held for the whole read-modify-write.
///
/// The lock and the atomic rename answer different questions and neither
/// replaces the other. Without the lock two parallel `pall8t run` both read
/// `next = 3`, both allocate 3, and the promise never to reuse a number
/// inside one server run is broken — the collision walk papers over the
/// name, but two tabs meant the same thing. Without the rename a crash
/// mid-write leaves a truncated file that reads as fresh. So the lock must
/// still be held when the rename happens; releasing it earlier reintroduces
/// the lost update.
///
/// `flock` and not a lock file's existence, because the kernel drops it when
/// a run is killed mid-allocation — and for the same reason there is no
/// explicit unlock here. An `flock` is released when the last descriptor on
/// the open file description closes, so dropping the file *is* the release;
/// an `LOCK_UN` before it would be a no-op that reads like the thing doing
/// the work.
struct Lock {
    _file: std::fs::File,
}

impl Lock {
    /// Non-blocking, retried up to [`LOCK_TRIES`] times. The hold is
    /// microseconds — invisible next to the `tab.list` and `agent.list`
    /// spawns already on this path — so contention means a stuck holder,
    /// and waiting for one is worse than going unnumbered.
    fn acquire(path: &Path) -> Option<Lock> {
        // Opened, never unlinked. Removing a lock file is the classic race
        // where two processes end up holding locks on two different inodes
        // for one path — worth stating outright, because both reapers in
        // this crate *do* delete what they create under `~/.pall8t`.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        for _ in 0..LOCK_TRIES {
            // SAFETY: `file` owns the fd for the whole call.
            let taken = unsafe {
                libc::flock(
                    std::os::unix::io::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } == 0;
            if taken {
                return Some(Lock { _file: file });
            }
            // Sleeps once more than strictly needed, after the last failed
            // try. Skipping that would mean a branch whose only effect is
            // 25 ms on a path that has already spent half a second failing
            // — untestable by construction, and not worth an untested
            // branch in a lock.
            std::thread::sleep(LOCK_RETRY);
        }
        None
    }
}

/// The number for this run's name, or `None` when the state file could not
/// be used at all.
///
/// Best-effort like the rest of the herdr integration: every failure warns
/// and the run continues. `None` costs the name its suffix, not the run —
/// [`crate::naming`] falls back to the bare base and its collision walk,
/// which still yields a name that resolves to exactly one agent.
pub(crate) fn number_for(req: &Alloc<'_>) -> Option<usize> {
    let dir = match crate::config::state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("pall8t: warning: cannot use the herdr tab numbering state: {e:#}");
            return None;
        }
    };
    let path = dir.join("herdr-naming.json");
    let lock_path = dir.join("herdr-naming.lock");
    let Some(_lock) = Lock::acquire(&lock_path) else {
        eprintln!(
            "pall8t: warning: another pall8t run is holding {}; this tab's number may repeat",
            lock_path.display()
        );
        return None;
    };

    let state = match read_state(&path) {
        Load::Have(s) => s,
        Load::Fresh => State::default(),
        Load::Frozen(v) => {
            // A newer pall8t wrote this — a rollback, or two builds on one
            // machine. Overwriting it would break the other binary's
            // numbering, and this run can do without a number.
            eprintln!(
                "pall8t: warning: {} was written by a newer pall8t (format {v}); \
                 leaving it alone and naming without a number",
                path.display()
            );
            return None;
        }
    };

    let server = req.socket_path.map(Path::new).and_then(server_run);
    let (number, state) = allocate(state, server.as_ref(), req);
    if let Err(e) = write_state(&path, &state) {
        // The number is already decided, so use it. The cost of not
        // recording it is that the next run may hand out the same one,
        // which the collision walk resolves into a distinct name.
        eprintln!(
            "pall8t: warning: could not record herdr tab numbering in {} ({e:#}); \
             numbers will repeat until this is fixed",
            path.display()
        );
    }
    Some(number)
}
#[cfg(test)]
mod tests {
    use super::*;

    const SOCK: &str = "/tmp/herdr-test.sock";

    /// One server run, told apart from another only by its inode — which is
    /// exactly what a re-bound socket changes.
    fn server(ino: u64) -> ServerRun {
        ServerRun {
            dev: 1,
            ino,
            birth_secs: Some(1_700_000_000),
            birth_nanos: Some(0),
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// The common request: this socket, no label of its own, no `tab.list`
    /// answer. Tests override the fields they are about with struct-update
    /// syntax, so each one reads as its own difference from the default.
    fn alloc<'a>(base: &'a str, tab_id: &'a str) -> Alloc<'a> {
        Alloc {
            socket_path: Some(SOCK),
            base,
            tab_id,
            own_label: None,
            live_tabs: None,
            live_labels: None,
            now: 1,
        }
    }

    fn session_of(state: &State) -> &Session {
        &state.sessions[SOCK]
    }

    #[test]
    fn a_number_is_handed_out_once_and_never_reissued_in_one_server_run() {
        let srv = server(1);
        let mut state = State::default();
        for (tab, expected) in [("w:t1", 1), ("w:t2", 2), ("w:t3", 3)] {
            let (n, next) = allocate(state, Some(&srv), &alloc("demo", tab));
            assert_eq!(
                n, expected,
                "each new tab takes the next number, counting from 1 in a \
                 fresh server run"
            );
            state = next;
        }

        // The middle tab closes. Its record goes; the counter does not.
        let live = set(&["w:t1", "w:t3"]);
        let (n, _) = allocate(
            state,
            Some(&srv),
            &Alloc {
                live_tabs: Some(&live),
                ..alloc("demo", "w:t4")
            },
        );
        assert_eq!(
            n, 4,
            "the number a closed tab held is not handed out again while this \
             server run lasts. Deriving the counter from the live tabs \
             (`len + 1`, or `1 + max`) would have produced 3 here and given \
             two tabs the same name in one session"
        );
    }

    #[test]
    fn a_second_run_in_the_same_tab_keeps_the_number_it_was_given() {
        let srv = server(1);
        let (first, state) = allocate(State::default(), Some(&srv), &alloc("demo", "w:t1"));
        let (second, state) = allocate(state, Some(&srv), &alloc("demo", "w:t1"));
        assert_eq!(
            (first, second),
            (1, 1),
            "a rerun in one tab must land on the name that tab already \
             advertises, or every message addressed to the old name misses"
        );
        assert_eq!(
            session_of(&state).next["demo"],
            2,
            "and it consumes nothing: a tab rerun a few times must not push \
             every later tab's number up"
        );
    }

    #[test]
    fn a_different_base_counts_separately_even_in_the_same_tab() {
        let srv = server(1);
        let (_, state) = allocate(State::default(), Some(&srv), &alloc("foo", "w:t1"));
        // The second tab is what makes this test bite: its recorded number
        // is 2, so a reused record and a fresh `web` sequence give
        // different answers. Had the rename happened in `w:t1`, whose
        // record is 1, both would have said 1 and the guard below could
        // have been dropped without a test noticing.
        let (foo, state) = allocate(state, Some(&srv), &alloc("foo", "w:t2"));
        let (web, state) = allocate(state, Some(&srv), &alloc("web", "w:t2"));
        assert_eq!(
            (foo, web),
            (2, 1),
            "`[herdr] agent_name` changing starts that name's own sequence — \
             `web-1` is not taken just because `foo-1` was, and the number \
             this tab held under its old base belongs to the old base's count"
        );
        let (foo_again, _) = allocate(state, Some(&srv), &alloc("foo", "w:t3"));
        assert_eq!(
            foo_again, 3,
            "and the base the tab used to answer to still remembers where it \
             was: a single shared counter would have said 2 here"
        );
    }

    #[test]
    fn a_new_server_run_resets_the_count_and_drops_the_tab_records() {
        let (_, state) = allocate(State::default(), Some(&server(1)), &alloc("demo", "w:t1"));
        let (_, state) = allocate(state, Some(&server(1)), &alloc("demo", "w:t2"));

        // herdr stopped and started: a new socket, so a new inode.
        let (n, state) = allocate(state, Some(&server(2)), &alloc("demo", "w:t3"));
        assert_eq!(
            n, 1,
            "a herdr server run is what the numbers are counted per, so a \
             different one counts from the start"
        );
        assert!(
            !session_of(&state).tabs.contains_key("w:t1"),
            "and the records of the previous run's tabs go with the count. \
             Keeping them would hand a surviving tab the number it had while \
             the fresh counter walked up to hand that number out again"
        );
    }

    #[test]
    fn an_unreadable_socket_keeps_counting_rather_than_resetting() {
        let (_, state) = allocate(State::default(), Some(&server(1)), &alloc("demo", "w:t1"));
        let (n, _) = allocate(state, None, &alloc("demo", "w:t2"));
        assert_eq!(
            n, 2,
            "reset is the destructive branch, so an unreadable socket must \
             not take it. Keeping a count that should have restarted only \
             makes a number look large; restarting one that should have kept \
             going hands out a name a live tab wears — and if the socket \
             stays unreadable it does that on every single run"
        );
    }

    #[test]
    fn numbers_written_without_a_known_server_are_not_a_restart_either() {
        // Counted while the socket could not be read, so nothing was
        // recorded about which run they belong to.
        let (_, state) = allocate(State::default(), None, &alloc("demo", "w:t1"));
        assert!(
            session_of(&state).server.is_none(),
            "nothing was learned, so nothing is claimed"
        );
        let (n, state) = allocate(state, Some(&server(1)), &alloc("demo", "w:t2"));
        assert_eq!(
            n, 2,
            "learning the server's identity now says nothing about whether it \
             changed — inventing a restart out of an old ignorance would \
             throw away a perfectly good count"
        );
        assert_eq!(
            session_of(&state).server.as_ref(),
            Some(&server(1)),
            "and the identity is recorded now that it is known, so the *next* \
             restart is visible"
        );
    }

    #[test]
    fn two_herdr_sessions_keep_separate_counts() {
        let (a, state) = allocate(
            State::default(),
            Some(&server(1)),
            &Alloc {
                socket_path: Some("/tmp/one.sock"),
                ..alloc("demo", "w:t1")
            },
        );
        let (b, state) = allocate(
            state,
            Some(&server(2)),
            &Alloc {
                socket_path: Some("/tmp/two.sock"),
                ..alloc("demo", "x:t1")
            },
        );
        let (a_again, _) = allocate(
            state,
            Some(&server(1)),
            &Alloc {
                socket_path: Some("/tmp/one.sock"),
                ..alloc("demo", "w:t2")
            },
        );
        assert_eq!(
            (a, b, a_again),
            (1, 1, 2),
            "`herdr --session` gives a user two independent servers. Sharing \
             one entry would make each run look like a restart of the other \
             and reset the count on every alternation between them"
        );
    }

    #[test]
    fn records_of_closed_tabs_go_but_only_when_herdr_actually_answered() {
        let srv = server(1);
        let (_, state) = allocate(State::default(), Some(&srv), &alloc("demo", "w:t1"));
        let (_, state) = allocate(state, Some(&srv), &alloc("demo", "w:t2"));

        let live = set(&["w:t2", "w:t3"]);
        let (_, pruned) = allocate(
            state.clone(),
            Some(&srv),
            &Alloc {
                live_tabs: Some(&live),
                ..alloc("demo", "w:t3")
            },
        );
        assert!(
            !session_of(&pruned).tabs.contains_key("w:t1"),
            "a tab herdr no longer lists has nothing left to keep a number for"
        );
        assert!(
            session_of(&pruned).tabs.contains_key("w:t2"),
            "a tab it does list keeps its number"
        );

        let (_, unpruned) = allocate(state.clone(), Some(&srv), &alloc("demo", "w:t3"));
        assert!(
            session_of(&unpruned).tabs.contains_key("w:t1"),
            "`live_tabs: None` means the call failed, not that herdr listed \
             no tabs. Pruning on it would forget every live tab's number \
             whenever `tab.list` had a bad moment"
        );

        // A tab so new that the snapshot taken moments ago predates it.
        let stale = set(&["w:t1", "w:t2"]);
        let (n, fresh) = allocate(
            state,
            Some(&srv),
            &Alloc {
                live_tabs: Some(&stale),
                ..alloc("demo", "w:t9")
            },
        );
        assert_eq!(
            session_of(&fresh).tabs["w:t9"].number,
            n,
            "this run's own tab always survives the prune — forgetting it \
             here would hand the same tab a second number on its next run"
        );
    }

    #[test]
    fn a_reset_count_starts_at_one_only_when_no_label_still_holds_a_number() {
        let empty = BTreeSet::new();
        let (n, _) = allocate(
            State::default(),
            Some(&server(1)),
            &Alloc {
                live_labels: Some(&empty),
                ..alloc("demo", "w:t1")
            },
        );
        assert_eq!(
            n, 1,
            "a genuinely empty session is the case the reset exists for"
        );

        // What herdr restored from `session.json` after a stop.
        let restored = set(&["demo-1", "demo-3", "other-9"]);
        let (n, _) = allocate(
            State::default(),
            Some(&server(1)),
            &Alloc {
                live_labels: Some(&restored),
                ..alloc("demo", "w:t7")
            },
        );
        assert_eq!(
            n, 4,
            "herdr restores tab labels across a stop, so a count that went \
             back to 1 would hand out `demo-1` while a restored tab still \
             wears it. Past the highest, not merely past however many \
             matched — two labels here, and 3 would be wrong"
        );
    }

    #[test]
    fn a_seed_reads_only_the_labels_this_base_could_have_written() {
        let labels = set(&["demo-2", "demo-5-2", "other-40", "demo", "demonstrate-7"]);
        assert_eq!(
            seed_from_labels(Some(&labels), "demo"),
            6,
            "`demo-5-2` is `demo-5` after the collision walk bumped it, so \
             the number it holds is still 5. `other-40` belongs to another \
             base, `demo` carries no number, and `demonstrate-7` merely \
             starts with the same letters — counting any of them would push \
             every name in this project past a number nothing uses"
        );
        assert_eq!(
            seed_from_labels(None, "demo"),
            1,
            "no tab list to read means nothing is known to be taken"
        );
    }

    #[test]
    fn a_tab_keeps_the_number_its_own_restored_label_carries() {
        let others = set(&["demo-5"]);
        let (n, state) = allocate(
            State::default(),
            Some(&server(1)),
            &Alloc {
                own_label: Some("demo-3"),
                live_labels: Some(&others),
                ..alloc("demo", "w:t2")
            },
        );
        assert_eq!(
            n, 3,
            "after a restart the counter is fresh but herdr restored this \
             tab's label. Taking a new number would rename a tab that was \
             already correctly named and break every reference to it"
        );
        assert_eq!(
            session_of(&state).next["demo"],
            6,
            "and the counter still ends past every number on screen. Stopping \
             at 4 — merely past what this tab took — would walk onto the \
             neighbour's `demo-5` two tabs from now"
        );
    }

    #[test]
    fn only_a_label_this_base_could_have_written_is_adopted() {
        for label in ["release work", "web-3", "demo"] {
            let (n, _) = allocate(
                State::default(),
                Some(&server(1)),
                &Alloc {
                    own_label: Some(label),
                    ..alloc("demo", "w:t2")
                },
            );
            assert_eq!(
                n, 1,
                "{label} carries no number pall8t put there for `demo`, so \
                 there is nothing to adopt and the counter decides. Trusting \
                 any label would let a human's text choose this tab's number"
            );
        }
    }

    #[test]
    fn the_session_map_does_not_grow_without_bound() {
        let mut state = State::default();
        // Older entries first, so `last_used` orders them the way the loop
        // creates them.
        for i in 0..=MAX_SESSIONS {
            let sock = format!("/tmp/s{i}.sock");
            let (_, next) = allocate(
                state,
                Some(&server(i as u64)),
                &Alloc {
                    socket_path: Some(&sock),
                    now: i as u64,
                    ..alloc("demo", "w:t1")
                },
            );
            state = next;
        }
        assert_eq!(
            state.sessions.len(),
            MAX_SESSIONS,
            "one entry per socket path ever seen would grow forever"
        );
        assert!(
            !state.sessions.contains_key("/tmp/s0.sock"),
            "the least recently used is the one to drop"
        );
        assert!(
            state
                .sessions
                .contains_key(&format!("/tmp/s{MAX_SESSIONS}.sock")),
            "never the entry being written — evicting the current run's own \
             counters would reset them on the very run that created them"
        );
    }

    #[test]
    fn a_state_file_from_a_newer_pall8t_is_left_unread() {
        assert_eq!(
            parse(r#"{"version":999,"sessions":{}}"#),
            Load::Frozen(999),
            "a newer pall8t wrote this; reading it as ours and writing ours \
             back would break the other binary's numbering"
        );
        assert_eq!(
            parse(r#"{"version":1,"sessions":{}}"#),
            Load::Have(State {
                version: 1,
                sessions: BTreeMap::new(),
            }),
            "this version's own file is read normally — the guard is on \
             *newer*, not on any version but the current one"
        );
        assert_eq!(
            parse("{ not json"),
            Load::Fresh,
            "a file this version cannot read is not evidence another version \
             needs it: start over rather than give up forever"
        );

        // The case the guard exists for, and the one it used to miss: a
        // newer schema whose *body* today's `State` cannot deserialize.
        // Before the envelope this fell to `Err` → `Fresh`, and the next
        // write replaced a file belonging to a binary still using it.
        for (label, text) in [
            ("a renamed field", r#"{"version":2,"panes":{}}"#),
            ("a retyped field", r#"{"version":2,"sessions":[]}"#),
            ("a dropped field", r#"{"version":2}"#),
        ] {
            assert_eq!(
                parse(text),
                Load::Frozen(2),
                "{label}: a file stamped newer than SCHEMA is frozen on the \
                 strength of its version alone — whether today's State can \
                 read the rest of it is not what decides who owns the file"
            );
        }

        assert_eq!(
            parse(r#"{"sessions":{}}"#),
            Load::Fresh,
            "no version at all is not a claim to be newer: unreadable stays \
             Fresh, and only a readable-but-newer version earns protection"
        );
    }
    #[test]
    fn the_state_file_shape_is_the_contract_another_version_reads() {
        let literal = r#"{
          "version": 1,
          "sessions": {
            "/tmp/herdr.sock": {
              "server": {"dev": 1, "ino": 42, "birth_secs": 7, "birth_nanos": 8},
              "last_used": 99,
              "next": {"demo": 4},
              "tabs": {"w:t1": {"base": "demo", "number": 3}}
            }
          }
        }"#;
        let Load::Have(parsed) = parse(literal) else {
            panic!("the shape this version writes must be the shape it reads");
        };
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::from_str::<serde_json::Value>(literal).unwrap(),
            "field for field, round trip — a rename here is invisible in \
             normal runs and silently resets everybody's numbering the first \
             time two pall8t versions share a $HOME"
        );
    }

    #[test]
    fn the_state_lock_keeps_a_second_allocator_out() {
        let dir = std::path::PathBuf::from("/tmp").join(format!(
            "p8t-lock-{}-{}",
            "tab-numbers",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr-naming.lock");

        let held = Lock::acquire(&path).expect("an uncontended lock is taken");
        assert!(
            Lock::acquire(&path).is_none(),
            "two `pall8t run` allocating at once would both read the same \
             counter, both take the same number, and give two tabs one name \
             — the lock is what serializes the read-modify-write"
        );
        drop(held);
        assert!(
            Lock::acquire(&path).is_some(),
            "and it is released, so the next run is not locked out for good"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// [`allocate`] driven by random operation sequences and checked against a
/// reference model (docs/testing.md, "Properties with a reference model").
///
/// The table tests above pin the scenarios someone thought of; these check
/// the promise itself over sequences nobody wrote down — restarts between
/// adoptions, pruning in the middle of a base switch, more sessions than
/// [`MAX_SESSIONS`]. The model keeps what an observer could know from the
/// outside — which numbers a server run has already *seen* — as a set, not
/// as the allocator's running counter, so the two formulations can only
/// agree if the counter bookkeeping is right.
///
/// What a label *means* is not re-derived here: the model asks
/// [`number_in_label`] (via [`Alloc::adopt`] and directly), which has its own
/// table tests. These properties are about what `allocate` does with it.
#[cfg(test)]
mod properties {
    use super::*;
    use crate::naming::with_counter;
    use proptest::prelude::*;

    /// `None` is the pane herdr gave no socket path; the rest are more
    /// sessions than [`MAX_SESSIONS`] keeps, so eviction is part of the
    /// sequences rather than a case of its own.
    const SOCKETS: [Option<&str>; 11] = [
        None,
        Some("/tmp/s0.sock"),
        Some("/tmp/s1.sock"),
        Some("/tmp/s2.sock"),
        Some("/tmp/s3.sock"),
        Some("/tmp/s4.sock"),
        Some("/tmp/s5.sock"),
        Some("/tmp/s6.sock"),
        Some("/tmp/s7.sock"),
        Some("/tmp/s8.sock"),
        Some("/tmp/s9.sock"),
    ];
    /// The empty id is a boundary herdr never sends, kept because nothing in
    /// `allocate` should care.
    const TABS: [&str; 5] = ["", "w1:t1", "w1:t2", "w2:t1", "w2:t2"];
    /// A base that itself ends in a number (`api-2`), the empty base (which
    /// `with_counter` renders as `p`), and one long enough that its labels
    /// get shortened to fit herdr's cap.
    const BASES: [&str; 4] = [
        "demo",
        "api-2",
        "",
        "a-rather-long-agent-name-that-hits-the-cap",
    ];
    /// Labels shaped like counters but not: a human's, a dangling dash, a
    /// counter with no head, a `+`-signed and a non-digit tail.
    const GARBAGE: [&str; 5] = ["human", "demo-", "-3", "demo-+3", "demo-3x"];

    #[derive(Debug, Clone)]
    struct Op {
        socket: usize,
        /// The server run the socket answers for, by inode; `None` is a
        /// socket that could not be stat'ed.
        server: Option<u64>,
        tab: usize,
        base: usize,
        own_label: Option<String>,
        live_tabs: Option<BTreeSet<String>>,
        live_labels: Option<BTreeSet<String>>,
    }

    /// A label as it can be on screen: one pall8t wrote for some base
    /// (number 0 included), that name after a collision walk, or garbage.
    fn label() -> impl Strategy<Value = String> {
        prop_oneof![
            (0..BASES.len(), 0usize..6).prop_map(|(b, n)| with_counter(BASES[b], n)),
            (0..BASES.len(), 0usize..6, 2usize..4)
                .prop_map(|(b, n, k)| with_counter(&with_counter(BASES[b], n), k)),
            (0..GARBAGE.len()).prop_map(|g| GARBAGE[g].to_string()),
        ]
    }

    fn op() -> impl Strategy<Value = Op> {
        (
            0..SOCKETS.len(),
            prop::option::of(1u64..=3),
            0..TABS.len(),
            0..BASES.len(),
            prop::option::of(label()),
            prop::option::of(prop::sample::subsequence(TABS.to_vec(), 0..=TABS.len())),
            prop::option::of(prop::collection::btree_set(label(), 0..4)),
        )
            .prop_map(
                |(socket, server, tab, base, own_label, live_tabs, live_labels)| Op {
                    socket,
                    server,
                    tab,
                    base,
                    own_label,
                    live_tabs: live_tabs.map(|t| t.into_iter().map(String::from).collect()),
                    live_labels,
                },
            )
    }

    fn server(ino: u64) -> ServerRun {
        ServerRun {
            dev: 1,
            ino,
            birth_secs: Some(1_700_000_000),
            birth_nanos: Some(0),
        }
    }

    /// What an outside observer knows about one session.
    #[derive(Default)]
    struct ModelSession {
        /// The server run the numbers belong to, as last learned.
        server: Option<u64>,
        /// Per base, every number this server run has handed out or been
        /// shown on a label while choosing a fresh one. A fresh number is
        /// one past all of them.
        seen: BTreeMap<String, BTreeSet<usize>>,
        tabs: BTreeMap<String, (String, usize)>,
        last_used: u64,
    }

    /// Runs `ops` through [`allocate`], checking each result against the
    /// model, and returns the final state.
    fn run(ops: &[Op]) -> Result<State, TestCaseError> {
        let mut state = State::default();
        let mut model: BTreeMap<String, ModelSession> = BTreeMap::new();
        for (now, op) in (1u64..).zip(ops) {
            let socket_path = SOCKETS[op.socket];
            let key = socket_path.unwrap_or_default().to_string();
            let tab = TABS[op.tab];
            let base = BASES[op.base];
            let req = Alloc {
                socket_path,
                base,
                tab_id: tab,
                own_label: op.own_label.as_deref(),
                live_tabs: op.live_tabs.as_ref(),
                live_labels: op.live_labels.as_ref(),
                now,
            };
            let live = op.server.map(server);
            let (n, next) = allocate(state, live.as_ref(), &req);
            state = next;

            // A restart is only ever *known*: two readable, different
            // sockets. Every other combination keeps counting.
            let mut ms = model.remove(&key).unwrap_or_default();
            if matches!((ms.server, op.server), (Some(r), Some(l)) if r != l) {
                ms = ModelSession::default();
            }
            if let Some(live_tabs) = &op.live_tabs {
                ms.tabs.retain(|t, _| t == tab || live_tabs.contains(t));
            }
            match ms.tabs.get(tab) {
                Some((b, recorded)) if b == base => prop_assert_eq!(
                    n,
                    *recorded,
                    "a second run in the same tab under the same base keeps \
                     the number the tab already advertises"
                ),
                _ => {
                    let seen = ms.seen.entry(base.to_string()).or_default();
                    seen.extend(
                        op.live_labels
                            .iter()
                            .flatten()
                            .filter_map(|l| number_in_label(l, base)),
                    );
                    if let Some(a) = req.adopt() {
                        prop_assert_eq!(
                            n,
                            a,
                            "a tab whose own label pall8t could have written \
                             keeps that number instead of being renamed"
                        );
                    } else {
                        let fresh = seen.last().map_or(1, |m| m + 1);
                        prop_assert_eq!(
                            n,
                            fresh,
                            "a fresh number is one past everything this \
                             server run has handed out or shown on screen: \
                             lower reissues a live tab's name, higher skips \
                             numbers for nothing"
                        );
                    }
                    seen.insert(n);
                    ms.tabs.insert(tab.to_string(), (base.to_string(), n));
                }
            }
            ms.server = op.server.or(ms.server);
            ms.last_used = now;
            model.insert(key.clone(), ms);
            // Past the bound, the least recently used other session goes —
            // decided here, not copied from the state, so an allocator that
            // evicts too eagerly, too late or the wrong entry disagrees.
            while model.len() > MAX_SESSIONS {
                let oldest = model
                    .iter()
                    .filter(|(k, _)| **k != key)
                    .min_by_key(|(k, s)| (s.last_used, (*k).clone()))
                    .map(|(k, _)| k.clone())
                    .expect("more than one session, so one is not this one");
                model.remove(&oldest);
            }

            prop_assert_eq!(
                state.sessions.keys().collect::<Vec<_>>(),
                model.keys().collect::<Vec<_>>(),
                "the state keeps at most MAX_SESSIONS sessions, drops only the \
                 least recently used when over, and never the one just written"
            );
            prop_assert!(
                state.sessions[&key].next[base] > n,
                "the counter sits past every number it handed out, so the \
                 next tab cannot be given this one"
            );
        }
        Ok(state)
    }

    proptest! {
        // Every mutant cargo-mutants tries runs this suite again, so the
        // case count is a cost multiplier on the PR gate, not only on
        // `cargo test`. 256 sequences of up to 40 operations is ~10k
        // allocations — well under a second — and `PROPTEST_CASES` raises
        // it for a deliberate soak.
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn allocate_matches_the_reference_model(ops in prop::collection::vec(op(), 0..40)) {
            run(&ops)?;
        }

        #[test]
        fn a_state_allocate_produced_survives_the_file_round_trip(
            ops in prop::collection::vec(op(), 0..40),
        ) {
            let state = run(&ops)?;
            let text = serde_json::to_string_pretty(&state).unwrap();
            prop_assert_eq!(
                parse(&text),
                Load::Have(state),
                "what `write_state` writes must read back as the same state, \
                 or every run after the first silently renumbers"
            );
        }

        #[test]
        fn a_file_from_a_newer_pall8t_is_frozen_whatever_its_body(
            version in (SCHEMA + 1)..=u32::MAX,
            body in prop::sample::select(vec!["{}", "[]", "null", "42", "\"x\""]),
        ) {
            let text = format!(r#"{{"version":{version},"sessions":{body}}}"#);
            prop_assert_eq!(
                parse(&text),
                Load::Frozen(version),
                "a newer format is recognised from the version alone, so a \
                 body today's `State` cannot read is still left alone rather \
                 than overwritten (issue #89)"
            );
        }
    }
}
