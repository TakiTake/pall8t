use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pall8t::{config, container, herdr, image, mounts, naming, worktree};
use std::io::IsTerminal;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

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
        /// Mount every [[mounts]] entry read-only for this run,
        /// overriding each entry's own `readonly`. `--readonly=false`
        /// forces them all writable instead
        #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
        readonly: Option<bool>,
        /// Forward the host's SSH agent into the sandbox for this run,
        /// overriding `[container] ssh`. `--ssh=false` forces it off
        #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
        ssh: Option<bool>,
        /// Command to run instead of the configured one (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Build the image explicitly (unconditionally)
    Build {
        /// Bypass the builder's layer cache, re-running every RUN step —
        /// picks up "latest" fetches (the claude CLI npm install) that an
        /// unchanged Containerfile line would otherwise keep serving from
        /// cache
        #[arg(long)]
        no_cache: bool,
    },
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
    /// Names this run's herdr agent once herdr detects it, then exits
    /// (spawned by `pall8t run`, never by hand — see `naming`, issue #71)
    #[command(hide = true)]
    NameAgent {
        /// Pane whose agent to name
        #[arg(long)]
        pane: String,
        /// Name to give it
        #[arg(long)]
        name: String,
        /// Tab to keep in step with the name, when pall8t labeled it
        #[arg(long)]
        tab: Option<String>,
        /// herdr CLI to call
        #[arg(long, default_value = "herdr")]
        herdr_bin: String,
        /// Log file for what it did
        #[arg(long)]
        log: std::path::PathBuf,
    },
    /// Host-side relay serving the sandbox herdr bridge (spawned by
    /// `pall8t run`, never by hand — see ADR-0007)
    #[command(hide = true)]
    Relay {
        /// Host herdr API socket to forward to
        #[arg(long)]
        socket: std::path::PathBuf,
        /// Unix socket to listen on (mounted into the sandbox)
        #[arg(long)]
        listen: std::path::PathBuf,
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
        Cmd::Run {
            readonly,
            ssh,
            command,
        } => cmd_run(command, readonly, ssh),
        Cmd::Build { no_cache } => cmd_build(no_cache),
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
        // Deliberately *not* the install message. This is the probe
        // failing for a reason that says nothing about whether the
        // runtime is installed, and telling someone to install software
        // they already have sends them to fix the wrong thing.
        container::SystemStatus::ProbeFailed(e) => Err(anyhow!(
            "cannot check whether the `container` runtime is up: {e} — the \
             CLI may be there and unreachable right now (a descriptor or \
             process limit, or a `container` that is not executable), so \
             retry before reinstalling"
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

/// The half of the preamble that touches nothing outside this machine's
/// filesystem: where we are, and what the config says. Split out so a
/// launch can be *planned* — and rejected — before [`build_image`] starts
/// the runtime and builds anything (issue #90). Reading it first is also
/// what reports a config problem on a machine where the `container` CLI
/// is missing — the user fixing one shouldn't have to fix the other first
/// to hear about it.
fn workspace_config() -> Result<(std::path::PathBuf, config::Config)> {
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot resolve the current directory")?;
    let cfg = config::load(&cwd)?;
    warn_config_issues(&cfg);
    Ok((cwd, cfg))
}

/// The expensive half: runtime up, host ids, image resolved (and built if
/// missing/forced).
fn build_image(
    cwd: &Path,
    cfg: &config::Config,
    mode: image::BuildMode,
) -> Result<(u32, u32, image::ResolvedImage)> {
    ensure_container_system()?;
    let (uid, gid) = container::host_ids();
    let resolved = image::ensure_built(cwd, cfg, uid, gid, mode)?;
    Ok((uid, gid, resolved))
}

/// Surfaces settings the loaded config still declares but pall8t no longer
/// honors, once per invocation, on stderr — so a `--json` arm's stdout stays
/// machine-readable. Every command that loads a config calls this: a
/// diagnostic command that stayed quiet about an ignored setting would be
/// the one place a confused user is most likely to look.
fn warn_config_issues(cfg: &config::Config) {
    for warning in &cfg.warnings {
        eprintln!("pall8t: warning: {warning}");
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

/// Provenance for `pall8t ls --json` and anything else asking what a
/// running sandbox is: what pall8t knows and the container does not say
/// about itself. Values are sanitised in [`container::run_argv`] (a `=` in
/// a project path would fail the run outright), so this only decides
/// *which* facts are recorded.
fn run_labels(cwd: &Path, image_tag: &str, main_git_dir: Option<&Path>) -> Vec<(String, String)> {
    let mut labels = vec![
        (
            container::LABEL_VERSION.to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        ("pall8t.project".to_string(), cwd.display().to_string()),
        ("pall8t.image".to_string(), image_tag.to_string()),
    ];
    if let Some(git_dir) = main_git_dir {
        labels.push((
            "pall8t.worktree.git_dir".to_string(),
            git_dir.display().to_string(),
        ));
    }
    labels
}

/// The herdr half of the provenance, for a run that has a pane around it.
/// Separate from [`run_labels`] because it is conditional on the herdr
/// environment and independent of whether the *bridge* then succeeds — a
/// bridge failure is best-effort and must not cost the run its labels.
fn herdr_labels(env: &herdr::HerdrEnv, sandbox: config::HerdrSandbox) -> Vec<(String, String)> {
    let mut labels = vec![("pall8t.herdr.pane".to_string(), env.pane_id.clone())];
    if let Some(w) = &env.workspace_id {
        labels.push(("pall8t.herdr.workspace".to_string(), w.clone()));
    }
    if let Some(t) = &env.tab_id {
        labels.push(("pall8t.herdr.tab".to_string(), t.clone()));
    }
    labels.push((
        "pall8t.herdr.sandbox".to_string(),
        sandbox.as_str().to_string(),
    ));
    labels
}

/// Everything about a launch that is decidable from the config and this
/// filesystem: the implicit mounts, a worktree's git directory, the
/// configured `[[mounts]]`, and the container home.
///
/// A named phase because #90 made it one — it runs *before* the runtime
/// starts and the image builds, so every way it can fail (a target that
/// is not absolute, one that walks upward or overlaps a protected path, a
/// source that does not exist or is not a directory, a comma in either)
/// costs a message rather than a container build.
struct MountPlan {
    mounts: Vec<container::Mount>,
    /// Probed here, and needed again for the provenance label.
    main_git_dir: Option<PathBuf>,
}

fn plan_mounts(cwd: &Path, cfg: &config::Config, readonly: Option<bool>) -> Result<MountPlan> {
    let mut mounts = vec![container::Mount::identity(cwd.to_path_buf())?];
    // One probe, two consumers: the mount below and the provenance label.
    let main_git_dir = worktree::main_git_dir(cwd);
    if let Some(git_dir) = main_git_dir.clone() {
        eprintln!(
            "pall8t: git worktree detected — also mounting {}",
            git_dir.display()
        );
        mounts.push(container::Mount::identity(git_dir)?);
    }
    let home_dest = PathBuf::from("/home/dev");
    // Container-side paths this run is built on, which no configured mount
    // may cover: the workspace, a worktree's git directory, and the home —
    // hiding the home would take out the agent's own config and session
    // history. Compared against mount *targets*, since that is where a
    // mount actually lands.
    let mut protected: Vec<_> = mounts.iter().map(|m| m.dest.clone()).collect();
    protected.push(home_dest.clone());
    if let Some(msg) = mounts::no_mounts_warning(readonly, cfg.mounts.len()) {
        eprintln!("{msg}");
    }
    let configured = mounts::resolve(&cfg.mounts, &protected, readonly)?;
    if let Some(msg) = mounts::outside_project_notice(&configured, cwd, cfg.mounts_from_project) {
        eprintln!("pall8t: warning: {msg}");
    }
    for m in configured {
        eprintln!("pall8t: {}", mounts::describe(&m));
        mounts.push(m);
    }
    mounts.push(container::Mount::rw(container::home_mount()?, home_dest)?);

    Ok(MountPlan {
        mounts,
        main_git_dir,
    })
}

fn cmd_run(cli_command: Vec<String>, readonly: Option<bool>, cli_ssh: Option<bool>) -> Result<()> {
    let (cwd, cfg) = workspace_config()?;
    let run_name = container::run_name(&cwd);

    let plan = plan_mounts(&cwd, &cfg, readonly)?;
    let mut mounts = plan.mounts;
    let main_git_dir = plan.main_git_dir;

    // Everything above is decidable from the config and this filesystem,
    // and every way it can fail is the user's to fix before anything is
    // built: a target that is not absolute, one that walks upward or
    // overlaps a protected path, a source that does not exist or is not a
    // directory, a comma in either. Only now does the runtime start and
    // the image build, so a typo in `[[mounts]]` costs a message instead
    // of a container build (issue #90). This extends the reason config
    // load already precedes the container check — a problem the user can
    // fix should not require a working `container` first — rather than
    // contradicting it.
    let (uid, gid, resolved) = build_image(&cwd, &cfg, image::BuildMode::IfMissing)?;
    // From here to the exec below, this run has an image and no container:
    // tab naming and the herdr bridge still come, and a first bridged run
    // downloads a herdr release in between. Nothing in `container list`
    // speaks for the image through that window — the reservation does
    // (issue #88). Mount planning used to sit in this window too; it now
    // runs before the build (issue #90), which shortens the exposure
    // without closing it. Best-effort: a hint we could not write is a
    // warning, not a failed run.
    if let Err(e) = image::reserve_tag(&resolved.tag) {
        eprintln!(
            "pall8t: warning: could not reserve {} ({e:#})",
            resolved.tag
        );
    }
    // A mount's own directory inode arrives inside the container owned by
    // root rather than the host user — the workspace included, not just
    // read-only reference mounts — so git refuses `status`/`log` there
    // until each mounted path is marked safe (see
    // `mounts::safe_directory_env`). Computed here, before the herdr
    // bridge appends its own mounts: those are a binary directory and a
    // socket, never a repository.
    let mount_targets: Vec<_> = mounts.iter().map(|m| m.dest.clone()).collect();

    let mut labels = run_labels(&cwd, &resolved.tag, main_git_dir.as_deref());

    let ssh = config::ssh_enabled(cfg.ssh, cli_ssh);
    // `var_os`, not `var`: SSH_AUTH_SOCK is a path, and a path is not
    // required to be UTF-8.
    let host_auth_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    // Connecting is the probe, not `Path::exists`: an agent that died
    // without cleaning up leaves a socket node that exists and refuses
    // every connection, which is the stale case the warning is for. The
    // connection is dropped immediately — reaching accept() is the whole
    // question, so nothing is written and no agent-protocol request is
    // made.
    let agent_reachable = |sock: &Path| UnixStream::connect(sock).is_ok();
    if let Some(msg) = config::ssh_warning(ssh, host_auth_sock.as_deref(), agent_reachable) {
        eprintln!("{msg}");
    } else if ssh {
        // Say so on the *working* path too, not only when it is broken.
        // A capability this wide that announces itself only on failure is
        // one a run can carry without anyone noticing — and the herdr
        // bridge, which is narrower, already says "herdr bridge active".
        eprintln!(
            "pall8t: [container] ssh is on — this run can use your SSH agent \
             to authenticate as you anywhere your keys are trusted"
        );
    }

    let herdr_env = herdr::detect();
    // An explicit `-- <cmd>` override is user intent and bypasses the
    // configured command entirely.
    let mut command = if cli_command.is_empty() {
        cfg.command.clone()
    } else {
        cli_command
    };
    let herdr_agent = herdr_env
        .as_ref()
        .and_then(|env| herdr::announce_pane_identity(env, &command));
    // Naming the tab and the agent is independent of the bridge below —
    // it is about herdr's view of the pane — so it happens in every
    // `[herdr] sandbox` mode, `off` included.
    if let Some(env) = &herdr_env {
        naming::name_pane(&naming::Request {
            herdr_bin: env.herdr_bin(),
            pane_id: &env.pane_id,
            tab_id: env.tab_id.as_deref(),
            socket_path: env.socket_path.as_deref(),
            workspace_dir: &cwd,
            cfg: &cfg.herdr,
            expect_agent: herdr_agent.is_some(),
        });
    }
    // The bridge (ADR-0007) makes the herdr CLI work inside the sandbox:
    // relay + env + Linux binary mount + bootstrap wrap. Best-effort — a
    // bridge failure warns and the run proceeds without it.
    let mut env_vars = mounts::safe_directory_env(&mount_targets);
    if let Some(env) = &herdr_env {
        labels.extend(herdr_labels(env, cfg.herdr.sandbox));
        match herdr::prepare_bridge(env, cfg.herdr.sandbox, &run_name) {
            Ok(Some(bridge)) => {
                eprintln!(
                    "pall8t: herdr bridge active ({}) — the sandboxed agent can reach \
                     this herdr session; audit log in ~/.pall8t/logs/",
                    cfg.herdr.sandbox.as_str()
                );
                mounts.extend(bridge.mounts);
                env_vars.extend(bridge.env);
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
        ssh,
        labels,
        hardening: cfg.hardening,
        command,
    };
    exec_container(&container::run_argv(&spec), herdr_agent.as_deref())
}

fn cmd_build(no_cache: bool) -> Result<()> {
    let mode = if no_cache {
        image::BuildMode::ForceNoCache
    } else {
        image::BuildMode::Force
    };
    let (cwd, cfg) = workspace_config()?;
    let (_, _, resolved) = build_image(&cwd, &cfg, mode)?;
    println!("built {}", resolved.tag);
    Ok(())
}

fn cmd_ls(json: bool) -> Result<()> {
    ensure_container_system()?;
    let containers = container::list_pall8t()?;
    if json {
        // Additive: `name`/`status` are the shape herdr and scripts already
        // read, and `image`/`labels` join them rather than replacing
        // anything. A container from an older pall8t simply has no labels.
        let items: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "status": c.state.as_str(),
                    "image": c.image,
                    "labels": c.labels,
                })
            })
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
            listen,
            mode,
            log,
        } => pall8t::relay::run(socket, listen, pall8t::relay::Mode::parse(mode)?, log),
        HerdrCmd::NameAgent {
            pane,
            name,
            tab,
            herdr_bin,
            log,
        } => naming::run_agent_namer(herdr_bin, pane, name, tab.as_deref(), log),
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
                warn_config_issues(cfg);
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
