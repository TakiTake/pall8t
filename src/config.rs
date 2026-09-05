use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Project-level config directory, rooted at the project directory — the
/// project-scope mirror of `~/.pall8t` ([`pall8t_root`]).
pub const PROJECT_DIR: &str = ".pall8t";

/// `<project_dir>/.pall8t/config.toml`, mirroring [`global_path`].
pub fn project_path(project_dir: &Path) -> PathBuf {
    project_dir.join(PROJECT_DIR).join("config.toml")
}

/// Merged, fully-defaulted configuration for one invocation: the global
/// `~/.pall8t/config.toml` overlaid by the project's `.pall8t/config.toml`
/// (requirements §5). Merging is per-field: a field the project file sets
/// wins, one it omits falls through to the global file, then to the
/// built-in default. `mounts` is treated as one field — a project that
/// declares any `[[mounts]]` replaces the global list rather than
/// appending to it, so a global convenience mount can't force itself into
/// every project.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub cpus: u32,
    pub memory: String,
    /// Explicit Containerfile path from config, if any. Resolution to the
    /// file actually built (including the local/default probing when this
    /// is `None`) happens in [`crate::image::resolve`].
    pub containerfile: Option<PathBuf>,
    /// Extra project files whose path+contents fold into the image tag hash
    /// alongside the Containerfile (issue #35), so editing a lockfile the
    /// Containerfile `COPY`s in triggers a rebuild instead of silently
    /// reusing a stale image. See [`crate::image::resolve_watch_paths`] for
    /// path validation and [`crate::image::combined_hash`] for the hash.
    pub watch: Vec<PathBuf>,
    pub command: Vec<String>,
    pub mounts: Vec<MountEntry>,
    /// `[herdr]` — how much of the host herdr session a sandboxed agent
    /// may reach through the relay bridge (see [`crate::relay`]).
    pub herdr: HerdrConfig,
    /// One message per setting the loaded config declares but pall8t does
    /// not act on — a section it no longer honors ([`deprecations_in`]) or
    /// one that is inert without the flag that enables it
    /// ([`inert_agent_name_warning`]) — for the caller to print once per
    /// invocation (see [`load`]). Empty in the common case.
    pub warnings: Vec<String>,
}

/// Merged `[herdr]` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HerdrConfig {
    pub sandbox: HerdrSandbox,
    /// Opt-in: name the herdr tab and agent this run launches in, so the
    /// name a human reads off the tab is the name they can address
    /// (issue #71, see [`crate::naming`]). Undefined means pall8t renames
    /// nothing, exactly as before the feature existed.
    pub auto_rename: bool,
    /// Name to use instead of the workspace directory's basename. Inert
    /// on its own — see [`inert_agent_name_warning`].
    pub agent_name: Option<String>,
}

/// What the sandboxed agent may do to the host herdr session over the
/// bridge. `full` is the default: transparent passthrough of the whole
/// herdr CLI surface except host-admin methods (see
/// [`crate::relay::classify`]) — pall8t is a guardrail, not a blocker.
/// `readonly` permits only inspection (list/get/read/wait); `off` disables
/// the bridge entirely (v1 behavior: the sandbox can't see herdr at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HerdrSandbox {
    #[default]
    Full,
    Readonly,
    Off,
}

impl HerdrSandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            HerdrSandbox::Full => "full",
            HerdrSandbox::Readonly => "readonly",
            HerdrSandbox::Off => "off",
        }
    }
}

/// One `[[mounts]]` entry: a host directory made visible inside the
/// container.
///
/// `deny_unknown_fields` for the same reason [`RawHerdr`] has it: these
/// keys decide whether an agent can write to a real directory, and a
/// config that misspells one (`read_only`, `dest`) would otherwise be
/// accepted while pall8t quietly used the default. A user who typed an
/// intent must never have it dropped in silence.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MountEntry {
    /// Host path to mount. `~` expands against the host's home.
    pub source: PathBuf,
    /// Absolute path inside the container. Defaults to `source` itself —
    /// an identity mount, so absolute paths mean the same thing on both
    /// sides (git metadata, build output, anything the agent reports).
    pub target: Option<PathBuf>,
    /// `true` mounts it read-only and the runtime refuses every write.
    /// Defaults to `false` (see [`mount_readonly`]).
    pub readonly: Option<bool>,
}

/// Whether one mount is read-only, given the entry's own `readonly` and
/// the `--readonly` override from the command line.
///
/// Precedence is the ordinary one — an explicit flag beats a config file,
/// a config file beats the default — and the default is writable. A mount
/// primitive that silently refused writes would surprise anyone who did
/// not ask for it; read-only is the stronger protection and is one word
/// away (ADR-0009).
pub fn mount_readonly(entry: &MountEntry, cli_override: Option<bool>) -> bool {
    cli_override.or(entry.readonly).unwrap_or(false)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    container: RawContainer,
    #[serde(default)]
    run: RawRun,
    mounts: Option<Vec<MountEntry>>,
    /// `[[repos]]` was the previous, git-only form of `[[mounts]]`. Parsed
    /// as an opaque value purely so [`load`] can fail with a message that
    /// names the replacement: its semantics do not map onto a mount
    /// (it cloned the repo and mounted the copy), so honoring it silently
    /// would give a protection level the user never chose.
    repos: Option<toml::Value>,
    /// `[home]` selected the experimental isolated-home compositor, since
    /// removed (ADR-0008). Still *parsed* so a config that carries it keeps
    /// loading — its presence only drives the deprecation warning in
    /// [`load`]. Accepted as an opaque value — not a table — so that a
    /// mistyped `home = "isolated"` still parses (and still warns) instead
    /// of failing the load, and so no field of the old schema survives here.
    home: Option<toml::Value>,
    #[serde(default)]
    herdr: RawHerdr,
}

/// `deny_unknown_fields` is security-relevant here, not just hygiene: a
/// typo like `sandbo = "off"` would otherwise be silently ignored and the
/// bridge would default to `full` — the user believes the sandbox is
/// sealed while it is wide open. Failing the parse is the only honest
/// behavior (PR #38 review finding).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHerdr {
    sandbox: Option<HerdrSandbox>,
    auto_rename: Option<bool>,
    agent_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawContainer {
    cpus: Option<u32>,
    memory: Option<String>,
    containerfile: Option<PathBuf>,
    watch: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawRun {
    command: Option<Vec<String>>,
}

/// `~/.pall8t` — the root under which everything pall8t owns lives
/// (config, container home, default Containerfile). The single place
/// that knows the app-dir location.
pub(crate) fn pall8t_root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t"))
}

/// `~/.pall8t/logs`, created on demand — where both append-only logs in
/// this crate live (the relay's audit log, the agent namer's). A named
/// root per directory is this module's convention; two callers spelling
/// the join out themselves is how they drift.
pub(crate) fn logs_dir() -> Result<PathBuf> {
    let dir = pall8t_root()?.join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `~/.pall8t/state`, created on demand — where the small JSON files
/// pall8t writes for itself and reads back on a later run live. Sibling
/// of [`logs_dir`], and named here for the same reason: two callers
/// spelling the join out themselves is how they drift.
///
/// Deliberately neither of the two neighbours it could have joined.
/// `logs/` is append-only human artifacts nobody parses; `config.toml`
/// at the root is the *user's* file, and burying machine state beside it
/// invites editing one while meaning the other.
pub(crate) fn state_dir() -> Result<PathBuf> {
    let dir = pall8t_root()?.join("state");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn global_path() -> Result<PathBuf> {
    Ok(pall8t_root()?.join("config.toml"))
}

fn read_raw(path: &Path) -> Result<Option<Raw>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let raw =
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    Ok(Some(raw))
}

/// Warnings for one config file about settings it declares that pall8t no
/// longer honors — today only `[home]`. Warning rather than failing the
/// parse: the section only ever selected an experimental, off-by-default
/// feature, so a stale one is never dangerous — but a leftover
/// `mode = "isolated"` silently doing nothing would leave the user believing
/// runs are still isolated, so say it out loud (ADR-0008). Pure so the
/// detection is testable: [`load`] itself reads the real `~/.pall8t`.
///
/// Only a section that actually *sets* something warns. 0.3.0's `init`
/// skeletons wrote a bare `[home]` header with every key commented out, and
/// `init` never rewrites an existing file — keying on the header alone would
/// nag every user who ran `pall8t init` and never opted in, forever.
///
/// The message deliberately does not say the leftover directories are safe
/// to delete: `instances/<run>/root/` is an unharvested run's whole `$HOME`
/// and `inbox/` holds changesets the user never promoted, so deleting them
/// unread is exactly the data loss the old `gc` refused to cause.
fn deprecations_in(raw: &Raw, path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let sets_something = match raw.home.as_ref() {
        None => false,
        // An empty table is the 0.3.0 skeleton's bare `[home]` header: it
        // sets nothing, so it says nothing. A non-empty one is a real
        // setting — including `[[home.policy]]` with no `[home]` header,
        // which lands here as a table holding just that key.
        Some(toml::Value::Table(t)) => !t.is_empty(),
        // `home = "isolated"`, `home = []`, … — a plausible miswrite of
        // `[home] mode = "isolated"`. pall8t honors none of it either way,
        // and silently swallowing an intent is the thing this warning
        // exists to prevent (CodeRabbit, PR #43).
        Some(_) => true,
    };
    if sets_something {
        out.push(format!(
            "[home] in {} is no longer supported and is ignored — the experimental \
             isolated-home compositor has been removed (every run mounts \
             ~/.pall8t/home, as `mode = \"shared\"` always did). Delete the section. \
             If you ran `mode = \"isolated\"`, ~/.pall8t/{{instances,inbox,revisions}} \
             may still hold runs you never harvested and changesets you never \
             promoted — pall8t no longer reads them, so copy out what you want \
             before deleting them.",
            path.display()
        ));
    }
    out
}

/// Fails when a config file still declares `[[repos]]`, naming the
/// replacement.
///
/// A hard error rather than the parse-and-warn `[home]` gets (ADR-0008),
/// because the two settings fail differently. An ignored `[home]` left a
/// feature switched off; an ignored `[[repos]]` would leave a repository
/// the user believes is mounted absent from the sandbox entirely — and
/// silently honoring it as a mount would be worse still, since
/// `[[repos]]` mounted a *clone* and a mount is the real directory. The
/// old key cannot be translated without choosing a protection level on
/// the user's behalf, so it asks.
fn repos_removed_error(raw: &Raw, path: &Path) -> Option<String> {
    raw.repos.as_ref()?;
    Some(format!(
        "[[repos]] in {} is no longer supported — it has been replaced by \
         [[mounts]], which mounts any directory rather than cloning a git \
         repository first (ADR-0009).\n\n\
         Replace each entry:\n\n    \
         [[repos]]\n    source = \"~/src/lib\"\n\n\
         with the mount you actually want:\n\n    \
         [[mounts]]\n    source = \"~/src/lib\"\n    readonly = true   \
         # the agent reads it and cannot change it\n\n\
         Note the difference: [[repos]] mounted a disposable `git clone --local` \
         copy, so writes never reached your checkout. A [[mounts]] entry mounts \
         the real directory — `readonly = true` is what protects it now. \
         Any clones under ~/.pall8t/repos are no longer used and can be deleted.",
        path.display()
    ))
}

/// Every config file still declaring `[[repos]]`, in one message.
///
/// Reported together rather than one at a time: a user with the section
/// in both the global and the project file would otherwise fix the first,
/// rerun, and only then be told about the second.
fn repos_removed_errors(files: &[(&Raw, &Path)]) -> Option<String> {
    let msgs: Vec<_> = files
        .iter()
        .filter_map(|(raw, path)| repos_removed_error(raw, path))
        .collect();
    (!msgs.is_empty()).then(|| msgs.join("\n\n"))
}

/// Loads the merged config for a project rooted at `project_dir`.
pub fn load(project_dir: &Path) -> Result<Config> {
    let global_path = global_path()?;
    let project_path = project_path(project_dir);
    let global = read_raw(&global_path)?.unwrap_or_default();
    let project = read_raw(&project_path)?.unwrap_or_default();
    // Both files are reported: a user cleaning up needs to know about each
    // one that still carries the section, not just the first found.
    // Before anything else: a config still on the old key gets a message,
    // not a run with a mount silently missing.
    if let Some(msg) = repos_removed_errors(&[
        (&global, global_path.as_path()),
        (&project, project_path.as_path()),
    ]) {
        return Err(anyhow!(msg));
    }
    let per_file: Vec<String> = deprecations_in(&global, &global_path)
        .into_iter()
        .chain(deprecations_in(&project, &project_path))
        .collect();
    let merged = merge(global, project);
    // The inert-setting check reads the *merged* config, not each file:
    // a global `auto_rename = true` enables a project's `agent_name`, and
    // warning per file would call that combination inert.
    let warnings = per_file
        .into_iter()
        .chain(inert_agent_name_warning(&merged.herdr))
        .collect();
    Ok(Config { warnings, ..merged })
}

/// The warning for a config that names the herdr agent but never turns
/// naming on. `agent_name` alone is a setting that silently does nothing,
/// which is exactly what the `[home]` warning above exists to prevent.
///
/// The alternative — letting `agent_name` imply `auto_rename = true` —
/// was rejected in issue #71: it would make "undefined means off" untrue
/// for half the feature, so a user who set a name to have it *ready*
/// would find pall8t renaming their tabs.
fn inert_agent_name_warning(herdr: &HerdrConfig) -> Option<String> {
    (!herdr.auto_rename && herdr.agent_name.is_some()).then(|| {
        "[herdr] agent_name is set but auto_rename is not — pall8t renames \
         neither the tab nor the agent. Add `auto_rename = true` under \
         [herdr] to turn naming on, or delete agent_name."
            .to_string()
    })
}

fn merge(global: Raw, project: Raw) -> Config {
    Config {
        cpus: project
            .container
            .cpus
            .or(global.container.cpus)
            .unwrap_or(4),
        memory: project
            .container
            .memory
            .or(global.container.memory)
            .unwrap_or_else(|| "8g".to_string()),
        containerfile: project
            .container
            .containerfile
            .or(global.container.containerfile),
        // Replace, not append — like `repos`: a project's
        // watch list fully controls what folds into its own image hash
        // rather than inheriting entries a global config can't guarantee
        // exist in every project.
        watch: project
            .container
            .watch
            .or(global.container.watch)
            .unwrap_or_default(),
        command: project
            .run
            .command
            .or(global.run.command)
            .unwrap_or_else(|| vec!["claude".to_string()]),
        mounts: project.mounts.or(global.mounts).unwrap_or_default(),
        // `[home]` is parsed and ignored; `load` turns its presence into a
        // deprecation message, which `merge` (path-less) can't produce.
        warnings: Vec::new(),
        herdr: HerdrConfig {
            sandbox: project
                .herdr
                .sandbox
                .or(global.herdr.sandbox)
                .unwrap_or_default(),
            // Per-field, like everything else here: a global
            // `auto_rename = true` stays on for a project that only sets
            // `agent_name`, and the pair is judged after merging (see
            // [`inert_agent_name_warning`]) rather than file by file.
            auto_rename: project
                .herdr
                .auto_rename
                .or(global.herdr.auto_rename)
                .unwrap_or(false),
            agent_name: project.herdr.agent_name.or(global.herdr.agent_name),
        },
    }
}

/// Skeleton written by `pall8t init` as `~/.pall8t/config.toml`.
pub const GLOBAL_SKELETON: &str = r#"# pall8t global configuration. Per-project .pall8t/config.toml overrides
# these values field by field.

[container]
# cpus = 4
# memory = "8g"
# containerfile = "/absolute/path/to/Containerfile"

[run]
# Command run by `pall8t run`. --dangerously-skip-permissions is NOT in
# the default; add it here explicitly if you want it.
# command = ["claude"]

# Directories from the host, made visible inside the container. Mount
# anything — a reference checkout, a notes folder, a dataset.
# [[mounts]]
# source = "~/src/some-library"
# Where it appears inside the container. Defaults to the same absolute
# path as the source, so paths mean the same thing on both sides.
# target = "/src/some-library"
# Writable by default. true mounts the real directory read-only and the
# runtime refuses every write to it — the way to let an agent read a
# checkout it must not change.
# readonly = false

[herdr]
# What a sandboxed agent may do to the host herdr session when pall8t runs
# inside a herdr pane (the bridge is inert outside herdr):
#   "full"     (default) transparent herdr CLI passthrough, except host-admin
#              methods (server stop/handoff/reload, integration/plugin
#              installs) which are always blocked
#   "readonly" inspection only (list/get/read/wait) — no panes, prompts,
#              or input from inside the sandbox
#   "off"      the sandbox can't see herdr at all
# sandbox = "full"
#
# Name the herdr tab and the agent this run launches in, with the same
# string, so what you read off the tab is what `herdr agent prompt <name>`
# takes. Off unless set: pall8t renames nothing by default. The name is
# the workspace directory's basename plus the tab's number (~/src/foo in
# tab 2 -> "foo-2"); a tab you renamed yourself keeps your label.
# auto_rename = true
# Name to use instead of the directory basename. Inert on its own —
# without auto_rename above, pall8t warns and renames nothing.
# agent_name = "api"
"#;

/// Skeleton written by `pall8t init` as `.pall8t/config.toml`.
pub const PROJECT_SKELETON: &str = r#"# pall8t project configuration. Fields set here override
# ~/.pall8t/config.toml.

[container]
# cpus = 4
# memory = "8g"
# Containerfile used for this project's image. Default (usually no need to
# set this): .pall8t/Containerfile if present, else the built-in default
# image. Only set this to point somewhere else — relative to the project
# dir (absolute paths and ~ also work). A project Containerfile builds with
# the project directory as its build context, so COPY paths are relative to
# the project root; if the tree is large, add ignore patterns in
# <containerfile>.dockerignore, next to and named after the Containerfile
# (the location apple/container reads them from):
# containerfile = "path/to/other/Containerfile"
#
# Extra project files whose contents also decide whether to rebuild the
# image — e.g. a lockfile the Containerfile COPYs in and builds from, so
# editing it doesn't silently reuse a stale image. Requires a project
# Containerfile (containerfile above or .pall8t/Containerfile); paths are
# relative to the project dir and must exist.
# watch = ["flake.nix", "flake.lock"]

[run]
# command = ["claude"]

# Mounts for this project. Declaring any here replaces the global list
# rather than adding to it.
# [[mounts]]
# source = "~/src/some-library"
# target = "/src/some-library"   # optional; defaults to the source path
# readonly = true                # optional; defaults to false (writable)
# Override for one run without editing this file:
#   pall8t run --readonly

[herdr]
# sandbox = "full"   # or "readonly" / "off" — see ~/.pall8t/config.toml
# auto_rename = true # name this run's herdr tab and agent "<dir>-<tab number>"
# agent_name = "api" # ... using this instead of the directory basename
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse(s: &str) -> Raw {
        toml::from_str(s).unwrap()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn project_path_is_dot_pall8t_config_toml() {
        let project_dir = Path::new("/some/project");
        assert_eq!(
            project_path(project_dir),
            project_dir.join(".pall8t").join("config.toml")
        );
    }

    #[test]
    fn project_config_is_read_from_dot_pall8t_dir() {
        // Exercises the exact path `load()` reads from, without `load()`
        // itself (which also reads the real ~/.pall8t/config.toml and so
        // isn't safe to assert on in a test).
        let project_dir = tmp_dir("project-config");
        let pall8t_dir = project_dir.join(PROJECT_DIR);
        fs::create_dir_all(&pall8t_dir).unwrap();
        fs::write(pall8t_dir.join("config.toml"), "[container]\ncpus = 2\n").unwrap();

        let raw = read_raw(&project_path(&project_dir)).unwrap().unwrap();
        assert_eq!(raw.container.cpus, Some(2));

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn project_config_ignores_root_pall8t_toml() {
        // The pre-issue-24 path must no longer be read at all: a root
        // pall8t.toml sitting next to (a missing) .pall8t/ is invisible —
        // hard switch, no fallback.
        let project_dir = tmp_dir("legacy-root-file");
        fs::write(project_dir.join("pall8t.toml"), "[container]\ncpus = 2\n").unwrap();

        let raw = read_raw(&project_path(&project_dir)).unwrap();
        assert!(
            raw.is_none(),
            "no .pall8t/config.toml exists at this project_dir"
        );

        let _ = fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn defaults_when_both_empty() {
        let cfg = merge(Raw::default(), Raw::default());
        assert_eq!(cfg.cpus, 4);
        assert_eq!(cfg.memory, "8g");
        assert_eq!(cfg.containerfile, None);
        assert!(cfg.watch.is_empty());
        assert_eq!(cfg.command, vec!["claude".to_string()]);
        assert!(cfg.mounts.is_empty());
        assert!(cfg.warnings.is_empty());
        assert_eq!(
            cfg.herdr.sandbox,
            HerdrSandbox::Full,
            "full herdr passthrough is the default"
        );
        assert!(
            !cfg.herdr.auto_rename && cfg.herdr.agent_name.is_none(),
            "naming is opt-in: undefined means pall8t renames neither the \
             tab nor the agent (issue #71)"
        );
    }

    #[test]
    fn herdr_naming_merges_per_field_and_only_agent_name_is_inert() {
        let global = parse("[herdr]\nauto_rename = true\nagent_name = \"api\"\n");
        let cfg = merge(global.clone(), Raw::default());
        assert!(cfg.herdr.auto_rename);
        assert_eq!(cfg.herdr.agent_name.as_deref(), Some("api"));
        assert!(
            inert_agent_name_warning(&cfg.herdr).is_none(),
            "a name with naming switched on is doing its job"
        );

        let cfg = merge(global, parse("[herdr]\nagent_name = \"web\"\n"));
        assert_eq!(cfg.herdr.agent_name.as_deref(), Some("web"), "project wins");
        assert!(
            cfg.herdr.auto_rename && inert_agent_name_warning(&cfg.herdr).is_none(),
            "and the global auto_rename still enables it — the pair is judged \
             after merging, so a project that only names itself isn't called inert"
        );

        let cfg = merge(Raw::default(), parse("[herdr]\nagent_name = \"api\"\n"));
        assert!(
            !cfg.herdr.auto_rename,
            "a name must not imply auto_rename: that would make \"undefined \
             means off\" untrue for half the feature (issue #71)"
        );
        let warning = inert_agent_name_warning(&cfg.herdr).expect("a setting that does nothing");
        assert!(
            warning.contains("auto_rename"),
            "and the warning has to name the flag that would turn it on: {warning}"
        );

        assert!(
            toml::from_str::<Raw>("[herdr]\nauto_renam = true\n").is_err(),
            "a misspelled key fails the parse rather than silently renaming \
             nothing (deny_unknown_fields)"
        );
    }

    #[test]
    fn herdr_sandbox_merges_per_field_and_rejects_unknown_values() {
        let global = parse("[herdr]\nsandbox = \"readonly\"\n");
        let cfg = merge(global.clone(), Raw::default());
        assert_eq!(cfg.herdr.sandbox, HerdrSandbox::Readonly);

        let project = parse("[herdr]\nsandbox = \"off\"\n");
        let cfg = merge(global, project);
        assert_eq!(cfg.herdr.sandbox, HerdrSandbox::Off, "project wins");

        assert!(
            toml::from_str::<Raw>("[herdr]\nsandbox = \"bogus\"\n").is_err(),
            "an unknown sandbox value must fail to parse"
        );
        assert!(
            toml::from_str::<Raw>("[herdr]\nsandbo = \"off\"\n").is_err(),
            "a misspelled key must fail the parse, not silently leave the \
             bridge in full mode (deny_unknown_fields)"
        );
    }

    /// A config left over from the isolated-home compositor (ADR-0008) must
    /// still load: `[home]` is parsed and ignored, so an upgrading user gets
    /// a warning to clean up, never a failed run.
    #[test]
    fn stale_home_section_parses_and_is_ignored() {
        let stale = parse(
            r#"
            [container]
            cpus = 2
            [home]
            mode = "isolated"
            revisions_keep = 5
            [[home.policy]]
            glob = ".config/a/**"
            class = "knowledge"
            "#,
        );

        let cfg = merge(stale.clone(), Raw::default());
        assert_eq!(cfg.cpus, 2, "the rest of the file is honored as usual");
        assert!(
            cfg.warnings.is_empty(),
            "`merge` knows no paths; `load` is what attaches the warning"
        );

        let path = Path::new("/x/.pall8t/config.toml");
        let warnings = deprecations_in(&stale, path);
        assert_eq!(
            warnings.len(),
            1,
            "a [home] section that sets something must be reported, not silently \
             ignored — otherwise `mode = \"isolated\"` looks alive while doing nothing"
        );
        assert!(
            warnings[0].contains("/x/.pall8t/config.toml"),
            "the warning must name the file to edit, or the user hunts between \
             the global and project configs: {}",
            warnings[0]
        );
        assert!(
            !warnings[0].contains("safe to"),
            "the warning must not bless deleting ~/.pall8t/instances|inbox — an \
             unharvested run's whole $HOME lives there (ADR-0008): {}",
            warnings[0]
        );

        assert!(
            deprecations_in(&parse("[container]\ncpus = 2\n"), path).is_empty(),
            "a config without [home] must warn about nothing"
        );
    }

    /// The warning fires on intent, not on syntax. Two regression pins in
    /// one: 0.3.0's `init` skeletons wrote a bare `[home]` header with every
    /// key commented out and `init` never rewrites an existing file, so
    /// warning on the header alone would nag every `init` user forever about
    /// a feature they never enabled; and a `home` value that isn't a table
    /// at all is still an intent being dropped, so it must not slip through.
    #[test]
    fn home_section_warns_only_when_it_sets_something() {
        let path = Path::new("/x/.pall8t/config.toml");
        for src in [
            // The 0.3.0 skeleton shape: header present, everything commented.
            "[container]\n[home]\n# mode = \"shared\"\n[herdr]\n",
            "[home]\n",
        ] {
            assert!(
                deprecations_in(&parse(src), path).is_empty(),
                "an empty [home] table sets nothing, so there is nothing to \
                 warn about: {src:?}"
            );
        }

        assert_eq!(
            deprecations_in(&parse("[[home.policy]]\nglob = \".x\"\n"), path).len(),
            1,
            "`[[home.policy]]` without a `[home]` header still sets something"
        );

        // Regression pin (CodeRabbit, PR #43): `home` need not be a table.
        // Narrowing the check with `as_table()` made every non-table value
        // look absent, so a miswritten `home = "isolated"` was dropped in
        // silence — the exact outcome the warning exists to prevent.
        for miswritten in ["home = \"isolated\"\n", "home = []\n", "home = 5\n"] {
            assert_eq!(
                deprecations_in(&parse(miswritten), path).len(),
                1,
                "a non-table `home` value is still an intent pall8t ignores, so \
                 it must warn: {miswritten:?}"
            );
        }
    }

    #[test]
    fn project_overrides_global_per_field() {
        let global = parse(
            r#"
            [container]
            cpus = 8
            memory = "16g"
            [run]
            command = ["codex"]
            "#,
        );
        let project = parse(
            r"
            [container]
            cpus = 2
            ",
        );
        let cfg = merge(global, project);
        assert_eq!(cfg.cpus, 2, "project field wins");
        assert_eq!(cfg.memory, "16g", "unset project field falls through");
        assert_eq!(cfg.command, vec!["codex".to_string()]);
    }

    #[test]
    fn project_mounts_replace_global_mounts() {
        let global = parse("[[mounts]]\nsource = \"~/src/a\"\n");
        let project = parse("[[mounts]]\nsource = \"~/src/b\"\n");
        let cfg = merge(global, project);
        assert_eq!(
            cfg.mounts,
            vec![MountEntry {
                source: "~/src/b".into(),
                target: None,
                readonly: None
            }],
            "a project's list replaces the global one rather than appending, so a \
             global convenience mount can't force itself into every project"
        );

        let global = parse("[[mounts]]\nsource = \"~/src/a\"\n");
        let cfg = merge(global, Raw::default());
        assert_eq!(
            cfg.mounts,
            vec![MountEntry {
                source: "~/src/a".into(),
                target: None,
                readonly: None
            }],
            "global mounts apply when the project declares none"
        );
    }

    #[test]
    fn mount_entry_parses_all_three_keys() {
        let cfg = merge(
            parse("[[mounts]]\nsource = \"~/notes\"\ntarget = \"/notes\"\nreadonly = true\n"),
            Raw::default(),
        );
        assert_eq!(
            cfg.mounts,
            vec![MountEntry {
                source: "~/notes".into(),
                target: Some("/notes".into()),
                readonly: Some(true)
            }]
        );
    }

    #[test]
    fn mount_readonly_precedence_table() {
        let unset = MountEntry {
            source: "~/src/a".into(),
            target: None,
            readonly: None,
        };
        let writable = MountEntry {
            readonly: Some(false),
            ..unset.clone()
        };
        let readonly = MountEntry {
            readonly: Some(true),
            ..unset.clone()
        };

        assert!(
            !mount_readonly(&unset, None),
            "writable is the default: a mount primitive that silently refused \
             writes would surprise anyone who did not ask for read-only"
        );
        assert!(
            !mount_readonly(&writable, None),
            "an entry can state the default explicitly"
        );
        assert!(mount_readonly(&readonly, None), "the entry opts in");

        assert!(
            !mount_readonly(&readonly, Some(false)),
            "--readonly=false beats an entry that asked for read-only — an \
             explicit flag is the more recent, more specific intent"
        );
        assert!(
            mount_readonly(&writable, Some(true)),
            "--readonly beats an entry that asked for writable"
        );
    }

    #[test]
    fn mount_entry_rejects_misspelled_keys() {
        for bad in [
            "[[mounts]]\nsource = \"~/a\"\nread_only = true\n",
            "[[mounts]]\nsource = \"~/a\"\ndest = \"/a\"\n",
        ] {
            assert!(
                toml::from_str::<Raw>(bad).is_err(),
                "a misspelled key must fail the parse: silently ignoring it hands \
                 back a default the user did not choose, and they find out only \
                 when a write succeeds that should not have: {bad:?}"
            );
        }
        assert!(
            toml::from_str::<Raw>("[[mounts]]\nsource = \"~/a\"\nreadonly = \"yes\"\n").is_err(),
            "readonly is a boolean; a string is a miswrite, not a truthy value"
        );
    }

    /// `[[repos]]` is gone, and a config still carrying it must say so
    /// rather than run with the mount missing. Not the parse-and-warn
    /// treatment `[home]` got: an ignored `[home]` left a feature off,
    /// while an ignored `[[repos]]` leaves a directory the user believes is
    /// mounted simply absent.
    #[test]
    fn removed_repos_section_is_reported_with_its_replacement() {
        let path = Path::new("/x/.pall8t/config.toml");
        let msg = repos_removed_error(&parse("[[repos]]\nsource = \"~/src/a\"\n"), path)
            .expect("[[repos]] must be reported");
        assert!(
            msg.contains("/x/.pall8t/config.toml"),
            "the message must name the file to edit: {msg}"
        );
        assert!(
            msg.contains("[[mounts]]") && msg.contains("readonly = true"),
            "it must show the replacement, not just refuse: {msg}"
        );
        assert!(
            msg.contains("clone"),
            "and must say the semantics changed — [[repos]] mounted a copy, a \
             mount is the real directory — or a user pastes the new form and \
             quietly loses the protection they had: {msg}"
        );

        assert_eq!(
            repos_removed_error(&parse("[[mounts]]\nsource = \"~/src/a\"\n"), path),
            None,
            "a config already migrated must load without complaint"
        );
    }

    /// Both files at once, not one per run: a user with the section in the
    /// global *and* the project config would otherwise fix one, rerun, and
    /// only then hear about the other (`CodeRabbit`, PR #46).
    #[test]
    fn both_legacy_config_files_are_reported_together() {
        let global_path = Path::new("/home/u/.pall8t/config.toml");
        let project_path = Path::new("/proj/.pall8t/config.toml");
        let legacy = parse("[[repos]]\nsource = \"~/src/a\"\n");
        let migrated = parse("[[mounts]]\nsource = \"~/src/a\"\n");

        let msg = repos_removed_errors(&[(&legacy, global_path), (&legacy, project_path)])
            .expect("two legacy files must be reported");
        assert!(
            msg.contains("/home/u/.pall8t/config.toml")
                && msg.contains("/proj/.pall8t/config.toml"),
            "both paths must appear, or the second one is a surprise on the next \
             run: {msg}"
        );

        let one = repos_removed_errors(&[(&migrated, global_path), (&legacy, project_path)])
            .expect("one legacy file must still be reported");
        assert!(
            !one.contains("/home/u/.pall8t/config.toml"),
            "a migrated file must not be named as a problem: {one}"
        );

        assert_eq!(
            repos_removed_errors(&[(&migrated, global_path), (&migrated, project_path)]),
            None,
            "nothing legacy, nothing to say"
        );
    }

    #[test]
    fn watch_merges_project_replaces_global() {
        let global = parse("[container]\nwatch = [\"a.lock\"]\n");
        let project = parse("[container]\nwatch = [\"b.lock\", \"c.lock\"]\n");
        let cfg = merge(global.clone(), project);
        assert_eq!(
            cfg.watch,
            vec![PathBuf::from("b.lock"), PathBuf::from("c.lock")],
            "project watch list replaces global rather than appending"
        );

        let cfg = merge(global, Raw::default());
        assert_eq!(
            cfg.watch,
            vec![PathBuf::from("a.lock")],
            "global watch applies when the project declares none"
        );
    }

    #[test]
    fn skeletons_parse_and_yield_defaults() {
        // The commented-out skeletons must stay valid TOML that changes
        // nothing until the user uncomments a line.
        let g: Raw = toml::from_str(GLOBAL_SKELETON).unwrap();
        let p: Raw = toml::from_str(PROJECT_SKELETON).unwrap();
        let cfg = merge(g, p);
        assert_eq!(cfg, merge(Raw::default(), Raw::default()));
    }
}
