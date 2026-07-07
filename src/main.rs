use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pall8t::{config, container, image, repos, worktree};
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
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init => cmd_init(),
        Cmd::Run { command } => cmd_run(command),
        Cmd::Build => cmd_build(),
        Cmd::Ls { json } => cmd_ls(json),
        Cmd::Exec { id, command } => cmd_exec(&id, command),
        Cmd::Stop { id } => cmd_stop(&id),
    }
}

fn ensure_container_system() -> Result<()> {
    if !container::cli_available() {
        return Err(anyhow!(
            "the `container` CLI is not available — install apple/container from \
             https://github.com/apple/container/releases"
        ));
    }
    if !container::system_running() {
        eprintln!("pall8t: starting the container system service…");
        container::system_start()?;
    }
    Ok(())
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
    ensure_container_system()?;
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the current directory")?;
    let cfg = config::load(&cwd)?;
    let (uid, gid) = container::host_ids();

    let resolved = image::ensure_built(&cwd, &cfg, uid, gid, false)?;

    let mut mounts = vec![container::Mount::identity(cwd.clone())];
    let mut protected = vec![cwd.clone()];
    if let Some(git_dir) = worktree::main_git_dir(&cwd) {
        eprintln!(
            "pall8t: git worktree detected — also mounting {}",
            git_dir.display()
        );
        protected.push(git_dir.clone());
        mounts.push(container::Mount::identity(git_dir));
    }
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
        host: container::home_mount()?,
        dest: "/home/dev".into(),
    });

    let command = if cli_command.is_empty() {
        cfg.command.clone()
    } else {
        cli_command
    };
    let spec = container::RunSpec {
        name: container::run_name(&cwd),
        image: resolved.tag,
        workdir: cwd,
        mounts,
        cpus: cfg.cpus,
        memory: cfg.memory,
        uid,
        gid,
        tty: std::io::stdin().is_terminal(),
        command,
    };
    exec_container(container::run_argv(&spec))
}

fn cmd_build() -> Result<()> {
    ensure_container_system()?;
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the current directory")?;
    let cfg = config::load(&cwd)?;
    let (uid, gid) = container::host_ids();
    let resolved = image::ensure_built(&cwd, &cfg, uid, gid, true)?;
    println!("built {}", resolved.tag);
    Ok(())
}

fn cmd_ls(json: bool) -> Result<()> {
    ensure_container_system()?;
    let containers = container::list_pall8t()?;
    if json {
        let items: Vec<serde_json::Value> = containers
            .iter()
            .map(|(name, state)| serde_json::json!({ "name": name, "status": state.as_str() }))
            .collect();
        println!("{}", serde_json::to_string(&items)?);
    } else {
        for (name, state) in containers {
            println!("{name}\t{}", state.as_str());
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
    let tty = std::io::stdin().is_terminal();
    // The container's own initial workdir (the workspace) — best-effort;
    // without it the command runs in the image WORKDIR.
    let workdir = container::workdir(id);
    exec_container(container::exec_argv(id, &command, tty, workdir.as_deref()))
}

fn cmd_stop(id: &str) -> Result<()> {
    ensure_container_system()?;
    container::stop(id)?;
    println!("stopped {id}");
    Ok(())
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
    if path.exists() {
        println!("exists, skipped: {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))?;
    println!("created:         {}", path.display());
    Ok(())
}
