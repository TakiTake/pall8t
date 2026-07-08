use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Per-project config file name, looked up in the current directory.
pub const PROJECT_FILE: &str = "pall8t.toml";

/// Merged, fully-defaulted configuration for one invocation: the global
/// `~/.pall8t/config.toml` overlaid by the project's `pall8t.toml`
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
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RepoEntry {
    /// Host path of a reference repository; duplicated via
    /// `git clone --local` and the copy mounted at this path (FR-4).
    pub source: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    container: RawContainer,
    #[serde(default)]
    run: RawRun,
    repos: Option<Vec<RepoEntry>>,
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
    let project = read_raw(&project_dir.join(PROJECT_FILE))?.unwrap_or_default();
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
    }
}

/// Skeleton written by `pall8t init` as `~/.pall8t/config.toml`.
pub const GLOBAL_SKELETON: &str = r#"# pall8t global configuration. Per-project pall8t.toml overrides these
# values field by field.

[container]
# cpus = 4
# memory = "8g"
# containerfile = "/absolute/path/to/Containerfile"

[run]
# Command run by `pall8t run`. --dangerously-skip-permissions is NOT in
# the default; add it here explicitly if you want it.
# command = ["claude"]
"#;

/// Skeleton written by `pall8t init` as `./pall8t.toml`.
pub const PROJECT_SKELETON: &str = r#"# pall8t project configuration. Fields set here override
# ~/.pall8t/config.toml.

[container]
# cpus = 4
# memory = "8g"
# Containerfile used for this project's image. Default: ./Containerfile
# if present, else the built-in default image.
# containerfile = "Containerfile"

[run]
# command = ["claude"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Raw {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn defaults_when_both_empty() {
        let cfg = merge(Raw::default(), Raw::default());
        assert_eq!(cfg.cpus, 4);
        assert_eq!(cfg.memory, "8g");
        assert_eq!(cfg.containerfile, None);
        assert_eq!(cfg.command, vec!["claude".to_string()]);
        assert!(cfg.repos.is_empty());
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
            r#"
            [container]
            cpus = 2
            "#,
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
