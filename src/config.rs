use anyhow::{Context, Result};
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
/// built-in default. `repos` is treated as one field — a project that
/// declares any `[[repos]]` replaces the global list rather than
/// appending to it, so a global convenience repo can't force itself into
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
    pub repos: Vec<RepoEntry>,
    /// `[herdr]` — how much of the host herdr session a sandboxed agent
    /// may reach through the relay bridge (see [`crate::relay`]).
    pub herdr: HerdrConfig,
    /// One message per config file that still carries a setting pall8t no
    /// longer honors, for the caller to print once per invocation (see
    /// [`load`]). Empty in the common case.
    pub deprecations: Vec<String>,
}

/// Merged `[herdr]` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HerdrConfig {
    pub sandbox: HerdrSandbox,
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

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RepoEntry {
    /// Host path of a reference repository; duplicated via
    /// `git clone --local` and the copy mounted at this path (FR-4).
    pub source: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    container: RawContainer,
    #[serde(default)]
    run: RawRun,
    repos: Option<Vec<RepoEntry>>,
    /// `[home]` selected the experimental isolated-home compositor, since
    /// removed (ADR-0008). Still *parsed* so a config that carries it keeps
    /// loading — its presence only drives the deprecation warning in
    /// [`load`]. Accepted as an opaque table so no field of the old schema
    /// has to survive here.
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
/// (config, container home, default Containerfile, reference-repo
/// clones). The single place that knows the app-dir location.
pub(crate) fn pall8t_root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t"))
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
    // `[[home.policy]]` alone (no `[home]` header) also lands here, as a
    // table with just that key — still a setting, still worth a warning.
    let sets_something = raw
        .home
        .as_ref()
        .and_then(toml::Value::as_table)
        .is_some_and(|t| !t.is_empty());
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

/// Loads the merged config for a project rooted at `project_dir`.
pub fn load(project_dir: &Path) -> Result<Config> {
    let global_path = global_path()?;
    let project_path = project_path(project_dir);
    let global = read_raw(&global_path)?.unwrap_or_default();
    let project = read_raw(&project_path)?.unwrap_or_default();
    // Both files are reported: a user cleaning up needs to know about each
    // one that still carries the section, not just the first found.
    let deprecations = deprecations_in(&global, &global_path)
        .into_iter()
        .chain(deprecations_in(&project, &project_path))
        .collect();
    Ok(Config {
        deprecations,
        ..merge(global, project)
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
        repos: project.repos.or(global.repos).unwrap_or_default(),
        // `[home]` is parsed and ignored; `load` turns its presence into a
        // deprecation message, which `merge` (path-less) can't produce.
        deprecations: Vec::new(),
        herdr: HerdrConfig {
            sandbox: project
                .herdr
                .sandbox
                .or(global.herdr.sandbox)
                .unwrap_or_default(),
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
# dir (absolute paths and ~ also work):
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

[herdr]
# sandbox = "full"   # or "readonly" / "off" — see ~/.pall8t/config.toml
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
        assert!(cfg.repos.is_empty());
        assert!(cfg.deprecations.is_empty());
        assert_eq!(
            cfg.herdr.sandbox,
            HerdrSandbox::Full,
            "full herdr passthrough is the default"
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
            cfg.deprecations.is_empty(),
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

    /// Regression pin: 0.3.0's `init` skeletons wrote a bare `[home]` header
    /// with every key commented out, and `init` never rewrites an existing
    /// file. Warning on the header alone would nag every user who ran `init`
    /// and never opted in — on every `run`, forever, about a feature they
    /// never used.
    #[test]
    fn empty_home_section_from_the_old_skeleton_does_not_warn() {
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
    fn project_repos_replace_global_repos() {
        let global = parse("[[repos]]\nsource = \"~/src/a\"\n");
        let project = parse("[[repos]]\nsource = \"~/src/b\"\n");
        let cfg = merge(global, project);
        assert_eq!(
            cfg.repos,
            vec![RepoEntry {
                source: "~/src/b".into()
            }]
        );

        let global = parse("[[repos]]\nsource = \"~/src/a\"\n");
        let cfg = merge(global, Raw::default());
        assert_eq!(
            cfg.repos,
            vec![RepoEntry {
                source: "~/src/a".into()
            }],
            "global repos apply when the project declares none"
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
