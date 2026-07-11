use crate::home::{Class, HomeMode, MergeStrategy};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
    pub command: Vec<String>,
    pub repos: Vec<RepoEntry>,
    /// `[home]` — how the container home is materialized and how a run's
    /// changes to it are classified (see [`crate::home`]).
    pub home: HomeConfig,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RepoEntry {
    /// Host path of a reference repository; duplicated via
    /// `git clone --local` and the copy mounted at this path (FR-4).
    pub source: PathBuf,
}

/// Merged `[home]` configuration. `mode` picks today's shared home or the
/// per-run fork-and-harvest model; `policy` prepends user overrides to the
/// built-in path classification ([`crate::home::default_rules`]).
/// `revisions_keep`/`inbox_ttl_days` bound `isolated` mode's disk usage and
/// staleness warnings (FR-7/FR-9); both are inert in `shared` mode.
#[derive(Debug, Clone, PartialEq)]
pub struct HomeConfig {
    pub mode: HomeMode,
    pub policy: Vec<PolicyRule>,
    pub revisions_keep: u32,
    pub inbox_ttl_days: u32,
}

impl Default for HomeConfig {
    /// Matches [`merge`]'s defaults, for callers (the standalone `home`
    /// subcommands) that fall back to this when the cwd's config can't be
    /// loaded at all rather than duplicating the default values.
    fn default() -> Self {
        HomeConfig {
            mode: HomeMode::default(),
            policy: Vec::new(),
            revisions_keep: crate::home::DEFAULT_REVISIONS_KEEP,
            inbox_ttl_days: crate::home::DEFAULT_INBOX_TTL_DAYS,
        }
    }
}

/// One `[[home.policy]]` rule: a glob (matched against a `$HOME`-relative
/// path) mapped to the `class` it forces and/or the merge `strategy` it uses.
/// First match wins, user overrides before the defaults — see
/// [`crate::home::classify`]. `class` may be omitted when only overriding the
/// strategy (it then defaults to `state`); a rule with neither is ignored
/// (see [`crate::home::validate_policy`]). Also `Serialize`: a revision
/// records the policy overrides active when it was recorded, so `diff` can
/// later redact a path that was declared secret at record time even if the
/// cwd's current policy no longer says so (see [`crate::home::diff`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    /// `path` is accepted as an alias so a rule can read naturally
    /// (`path = ".claude/history.jsonl"`); `glob` is the documented key.
    #[serde(alias = "path")]
    pub glob: String,
    #[serde(default)]
    pub class: Option<Class>,
    #[serde(default)]
    pub strategy: Option<MergeStrategy>,
}

#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    container: RawContainer,
    #[serde(default)]
    run: RawRun,
    repos: Option<Vec<RepoEntry>>,
    #[serde(default)]
    home: RawHome,
}

#[derive(Debug, Default, Deserialize)]
struct RawHome {
    mode: Option<HomeMode>,
    policy: Option<Vec<PolicyRule>>,
    revisions_keep: Option<u32>,
    inbox_ttl_days: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawContainer {
    cpus: Option<u32>,
    memory: Option<String>,
    containerfile: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
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

/// Loads the merged config for a project rooted at `project_dir`.
pub fn load(project_dir: &Path) -> Result<Config> {
    let global = read_raw(&global_path()?)?.unwrap_or_default();
    let project = read_raw(&project_path(project_dir))?.unwrap_or_default();
    Ok(merge(global, project))
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
        command: project
            .run
            .command
            .or(global.run.command)
            .unwrap_or_else(|| vec!["claude".to_string()]),
        repos: project.repos.or(global.repos).unwrap_or_default(),
        home: HomeConfig {
            mode: project.home.mode.or(global.home.mode).unwrap_or_default(),
            // Policy overrides replace rather than append (like `repos`), so
            // a project fully controls its classification without inheriting
            // global rules it can't see; the built-in defaults always apply
            // underneath in `classify`.
            policy: project
                .home
                .policy
                .or(global.home.policy)
                .unwrap_or_default(),
            revisions_keep: project
                .home
                .revisions_keep
                .or(global.home.revisions_keep)
                .unwrap_or(crate::home::DEFAULT_REVISIONS_KEEP),
            inbox_ttl_days: project
                .home
                .inbox_ttl_days
                .or(global.home.inbox_ttl_days)
                .unwrap_or(crate::home::DEFAULT_INBOX_TTL_DAYS),
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

[home]
# mode = "shared"    # default: every run mounts ~/.pall8t/home rw (v1 behavior)
# mode = "isolated"  # per-run fork; harvest changes into an inbox to promote
#
# Override the built-in path classification (secret | state | knowledge |
# ephemeral). First match wins, these before the defaults.
# [[home.policy]]
# glob = ".config/my-tool/**"
# class = "knowledge"
#
# `strategy = "union"` line-merges an append-only file (keeps both sides, never
# conflicts) instead of the class default. `class` may be omitted with a
# strategy (defaults to state — auto-merged at harvest). `path` is an alias for
# `glob`.
# [[home.policy]]
# glob = ".config/my-tool/log.jsonl"
# strategy = "union"
#
# How much `isolated`-mode history to keep (FR-7). Pruned after each
# recorded revision and again by `pall8t home gc`.
# revisions_keep = 20
#
# `pall8t home gc` warns (never deletes) about inbox changesets older than
# this many days — dropping unreviewed knowledge is always a user decision.
# inbox_ttl_days = 14
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

[run]
# command = ["claude"]

[home]
# mode = "isolated"  # per-run home fork + harvest/promote (default: shared)
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
        assert_eq!(cfg.command, vec!["claude".to_string()]);
        assert!(cfg.repos.is_empty());
        assert_eq!(
            cfg.home.mode,
            HomeMode::Shared,
            "shared home is the default"
        );
        assert!(cfg.home.policy.is_empty());
        assert_eq!(cfg.home.revisions_keep, crate::home::DEFAULT_REVISIONS_KEEP);
        assert_eq!(cfg.home.inbox_ttl_days, crate::home::DEFAULT_INBOX_TTL_DAYS);
        assert_eq!(
            cfg.home,
            HomeConfig::default(),
            "Default matches merge()'s defaults"
        );
    }

    #[test]
    fn revisions_keep_and_inbox_ttl_merge_per_field() {
        let global = parse(
            r"
            [home]
            revisions_keep = 5
            inbox_ttl_days = 3
            ",
        );
        // Project overrides only one of the two.
        let project = parse("[home]\nrevisions_keep = 50\n");
        let cfg = merge(global, project);
        assert_eq!(cfg.home.revisions_keep, 50, "project field wins");
        assert_eq!(
            cfg.home.inbox_ttl_days, 3,
            "unset project field falls through"
        );
    }

    #[test]
    fn home_mode_and_policy_merge_per_field() {
        let global = parse(
            r#"
            [home]
            mode = "isolated"
            [[home.policy]]
            glob = ".config/a/**"
            class = "knowledge"
            "#,
        );
        // Project omits mode (falls through) but replaces the policy list.
        let project = parse(
            r#"
            [[home.policy]]
            glob = ".config/b/**"
            class = "ephemeral"
            "#,
        );
        let cfg = merge(global, project);
        assert_eq!(
            cfg.home.mode,
            HomeMode::Isolated,
            "unset project mode falls through"
        );
        assert_eq!(
            cfg.home.policy,
            vec![PolicyRule {
                glob: ".config/b/**".to_string(),
                class: Some(Class::Ephemeral),
                strategy: None,
            }],
            "project policy replaces global policy"
        );
    }

    #[test]
    fn policy_strategy_and_path_alias_parse() {
        // The user's snippet (`path` + `strategy`, no explicit class) parses;
        // `path` is an alias for `glob`.
        let cfg = parse(
            r#"
            [[home.policy]]
            path = ".claude/history.jsonl"
            strategy = "union"
            "#,
        );
        assert_eq!(
            cfg.home.policy.unwrap(),
            vec![PolicyRule {
                glob: ".claude/history.jsonl".to_string(),
                class: None,
                strategy: Some(MergeStrategy::Union),
            }]
        );
    }

    #[test]
    fn policy_rejects_invalid_strategy_and_unknown_field() {
        assert!(
            toml::from_str::<Raw>("[[home.policy]]\nglob = \".x\"\nstrategy = \"bogus\"\n")
                .is_err(),
            "an unknown strategy value must fail to parse"
        );
        assert!(
            toml::from_str::<Raw>("[[home.policy]]\nglob = \".x\"\nclas = \"state\"\n").is_err(),
            "a misspelled field must fail to parse (deny_unknown_fields)"
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
    fn skeletons_parse_and_yield_defaults() {
        // The commented-out skeletons must stay valid TOML that changes
        // nothing until the user uncomments a line.
        let g: Raw = toml::from_str(GLOBAL_SKELETON).unwrap();
        let p: Raw = toml::from_str(PROJECT_SKELETON).unwrap();
        let cfg = merge(g, p);
        assert_eq!(cfg, merge(Raw::default(), Raw::default()));
    }
}
