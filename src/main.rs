use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pall8t::{config, container, herdr, image, repos, worktree};
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
    /// Host-side relay serving the sandbox herdr bridge (spawned by
    /// `pall8t run`, never by hand — see ADR-0007)
    #[command(hide = true)]
    Relay {
        /// Host herdr API socket to forward to
        #[arg(long)]
        socket: std::path::PathBuf,
        /// Sandbox container whose IP is the only accepted peer
        #[arg(long)]
        container: String,
        /// Policy mode: full | readonly
        #[arg(long)]
        mode: String,
        /// Audit log file
        #[arg(long)]
        log: std::path::PathBuf,
    },
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
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
        Cmd::Herdr { cmd } => cmd_herdr(&cmd),
    }
}

fn ensure_container_system() -> Result<()> {
    match container::system_status() {
        container::SystemStatus::Running => {
            warn_if_container_outdated();
            Ok(())
        }
        container::SystemStatus::Stopped => {
            eprintln!("pall8t: starting the container system service…");
            // `?` before the warning, deliberately: a failed start aborts the
            // command, and stacking "your runtime is old" onto the error the
            // user has to act on buys nothing — the version is not why the
            // service failed. Ordering it this way makes the wrong order
            // unrepresentable rather than merely unwritten.
            container::system_start()?;
            warn_if_container_outdated();
            Ok(())
        }
        // No version probe here: with no CLI to ask, the install message is
        // the whole answer.
        container::SystemStatus::CliMissing => Err(anyhow!(
            "the `container` CLI is not available — install apple/container from \
             https://github.com/apple/container/releases"
        )),
    }
}

/// Warns once per invocation when the installed apple/container predates
/// the version pall8t's sandbox boundary depends on (see
/// [`container::version_warning_for_installed`]). A warning, never an
/// error: pall8t still works on older runtimes, and refusing to run would
/// be a worse trade than telling the user what is weaker. Goes to stderr,
/// so `ls --json`'s stdout stays clean JSON.
///
/// Deliberately not surfaced in `pall8t herdr doctor`: that diagnostic is
/// scoped to the herdr bridge (env, socket, Linux binary), reports a JSON
/// shape herdr itself consumes, and never launches a container. This
/// warning instead rides the runtime path — every command that actually
/// talks to apple/container reaches it.
fn warn_if_container_outdated() {
    if let Some(msg) = container::version_warning_for_installed() {
        eprintln!("{msg}");
    }
}

/// apple/container 1.0.0 fails outright when `-t` is requested without a
/// terminal, so every command derives its TTY request from here.
fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Shared `run`/`build` preamble: canonical cwd, merged config, container
/// system up, host ids, image resolved (and built if missing/forced).
///
/// Config is read *before* the container check so a config problem is
/// reported even on a machine where the `container` CLI is missing — the
/// user fixing one shouldn't have to fix the other first to hear about it.
fn workspace_image(
    force: bool,
) -> Result<(
    std::path::PathBuf,
    config::Config,
    u32,
    u32,
    image::ResolvedImage,
)> {
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the current directory")?;
    let cfg = config::load(&cwd)?;
    warn_deprecations(&cfg);
    ensure_container_system()?;
    let (uid, gid) = container::host_ids();
    let resolved = image::ensure_built(&cwd, &cfg, uid, gid, force)?;
    Ok((cwd, cfg, uid, gid, resolved))
}

/// Surfaces settings the loaded config still declares but pall8t no longer
/// honors, once per invocation, on stderr — so a `--json` arm's stdout stays
/// machine-readable. Every command that loads a config calls this: a
/// diagnostic command that stayed quiet about an ignored setting would be
/// the one place a confused user is most likely to look.
fn warn_deprecations(cfg: &config::Config) {
    for d in &cfg.deprecations {
        eprintln!("pall8t: warning: {d}");
    }
}

/// Replaces this process with `container <argv>`: the cleanest possible
/// TTY passthrough — the kernel delivers signals straight to the
/// `container` CLI and the exit code needs no forwarding, because pall8t
/// is no longer there (NFR-4). `arg0` overrides only argv[0]: herdr
/// identifies a pane's agent from the host process tree by argv0 basename,
/// so naming the process after the sandboxed agent is what lets herdr
/// track its state (see `herdr::agent_hint`). With an arg0 set, the exec
/// target is resolved through any Homebrew wrapper script first — the
/// wrapper's inner `exec` would otherwise rewrite argv[0] and destroy the
/// hint (see `container::client_exec_target`) — and `HERDR_AGENT` is set
/// on the process so a future herdr macOS env hint can pick the name up
/// even where argv0 can't survive.
fn exec_container(argv: &[String], arg0: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = match arg0 {
        Some(arg0) => {
            let (target, env) = container::client_exec_target();
            let mut cmd = std::process::Command::new(target);
            cmd.arg0(arg0);
            cmd.envs(env);
            cmd.env("HERDR_AGENT", arg0);
            cmd
        }
        None => std::process::Command::new("container"),
    };
    let err = cmd.args(argv).exec();
    Err(anyhow!(err).context("failed to exec `container`"))
}

/// Serializes and prints one line of JSON — the tail of every `--json`
/// arm.
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
        host: container::home_mount()?,
        dest: "/home/dev".into(),
    });

    let herdr_env = herdr::detect();
    // An explicit `-- <cmd>` override is user intent and bypasses the
    // configured command entirely, so herdr's tmux-wrapper override only
    // ever applies to the configured default.
    let mut command = if cli_command.is_empty() {
        herdr::maybe_override_for_herdr(cfg.command.clone(), herdr_env.is_some())
    } else {
        cli_command
    };
    let herdr_agent = herdr_env
        .as_ref()
        .and_then(|env| herdr::announce_pane_identity(env, &command));
    // The bridge (ADR-0007) makes the herdr CLI work inside the sandbox:
    // relay + env + Linux binary mount + bootstrap wrap. Best-effort — a
    // bridge failure warns and the run proceeds without it.
    let mut env_vars = Vec::new();
    if let Some(env) = &herdr_env {
        match herdr::prepare_bridge(env, cfg.herdr.sandbox, &run_name) {
            Ok(Some(bridge)) => {
                eprintln!(
                    "pall8t: herdr bridge active ({}) — the sandboxed agent can reach \
                     this herdr session; audit log in ~/.pall8t/logs/",
                    cfg.herdr.sandbox.as_str()
                );
                mounts.extend(bridge.mounts);
                env_vars = bridge.env;
                command = herdr::wrap_command_for_bridge(command);
            }
            Ok(None) => {}
            Err(e) => eprintln!("pall8t: warning: herdr bridge disabled: {e:#}"),
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
        env: env_vars,
        command,
    };
    exec_container(&container::run_argv(&spec), herdr_agent.as_deref())
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
    // No herdr argv0 hint here: `exec` is the one-off/debug path, and an
    // ambient HERDR_AGENT would mislabel e.g. a plain `bash` as the agent.
    exec_container(
        &container::exec_argv(id, command, stdin_is_tty(), workdir.as_deref()),
        None,
    )
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
        HerdrCmd::Relay {
            socket,
            container,
            mode,
            log,
        } => pall8t::relay::run(socket, container, pall8t::relay::Mode::parse(mode)?, log),
        HerdrCmd::Doctor { json } => {
            let snap = herdr::DoctorSnapshot::from_process_env();
            let socket_reachable = snap
                .socket_path
                .as_deref()
                .is_some_and(herdr_socket_reachable);
            let bin_resolvable = herdr::bin_resolvable(snap.herdr_bin());
            let mut checks = herdr::doctor_checks(&snap, socket_reachable, bin_resolvable);
            let cfg = std::env::current_dir()
                .ok()
                .and_then(|cwd| config::load(&cwd).ok());
            if let Some(cfg) = &cfg {
                warn_deprecations(cfg);
            }
            let mode = cfg.map_or(config::HerdrSandbox::default(), |c| c.herdr.sandbox);
            let cached = herdr::cached_linux_herdr(snap.herdr_bin());
            checks.extend(herdr::bridge_checks(mode.as_str(), cached.as_deref()));
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
