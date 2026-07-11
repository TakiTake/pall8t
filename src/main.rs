use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pall8t::{config, container, herdr, home, image, repos, worktree};
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
    /// and the project's .pall8t/config.toml skeleton
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
    /// herdr (the terminal agent multiplexer) integration helpers
    Herdr {
        #[command(subcommand)]
        cmd: HerdrCmd,
    },
}

#[derive(Subcommand)]
enum HerdrCmd {
    /// Check whether pall8t can see and reach the herdr pane it's running
    /// under (env vars, socket, `herdr` binary) — read-only, never mutates
    /// anything
    Doctor {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HomeCmd {
    /// Harvest finished isolated runs into the inbox (also runs lazily on
    /// the next `pall8t run`)
    Harvest {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
    /// List pending changesets awaiting promotion
    Inbox {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
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
    /// List the base's revision history, newest first (FR-7)
    Log {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
    /// Show what one revision changed (FR-7)
    Diff { seq: u64 },
    /// Restore the base to a prior revision (itself recorded as a new,
    /// undoable revision) (FR-7)
    Rollback { seq: u64 },
    /// List isolated-mode instances: running, finished (awaiting harvest),
    /// or suspicious/orphaned (FR-9)
    Ls {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
    /// Remove an instance without harvesting it (FR-9)
    Rm {
        run: String,
        /// Remove even if the forker pid looks alive. Safe if that's pid
        /// recycling (discards the run's unharvested changes); if the run
        /// IS still live, its instance root may be bind-mounted as
        /// /home/dev inside a running container, and this corrupts that
        /// container's home out from under it.
        #[arg(long)]
        force: bool,
    },
    /// Clean orphaned tombstones, prune old revisions, and warn about
    /// stale (never auto-delete) inbox changesets (FR-9)
    Gc {
        /// Machine-readable output (for herdr etc.)
        #[arg(long)]
        json: bool,
    },
}

/// Exit codes stable across the `home` family (FR-11): `0` success, `1`
/// error, `2` unresolved merge conflicts (`promote`/`merge`) — distinct from
/// `1` so scripts/herdr can tell "needs human conflict resolution" apart
/// from any other failure without parsing the message.
const EXIT_CONFLICT: u8 = 2;

/// Marks an anyhow error as a conflict outcome (see [`EXIT_CONFLICT`]) while
/// keeping today's human-readable message and the `?`-based control flow
/// every other command uses.
#[derive(Debug)]
struct ConflictError(String);

impl std::fmt::Display for ConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConflictError {}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            if e.downcast_ref::<ConflictError>().is_some() {
                std::process::ExitCode::from(EXIT_CONFLICT)
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init => cmd_init(),
        Cmd::Run { command } => cmd_run(command),
        Cmd::Build => cmd_build(),
        Cmd::Ls { json } => cmd_ls(json),
        Cmd::Exec { id, command } => cmd_exec(&id, &command),
        Cmd::Stop { id } => cmd_stop(&id),
        Cmd::Home { cmd } => cmd_home(cmd),
        Cmd::Herdr { cmd } => cmd_herdr(&cmd),
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
fn exec_container(argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("container").args(argv).exec();
    Err(anyhow!(err).context("failed to exec `container`"))
}

/// Serializes and prints one line of JSON — the common tail of every
/// `--json` arm across the `home` subcommand family (FR-11).
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
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

    let herdr_env = herdr::detect();
    // An explicit `-- <cmd>` override is user intent and bypasses the
    // configured command entirely, so herdr's tmux-wrapper override only
    // ever applies to the configured default.
    let command = if cli_command.is_empty() {
        herdr::maybe_override_for_herdr(cfg.command.clone(), herdr_env.is_some())
    } else {
        cli_command
    };
    if let Some(env) = &herdr_env {
        // Cosmetic sidebar identity — never worth failing the run over.
        if let Err(e) = herdr::report_metadata(env) {
            eprintln!("pall8t: warning: could not report herdr pane metadata: {e:#}");
        }
    }
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
    exec_container(&container::run_argv(&spec))
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
        print_json(&items)?;
    } else {
        for c in containers {
            println!("{}\t{}", c.name, c.state.as_str());
        }
    }
    Ok(())
}

fn cmd_exec(id: &str, command: &[String]) -> Result<()> {
    if command.is_empty() {
        return Err(anyhow!(
            "no command given — usage: pall8t exec <id> -- <cmd>…"
        ));
    }
    ensure_container_system()?;
    // The container's own initial workdir (the workspace) — best-effort;
    // without it the command runs in the image WORKDIR.
    let workdir = container::workdir(id);
    exec_container(&container::exec_argv(
        id,
        command,
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

/// Connect-only probe (no request sent — `doctor` must not have side
/// effects): true if something is listening on `path`.
fn herdr_socket_reachable(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn cmd_herdr(cmd: &HerdrCmd) -> Result<()> {
    match cmd {
        HerdrCmd::Doctor { json } => {
            let snap = herdr::DoctorSnapshot::from_process_env();
            let socket_reachable = snap
                .socket_path
                .as_deref()
                .is_some_and(herdr_socket_reachable);
            let bin_resolvable = herdr::bin_resolvable(snap.herdr_bin());
            let checks = herdr::doctor_checks(&snap, socket_reachable, bin_resolvable);
            if *json {
                print_json(&checks)?;
            } else {
                for c in &checks {
                    let mark = if c.ok { "✓" } else { "✗" };
                    println!("{mark} {:<16} {}", c.name, c.detail);
                }
            }
            Ok(())
        }
    }
}

/// The host path to mount at `/home/dev`. `shared` mode is byte-for-byte
/// today's behavior: the base home, unchanged. `isolated` mode first
/// harvests any finished runs (lazy, FR-8), then forks a private instance
/// for this run (FR-1).
fn home_for_run(cfg: &config::Config, run_name: &str, cwd: &Path) -> Result<std::path::PathBuf> {
    match cfg.home.mode {
        home::HomeMode::Shared => container::home_mount(),
        home::HomeMode::Isolated => {
            warn_home_config_issues(&cfg.home);
            match home::harvest_finished(&cfg.home.policy, cfg.home.revisions_keep) {
                Ok(runs) if !runs.is_empty() => {
                    eprintln!(
                        "pall8t: harvested {} finished run(s) — see `pall8t home inbox`",
                        runs.len()
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("pall8t: warning: could not harvest finished runs: {e:#}"),
            }
            let instance = home::fork_instance(run_name, cwd, &cfg.home.policy)?;
            eprintln!("pall8t: isolated home — forked a private instance for this run");
            Ok(instance)
        }
    }
}

/// The cwd project's `[home]` config, for the standalone `home` commands.
/// Best-effort: a missing/broken config falls back to the built-in defaults
/// rather than failing the command.
fn cwd_home_config() -> config::HomeConfig {
    let cfg = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::load(&cwd).ok())
        .map(|c| c.home)
        .unwrap_or_default();
    warn_home_config_issues(&cfg);
    cfg
}

/// Surfaces `[home]` misconfigurations once per CLI invocation: nonsensical
/// `[[home.policy]]` rules (see [`home::validate_policy`]), and
/// `revisions_keep = 0` (which `home.rs`'s `prune_revisions` silently
/// clamps to 1 regardless of whether this fires — warning there instead
/// would print once per recorded revision, for the life of the project).
/// The two share one entry point since they're always checked together, at
/// the same two call sites, for the same reason: a per-project setting
/// worth telling the user about exactly once, not on every mutation.
fn warn_home_config_issues(cfg: &config::HomeConfig) {
    for w in home::validate_policy(&cfg.policy) {
        eprintln!("pall8t: warning: {w}");
    }
    if cfg.revisions_keep == 0 {
        eprintln!(
            "pall8t: warning: [home] revisions_keep = 0 would erase all history — using 1 instead"
        );
    }
}

fn cmd_home(cmd: HomeCmd) -> Result<()> {
    match cmd {
        HomeCmd::Harvest { json } => {
            let cfg = cwd_home_config();
            let runs = home::harvest_finished(&cfg.policy, cfg.revisions_keep)?;
            if json {
                print_json(&runs)?;
            } else if runs.is_empty() {
                println!("no finished runs to harvest");
            } else {
                println!("harvested {} run(s):", runs.len());
                for r in runs {
                    println!("  {r}");
                }
            }
            Ok(())
        }
        HomeCmd::Inbox { json } => {
            let changesets = home::list_changesets()?;
            if json {
                print_json(&changesets)?;
            } else if changesets.is_empty() {
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
            let cfg = cwd_home_config();
            let outcome = home::promote(&run, &paths, &cfg.policy, cfg.revisions_keep)?;
            for p in &outcome.promoted {
                println!("promoted {p}");
            }
            if outcome.conflicts.is_empty() {
                Ok(())
            } else {
                // Base left untouched for these; surface for resolution
                // (FR-5) and exit non-zero (2 — FR-11) so scripts notice.
                Err(anyhow::Error::new(ConflictError(format!(
                    "{} path(s) conflicted and stay staged (base unchanged): {}",
                    outcome.conflicts.len(),
                    outcome.conflicts.join(", ")
                ))))
            }
        }
        HomeCmd::Drop { run, paths } => {
            home::drop_changeset(&run, &paths)?;
            println!("dropped {run}");
            Ok(())
        }
        HomeCmd::Merge { run } => cmd_home_merge(run.as_deref()),
        HomeCmd::Log { json } => cmd_home_log(json),
        HomeCmd::Diff { seq } => {
            print!("{}", home::diff(seq, &cwd_home_config().policy)?);
            Ok(())
        }
        HomeCmd::Rollback { seq } => {
            let cfg = cwd_home_config();
            home::rollback(seq, &cfg.policy, cfg.revisions_keep)?;
            println!("rolled back to revision {seq} (recorded as a new revision)");
            Ok(())
        }
        HomeCmd::Ls { json } => cmd_home_ls(json),
        HomeCmd::Rm { run, force } => {
            home::rm(&run, force)?;
            println!("removed {run}");
            Ok(())
        }
        HomeCmd::Gc { json } => cmd_home_gc(json),
    }
}

fn cmd_home_merge(run: Option<&str>) -> Result<()> {
    let cfg = cwd_home_config();
    let report = home::merge(run, &cfg.policy, cfg.revisions_keep)?;
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
        // Exit non-zero (2 — FR-11), like `promote`, and point at per-path
        // resolution.
        Some(step) => Err(anyhow::Error::new(ConflictError(format!(
            "{} path(s) in {} conflicted and stay staged (base unchanged); later \
             changesets left untouched. Resolve with `pall8t home promote {} <path>` or \
             `pall8t home drop {} <path>`, then re-run `pall8t home merge`: {}",
            step.conflicts.len(),
            step.run,
            step.run,
            step.run,
            step.conflicts.join(", ")
        )))),
    }
}

fn cmd_home_log(json: bool) -> Result<()> {
    let revisions = home::list_revisions()?;
    if json {
        print_json(&revisions)?;
    } else if revisions.is_empty() {
        println!("no revisions yet");
    } else {
        for r in revisions {
            println!(
                "{:>6}  {:<8}  {}  {} path(s)  {}",
                r.seq,
                r.op.to_string(),
                home::fmt_epoch(r.created),
                r.paths,
                r.runs.join(", ")
            );
        }
    }
    Ok(())
}

fn cmd_home_ls(json: bool) -> Result<()> {
    let instances = home::list_instances()?;
    if json {
        print_json(&instances)?;
    } else if instances.is_empty() {
        println!("no instances");
    } else {
        for i in instances {
            let flag = if i.suspicious {
                "  ! implausibly old for \"running\" — pid may have been recycled; \
                 see `pall8t home rm --force`"
            } else {
                ""
            };
            println!(
                "{}\t{}\t{}\t{}{}",
                i.run,
                i.status,
                home::fmt_epoch(i.created),
                i.workspace,
                flag
            );
        }
    }
    Ok(())
}

fn cmd_home_gc(json: bool) -> Result<()> {
    let cfg = cwd_home_config();
    let report = home::gc(cfg.revisions_keep, cfg.inbox_ttl_days)?;
    if json {
        print_json(&report)?;
    } else {
        println!(
            "removed {} partial fork(s), {} orphaned tombstone(s), {} orphaned revision \
             snapshot(s); pruned {} old revision(s)",
            report.removed_partials,
            report.removed_discards,
            report.removed_revision_snapshots,
            report.revisions_pruned
        );
        for c in &report.stale_changesets {
            println!(
                "pall8t: warning: changeset {} is {} day(s) old — `pall8t home promote {}` or \
                 `pall8t home drop {}` to resolve it (never auto-deleted)",
                c.run, c.age_days, c.run, c.run
            );
        }
    }
    Ok(())
}

/// FR-6: create `~/.pall8t/home`, config skeletons, and the default
/// Containerfile. The default Containerfile is written to
/// `~/.pall8t/Containerfile`, NOT the project's `.pall8t/Containerfile` —
/// that path is [`image::resolve`]'s per-project probe, so writing one
/// there on every `init` would opt every project into its own image build
/// instead of sharing the default; copy `~/.pall8t/Containerfile` into
/// `.pall8t/Containerfile` only to actually customize it for a project.
/// Never overwrites an existing file.
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
    write_if_missing(&config::project_path(&cwd), config::PROJECT_SKELETON)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit-code mapping (FR-11) hinges on `main`'s
    /// `downcast_ref::<ConflictError>()` check finding the marker error
    /// through anyhow's type erasure; this is the part of that mapping
    /// exercisable without spawning a real `pall8t` process end to end.
    #[test]
    fn conflict_error_downcasts_through_anyhow_but_other_errors_dont() {
        let conflict = anyhow::Error::new(ConflictError("2 conflicts".to_string()));
        assert!(conflict.downcast_ref::<ConflictError>().is_some());
        assert_eq!(conflict.to_string(), "2 conflicts");

        let generic = anyhow!("some other failure");
        assert!(generic.downcast_ref::<ConflictError>().is_none());
    }
}
