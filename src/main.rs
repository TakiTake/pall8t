use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pall8t::{config, container, home, image, repos, worktree};
use std::io::IsTerminal;
use std::path::Path;

/// Run AI coding agents in apple/container sandboxes. Headless: pall8t is
/// a well-behaved foreground CLI for tmux/herdr to spawn — TTY
/// passthrough, signal forwarding, correct exit codes (ADR-0006).
#[derive(Parser)]
#[command(name = "pall8t", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate ~/.pall8t (home, config skeleton, default Containerfile)
    /// and the project's pall8t.toml skeleton
    Init,
    /// Rebuild the image if the Containerfile changed, then run the agent
    /// in the sandbox (foreground, cwd mounted as the workspace)
    Run {
        /// Command to run instead of the configured one (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Build the image explicitly (unconditionally)
    Build,
    /// List containers started by pall8t
    Ls {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
    /// Run a command inside a running container
    Exec {
        id: String,
        /// Command to run (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Stop a container
    Stop { id: String },
    /// Inspect and fold back changes from isolated-home runs
    Home {
        #[command(subcommand)]
        cmd: HomeCmd,
    },
}

#[derive(Subcommand)]
enum HomeCmd {
    /// Harvest finished isolated runs into the inbox (also runs lazily on
    /// the next `pall8t run`)
    Harvest,
    /// List pending changesets awaiting promotion
    Inbox,
    /// Show what a run changed
    Show { run: String },
    /// Merge a run's changes (or selected paths) into the base home
    Promote {
        run: String,
        /// Limit to these `$HOME`-relative paths (default: all)
        paths: Vec<String>,
    },
    /// Discard a run's changeset (or selected paths)
    Drop {
        run: String,
        /// Limit to these `$HOME`-relative paths (default: the whole run)
        paths: Vec<String>,
    },
    /// harvest + show + promote-all in one step: fold pending runs (or one
    /// <run>) into the base, printing what each changed
    Merge {
        /// Merge just this run (default: every pending run)
        run: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init => cmd_init(),
        Cmd::Run { command } => cmd_run(command),
        Cmd::Build => cmd_build(),
        Cmd::Ls { json } => cmd_ls(json),
        Cmd::Exec { id, command } => cmd_exec(&id, command),
        Cmd::Stop { id } => cmd_stop(&id),
        Cmd::Home { cmd } => cmd_home(cmd),
    }
}

fn ensure_container_system() -> Result<()> {
    match container::system_status() {
        container::SystemStatus::Running => Ok(()),
        container::SystemStatus::Stopped => {
            eprintln!("pall8t: starting the container system service…");
            container::system_start()
        }
        container::SystemStatus::CliMissing => Err(anyhow!(
            "the `container` CLI is not available — install apple/container from \
             https://github.com/apple/container/releases"
        )),
    }
}

/// apple/container 1.0.0 fails outright when `-t` is requested without a
/// terminal, so every command derives its TTY request from here.
fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Shared `run`/`build` preamble: container system up, canonical cwd,
/// merged config, host ids, image resolved (and built if missing/forced).
fn workspace_image(
    force: bool,
) -> Result<(
    std::path::PathBuf,
    config::Config,
    u32,
    u32,
    image::ResolvedImage,
)> {
    ensure_container_system()?;
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the current directory")?;
    let cfg = config::load(&cwd)?;
    let (uid, gid) = container::host_ids();
    let resolved = image::ensure_built(&cwd, &cfg, uid, gid, force)?;
    Ok((cwd, cfg, uid, gid, resolved))
}

/// Replaces this process with `container <argv>`: the cleanest possible
/// TTY passthrough — the kernel delivers signals straight to the
/// `container` CLI and the exit code needs no forwarding, because pall8t
/// is no longer there (NFR-4).
fn exec_container(argv: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("container").args(&argv).exec();
    Err(anyhow!(err).context("failed to exec `container`"))
}

fn cmd_run(cli_command: Vec<String>) -> Result<()> {
    let (cwd, cfg, uid, gid, resolved) = workspace_image(false)?;
    let run_name = container::run_name(&cwd);

    let mut mounts = vec![container::Mount::identity(cwd.clone())];
    if let Some(git_dir) = worktree::main_git_dir(&cwd) {
        eprintln!(
            "pall8t: git worktree detected — also mounting {}",
            git_dir.display()
        );
        mounts.push(container::Mount::identity(git_dir));
    }
    // The live identity-mounted paths assembled so far (cwd, worktree git
    // dir) are exactly what a reference-repo mount must never shadow.
    let protected: Vec<_> = mounts.iter().map(|m| m.host.clone()).collect();
    for rm in repos::prepare(&cfg.repos, &protected)? {
        eprintln!(
            "pall8t: reference repo {} (writes hit the copy {})",
            rm.source.display(),
            rm.clone.display()
        );
        mounts.push(container::Mount {
            host: rm.clone,
            dest: rm.source,
        });
    }
    mounts.push(container::Mount {
        host: home_for_run(&cfg, &run_name, &cwd)?,
        dest: "/home/dev".into(),
    });

    let command = if cli_command.is_empty() {
        cfg.command.clone()
    } else {
        cli_command
    };
    let spec = container::RunSpec {
        name: run_name,
        image: resolved.tag,
        workdir: cwd,
        mounts,
        cpus: cfg.cpus,
        memory: cfg.memory,
        uid,
        gid,
        tty: stdin_is_tty(),
        command,
    };
    exec_container(container::run_argv(&spec))
}

fn cmd_build() -> Result<()> {
    let (_, _, _, _, resolved) = workspace_image(true)?;
    println!("built {}", resolved.tag);
    Ok(())
}

fn cmd_ls(json: bool) -> Result<()> {
    ensure_container_system()?;
    let containers = container::list_pall8t()?;
    if json {
        let items: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "status": c.state.as_str() }))
            .collect();
        println!("{}", serde_json::to_string(&items)?);
    } else {
        for c in containers {
            println!("{}\t{}", c.name, c.state.as_str());
        }
    }
    Ok(())
}

fn cmd_exec(id: &str, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        return Err(anyhow!(
            "no command given — usage: pall8t exec <id> -- <cmd>…"
        ));
    }
    ensure_container_system()?;
    // The container's own initial workdir (the workspace) — best-effort;
    // without it the command runs in the image WORKDIR.
    let workdir = container::workdir(id);
    exec_container(container::exec_argv(
        id,
        &command,
        stdin_is_tty(),
        workdir.as_deref(),
    ))
}

fn cmd_stop(id: &str) -> Result<()> {
    ensure_container_system()?;
    container::stop(id)?;
    println!("stopped {id}");
    Ok(())
}

/// The host path to mount at `/home/dev`. `shared` mode is byte-for-byte
/// today's behavior: the base home, unchanged. `isolated` mode first
/// harvests any finished runs (lazy, FR-8), then forks a private instance
/// for this run (FR-1).
fn home_for_run(cfg: &config::Config, run_name: &str, cwd: &Path) -> Result<std::path::PathBuf> {
    match cfg.home.mode {
        home::HomeMode::Shared => container::home_mount(),
        home::HomeMode::Isolated => {
            match home::harvest_finished(&cfg.home.policy) {
                Ok(runs) if !runs.is_empty() => {
                    eprintln!(
                        "pall8t: harvested {} finished run(s) — see `pall8t home inbox`",
                        runs.len()
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("pall8t: warning: could not harvest finished runs: {e:#}"),
            }
            let instance = home::fork_instance(run_name, cwd)?;
            eprintln!("pall8t: isolated home — forked a private instance for this run");
            Ok(instance)
        }
    }
}

/// The cwd project's policy overrides, for the standalone `home` commands
/// that reclassify (harvest, merge). Best-effort: a missing/broken config
/// falls back to the built-in defaults rather than failing the command.
fn cwd_home_policy() -> Vec<config::PolicyRule> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| config::load(&cwd).ok())
        .map(|c| c.home.policy)
        .unwrap_or_default()
}

fn cmd_home(cmd: HomeCmd) -> Result<()> {
    match cmd {
        HomeCmd::Harvest => {
            let runs = home::harvest_finished(&cwd_home_policy())?;
            if runs.is_empty() {
                println!("no finished runs to harvest");
            } else {
                println!("harvested {} run(s):", runs.len());
                for r in runs {
                    println!("  {r}");
                }
            }
            Ok(())
        }
        HomeCmd::Inbox => {
            let changesets = home::list_changesets()?;
            if changesets.is_empty() {
                println!("inbox empty");
            } else {
                for c in changesets {
                    println!("{}\t{} path(s)\t{}", c.run, c.entries, c.workspace);
                }
            }
            Ok(())
        }
        HomeCmd::Show { run } => {
            print!("{}", home::show(&run)?);
            Ok(())
        }
        HomeCmd::Promote { run, paths } => {
            let outcome = home::promote(&run, &paths)?;
            for p in &outcome.promoted {
                println!("promoted {p}");
            }
            if outcome.conflicts.is_empty() {
                Ok(())
            } else {
                // Base left untouched for these; surface for resolution
                // (FR-5) and exit non-zero so scripts notice.
                Err(anyhow!(
                    "{} path(s) conflicted and stay staged (base unchanged): {}",
                    outcome.conflicts.len(),
                    outcome.conflicts.join(", ")
                ))
            }
        }
        HomeCmd::Drop { run, paths } => {
            home::drop_changeset(&run, &paths)?;
            println!("dropped {run}");
            Ok(())
        }
        HomeCmd::Merge { run } => cmd_home_merge(run),
    }
}

fn cmd_home_merge(run: Option<String>) -> Result<()> {
    let report = home::merge(run.as_deref(), &cwd_home_policy())?;
    if report.steps.is_empty() {
        // Harvest may still have had side effects (secret/state write-back)
        // even with no knowledge changeset to promote — say so honestly.
        if report.harvested.is_empty() {
            println!("nothing to merge");
        } else {
            println!(
                "harvested {} run(s); no knowledge changes to promote",
                report.harvested.len()
            );
        }
        return Ok(());
    }
    let mut conflicted: Option<&home::MergeStep> = None;
    for step in &report.steps {
        // Show what is being folded in, then what landed — the record of the
        // merge (there is no confirmation prompt; `merge` is itself explicit).
        print!("{}", step.shown);
        for p in &step.promoted {
            println!("promoted {p}");
        }
        if !step.conflicts.is_empty() {
            conflicted = Some(step);
        }
    }
    match conflicted {
        None => Ok(()),
        // Processing stopped at this changeset (FR-5); later ones stay staged.
        // Exit non-zero, like `promote`, and point at per-path resolution.
        Some(step) => Err(anyhow!(
            "{} path(s) in {} conflicted and stay staged (base unchanged); later \
             changesets left untouched. Resolve with `pall8t home promote {} <path>` or \
             `pall8t home drop {} <path>`, then re-run `pall8t home merge`: {}",
            step.conflicts.len(),
            step.run,
            step.run,
            step.run,
            step.conflicts.join(", ")
        )),
    }
}

/// FR-6: create `~/.pall8t/home`, config skeletons, and the default
/// Containerfile (in `~/.pall8t`, NOT the project — a `./Containerfile`
/// would flip [`image::resolve`]'s priority to a per-project base and give
/// every init'ed project its own byte-identical image; copy
/// `~/.pall8t/Containerfile` into the project only to actually customize
/// it). Never overwrites an existing file.
fn cmd_init() -> Result<()> {
    let home = container::home_mount()?;
    println!("container home:  {}", home.display());

    let global = config::global_path()?;
    write_if_missing(&global, config::GLOBAL_SKELETON)?;
    write_if_missing(
        &container::default_containerfile_location()?,
        container::DEFAULT_CONTAINERFILE,
    )?;

    let cwd = std::env::current_dir()?;
    write_if_missing(&cwd.join(config::PROJECT_FILE), config::PROJECT_SKELETON)?;

    println!(
        "\nFirst use: the agent must log in once inside the container, e.g.\n\
         \n    pall8t run\n\
         \nCredentials persist in {} — the host's own agent config (~/.claude etc.)\n\
         is never touched.",
        home.display()
    );
    Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    if pall8t::util::ensure_file(path, content)? {
        println!("created:         {}", path.display());
    } else {
        println!("exists, skipped: {}", path.display());
    }
    Ok(())
}
