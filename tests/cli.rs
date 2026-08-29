//! End-to-end tests of the `pall8t` binary itself.
//!
//! `docs/testing.md` rules out tests that need a live `container` runtime
//! or herdr session, and none of these have one: every case here is either
//! pure argument handling, work confined to a throwaway `$HOME`, or the
//! *absence* of the `container` CLI, which the sandbox guarantees by
//! handing the child an empty `PATH`. What they buy over the in-crate unit
//! tests is the wiring — `main`'s exit codes, clap's shape, which stream
//! each message goes to, and the fact that `~/.pall8t` is derived from the
//! environment rather than hardcoded.
//!
//! Isolation rests on one verified fact: `dirs::home_dir()` reads `$HOME`
//! first and only falls back to `getpwuid` when it is unset or empty
//! (dirs-sys 0.4.1). Every child therefore gets an explicit `HOME` — never
//! `env_clear()` alone, which would send `~/.pall8t` back to the real home
//! directory and let a test write into it.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// One test's throwaway world: an isolated `$HOME` (so `~/.pall8t` is a
/// temp tree) and a project directory to run in.
///
/// Kept under `/tmp` rather than `std::env::temp_dir()` for the same
/// reason `relay.rs`'s tests are: the relay binds Unix sockets under
/// `$HOME/.pall8t/run`, and macOS' per-user temp directory is long enough
/// to blow the 104-byte `sun_path` budget once a socket name is appended.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = PathBuf::from("/tmp").join(format!("p8t-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["home", "project", "bin"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Sandbox { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// `~/.pall8t` as this sandbox's children see it.
    fn pall8t_root(&self) -> PathBuf {
        self.home().join(".pall8t")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pall8t"));
        self.apply_env(&mut cmd);
        cmd
    }

    /// A `/bin/sh` in the same throwaway world, for the one test that needs
    /// a child of something other than itself.
    fn sh_command(&self) -> Command {
        let mut cmd = Command::new("/bin/sh");
        self.apply_env(&mut cmd);
        cmd
    }

    fn apply_env(&self, cmd: &mut Command) {
        cmd.env_clear();
        // `env_clear` is what makes these tests independent of the
        // developer's shell — including `HERDR_*`, which would otherwise
        // make "no pane here" untrue when the suite runs inside a herdr
        // pane. It also drops the coverage instrumentation's output path,
        // and a child that cannot write a profile silently contributes
        // nothing to the report, so that one variable is forwarded back.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            cmd.env("LLVM_PROFILE_FILE", profile);
        }
        // Never omit HOME: with it unset, dirs falls back to the real
        // passwd entry and the test writes into the user's home. PATH is an
        // empty directory, so `container` (and `git`, and `herdr`) are
        // definitively not found — that is what makes the runtime-missing
        // arms deterministic instead of dependent on whether the developer
        // has apple/container installed.
        cmd.env("HOME", self.home())
            .env("PATH", self.root.join("bin"))
            .current_dir(self.project());
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// Like [`Sandbox::run`], but never waits forever.
    ///
    /// For the subcommand that is *supposed* to refuse and exit: `herdr
    /// relay` serves until its parent goes away, so if the refusal it is
    /// being tested for ever stopped happening, a plain `output()` would
    /// hang the suite instead of failing it. Killing at the deadline turns
    /// that into a normal assertion failure.
    fn run_bounded(&self, args: &[&str], limit: std::time::Duration) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + limit;
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }

    /// Writes a project `.pall8t/config.toml`.
    fn write_project_config(&self, body: &str) {
        let dir = self.project().join(".pall8t");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    /// Writes the global `~/.pall8t/config.toml`.
    fn write_global_config(&self, body: &str) {
        std::fs::create_dir_all(self.pall8t_root()).unwrap();
        std::fs::write(self.pall8t_root().join("config.toml"), body).unwrap();
    }
}

/// A stand-in `container` CLI on the sandbox's `PATH`.
///
/// This is *not* the "live runtime" `docs/testing.md` rules out: nothing
/// here virtualizes apple/container's behaviour. It replays literal
/// captured output for the three read-only queries pall8t parses
/// (`--version`, `list --all --format json`, `image list --format json`)
/// and records the argv of every call, so the tests can assert the one
/// thing that is genuinely pall8t's contract with the runtime — the
/// command lines it constructs — without a VM, a build, or a machine that
/// happens to have apple/container installed.
struct FakeRuntime<'a> {
    sandbox: &'a Sandbox,
}

impl<'a> FakeRuntime<'a> {
    /// `version` is the banner `container --version` replies with;
    /// `containers` and `images` are the JSON bodies for the two listings.
    fn install(sandbox: &'a Sandbox, version: &str, containers: &str, images: &str) -> Self {
        std::fs::write(sandbox.root.join("containers.json"), containers).unwrap();
        std::fs::write(sandbox.root.join("images.json"), images).unwrap();
        let root = sandbox.root.display();
        let script = format!(
            r#"#!/bin/sh
# Stand-in apple/container CLI for pall8t's integration tests.
# Its own PATH, because the one it inherits is the deliberately empty
# directory that hides the real runtime — without this, `cat` below is not
# found and every listing silently comes back empty.
PATH=/bin:/usr/bin
printf '%s\n' "$*" >> "{root}/argv.log"
case "$1" in
  --version) echo "{version}" ;;
  system)   [ "$2" = "status" ] || [ "$2" = "start" ] || exit 1 ;;
  list)     cat "{root}/containers.json" ;;
  image)
    case "$2" in
      # `image list` is the last thing pall8t asks the runtime before it
      # execs into it. A test that wants to observe the launch path removes
      # the runtime here (see `vanish_after_image_list`).
      list)   cat "{root}/images.json"; [ -f "{root}/vanish" ] && rm -f "{root}/bin/container" ;;
      delete) ;;
      *) exit 1 ;;
    esac ;;
  build)   echo "fake build ok" >&2 ;;
  stop)    ;;
  run|exec) echo "fake $1 reached" ;;
  *) exit 1 ;;
esac
exit 0
"#
        );
        let bin = sandbox.root.join("bin").join("container");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakeRuntime { sandbox }
    }

    /// The default install: a current runtime, no containers, no images.
    fn current(sandbox: &'a Sandbox) -> Self {
        Self::install(
            sandbox,
            "container CLI version 1.2.2 (build: release, commit: unspeci)",
            "[]",
            "[]",
        )
    }

    fn set_containers(&self, json: &str) {
        std::fs::write(self.sandbox.root.join("containers.json"), json).unwrap();
    }

    fn set_images(&self, refs: &[String]) {
        let json = serde_json::to_string(refs).unwrap();
        std::fs::write(self.sandbox.root.join("images.json"), json).unwrap();
    }

    /// Makes the runtime disappear right after the image check, which is
    /// the last thing `pall8t run` asks it before `execve`. Handing the
    /// launch path a runtime it cannot exec turns a process replacement
    /// into an ordinary error return — the only way a test can watch what
    /// `run` did on its way there, since an `execve` leaves nothing behind
    /// to inspect. It also pins real behaviour: a runtime that vanishes
    /// mid-launch must fail loudly, not exit 0 having started nothing.
    fn vanish_after_image_list(&self) {
        std::fs::write(self.sandbox.root.join("vanish"), b"").unwrap();
    }

    fn clear_log(&self) {
        let _ = std::fs::remove_file(self.sandbox.root.join("argv.log"));
    }

    /// Every command line pall8t handed the runtime, one per line.
    fn argv_log(&self) -> String {
        std::fs::read_to_string(self.sandbox.root.join("argv.log")).unwrap_or_default()
    }

    fn called(&self, needle: &str) -> bool {
        self.argv_log().lines().any(|l| l.contains(needle))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn version_reports_the_crate_version() {
    let sb = Sandbox::new("version");
    let out = sb.run(&["--version"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains(env!("CARGO_PKG_VERSION")),
        "`--version` must print the version cargo built, so a release \
         binary can be identified in the field: {}",
        stdout(&out)
    );
}

#[test]
fn help_lists_every_subcommand() {
    let sb = Sandbox::new("help");
    let out = sb.run(&["--help"]);
    assert!(out.status.success());
    let text = stdout(&out);
    for cmd in ["init", "run", "build", "ls", "exec", "stop", "herdr"] {
        assert!(
            text.contains(cmd),
            "`{cmd}` is a supported subcommand and must appear in --help: {text}"
        );
    }
}

#[test]
fn the_internal_subcommands_stay_hidden_from_help() {
    let sb = Sandbox::new("hidden");
    let out = sb.run(&["herdr", "--help"]);
    assert!(out.status.success());
    assert!(
        !stdout(&out).contains("relay"),
        "`herdr relay` is spawned by `pall8t run`, never typed — listing it \
         invites hand-running the one subcommand that chmods and sweeps a \
         directory: {}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("name-agent"),
        "and `herdr name-agent` is likewise pall8t's own machinery: it \
         outlives the run that spawned it and reports to a log, neither of \
         which makes sense typed by hand: {}",
        stdout(&out)
    );
}

#[test]
fn an_unknown_subcommand_fails_rather_than_doing_something_else() {
    let sb = Sandbox::new("unknown");
    let out = sb.run(&["frobnicate"]);
    assert!(
        !out.status.success(),
        "an unrecognized subcommand must not exit 0 — a wrapper script \
         checking the exit code has to be able to tell"
    );
}

#[test]
fn init_creates_the_skeletons_then_reports_them_as_existing() {
    let sb = Sandbox::new("init");

    let first = sb.run(&["init"]);
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    let home = sb.pall8t_root().join("home");
    let global = sb.pall8t_root().join("config.toml");
    let containerfile = sb.pall8t_root().join("Containerfile");
    let project = sb.project().join(".pall8t").join("config.toml");
    for path in [&home, &global, &containerfile, &project] {
        assert!(
            path.exists(),
            "init must materialize {} — that is the whole command",
            path.display()
        );
    }
    assert!(
        stdout(&first).contains("created:"),
        "the first run reports what it made: {}",
        stdout(&first)
    );
    assert!(
        home.starts_with(sb.home()),
        "everything init writes must sit under $HOME, not a hardcoded path: {}",
        home.display()
    );

    let second = sb.run(&["init"]);
    assert!(second.status.success());
    assert!(
        stdout(&second).contains("exists, skipped:") && !stdout(&second).contains("created:"),
        "init is idempotent: a second run must report every file as already \
         there rather than rewriting it: {}",
        stdout(&second)
    );
}

#[test]
fn init_never_overwrites_an_edited_file() {
    let sb = Sandbox::new("init-edit");
    sb.run(&["init"]);
    let containerfile = sb.pall8t_root().join("Containerfile");
    std::fs::write(&containerfile, "FROM scratch\n# mine\n").unwrap();

    let out = sb.run(&["init"]);

    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(&containerfile).unwrap(),
        "FROM scratch\n# mine\n",
        "the Containerfile is the user's to edit — re-running init must \
         never restore the shipped default over it"
    );
}

#[test]
fn every_command_that_needs_the_runtime_says_where_to_get_it() {
    let sb = Sandbox::new("no-cli");
    // `run` and `build` are included deliberately: they must fail on the
    // missing runtime *before* building an image, so this stays a
    // read-only test.
    for args in [
        vec!["ls"],
        vec!["build"],
        vec!["run"],
        vec!["stop", "pall8t-x"],
        vec!["exec", "pall8t-x", "--", "true"],
    ] {
        let out = sb.run(&args);
        assert!(
            !out.status.success(),
            "`pall8t {}` cannot work without the container CLI and must exit \
             non-zero",
            args.join(" ")
        );
        assert!(
            stderr(&out).contains("github.com/apple/container"),
            "the error has to name where to get the missing runtime, not just \
             that something failed (`pall8t {}`): {}",
            args.join(" "),
            stderr(&out)
        );
    }
}

#[test]
fn exec_without_a_command_says_how_to_pass_one() {
    let sb = Sandbox::new("exec-usage");
    let out = sb.run(&["exec", "pall8t-x"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("pall8t exec <id> -- <cmd>"),
        "the usage line is the fix, and it must arrive before the container \
         CLI is even consulted: {}",
        stderr(&out)
    );
}

#[test]
fn herdr_doctor_json_reports_no_pane_outside_herdr() {
    let sb = Sandbox::new("doctor-json");
    let out = sb.run(&["herdr", "doctor", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let checks: serde_json::Value = serde_json::from_str(stdout(&out).trim())
        .unwrap_or_else(|e| panic!("--json must emit parseable JSON ({e}): {}", stdout(&out)));
    let checks = checks.as_array().expect("doctor reports a list of checks");
    assert!(
        !checks.is_empty(),
        "doctor with no herdr around still has something to report — that \
         there is no pane is the report"
    );
    for c in checks {
        assert!(
            c.get("name").is_some() && c.get("ok").is_some() && c.get("detail").is_some(),
            "herdr consumes this shape; every check needs name/ok/detail: {c}"
        );
    }
    assert!(
        checks.iter().any(|c| c["ok"] == false),
        "outside a herdr pane at least one check must fail — a doctor that \
         says everything is fine here would be lying"
    );
}

#[test]
fn herdr_doctor_prints_a_mark_per_check_without_json() {
    let sb = Sandbox::new("doctor-text");
    let out = sb.run(&["herdr", "doctor"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains('✓') || text.contains('✗'),
        "the human-readable arm marks each check pass/fail: {text}"
    );
}

#[test]
fn herdr_doctor_sees_the_socket_when_the_pane_env_points_at_a_live_one() {
    let sb = Sandbox::new("doctor-sock");
    // A real listener, so the connect-only probe has something to answer
    // for. No herdr involved — doctor never sends a request.
    let sock = sb.root.join("herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

    let out = sb
        .command()
        .args(["herdr", "doctor", "--json"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "pane_test")
        .env("HERDR_SOCKET_PATH", &sock)
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let checks: serde_json::Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    let socket_check = checks
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str().is_some_and(|n| n.contains("socket")))
        .expect("a socket check exists");
    assert_eq!(
        socket_check["ok"], true,
        "something is listening on the path the pane env names, so the probe \
         must say so: {socket_check}"
    );
    drop(listener);
}

#[test]
fn a_deprecated_config_section_is_warned_about_on_stderr_only() {
    let sb = Sandbox::new("deprecated");
    sb.write_project_config("[home]\nmode = \"isolated\"\n");

    let out = sb.run(&["herdr", "doctor", "--json"]);

    assert!(out.status.success());
    assert!(
        stderr(&out).contains("[home]") && stderr(&out).contains("no longer supported"),
        "an ignored setting must be said out loud, or the user keeps \
         believing it works: {}",
        stderr(&out)
    );
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim()).expect(
        "the warning belongs on stderr: stdout stays parseable JSON for the \
         tools that consume --json",
    );
}

#[test]
fn an_invalid_config_names_the_file_it_could_not_read() {
    let sb = Sandbox::new("bad-toml");
    sb.write_project_config("cpus = \n");

    let out = sb.run(&["run"]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(".pall8t/config.toml"),
        "with two config files in play, the error must say which one is \
         broken: {}",
        stderr(&out)
    );
}

#[test]
fn a_config_still_using_repos_is_an_error_naming_its_replacement() {
    let sb = Sandbox::new("repos");
    sb.write_project_config("[[repos]]\nsource = \"~/src/lib\"\n");

    let out = sb.run(&["run"]);

    assert!(
        !out.status.success(),
        "[[repos]] silently ignored would leave a directory the user believes \
         is mounted missing from the sandbox"
    );
    assert!(
        stderr(&out).contains("[[mounts]]"),
        "the error has to name the replacement, not just the removal: {}",
        stderr(&out)
    );
}

#[test]
fn a_config_problem_is_reported_even_without_the_container_runtime() {
    let sb = Sandbox::new("cfg-before-cli");
    sb.write_global_config("[[repos]]\nsource = \"~/src/lib\"\n");

    let out = sb.run(&["build"]);

    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("[[mounts]]") && !stderr(&out).contains("github.com/apple/container"),
        "config is read before the runtime check on purpose: someone fixing \
         a broken config shouldn't have to install apple/container first to \
         be told what is wrong with it: {}",
        stderr(&out)
    );
}

#[test]
fn the_relay_refuses_a_listen_path_outside_its_own_run_directory() {
    let sb = Sandbox::new("relay-guard");
    let stray = sb.root.join("stray.sock");

    let out = sb.run_bounded(
        &[
            "herdr",
            "relay",
            "--socket",
            "/nonexistent/herdr.sock",
            "--listen",
            stray.to_str().unwrap(),
            "--mode",
            "full",
            "--log",
            sb.root.join("relay.log").to_str().unwrap(),
        ],
        std::time::Duration::from_secs(10),
    );

    assert!(
        !out.status.success(),
        "the relay chmods its directory to 0700 and unlinks sockets in it — \
         pointed at a directory it does not own, it must refuse before doing \
         any of that"
    );
    assert!(
        stderr(&out).contains(".pall8t/run"),
        "the refusal has to say where the socket belongs: {}",
        stderr(&out)
    );
    assert!(
        !stray.exists(),
        "nothing may be bound outside the run directory"
    );
}

#[test]
fn the_relay_rejects_a_policy_mode_it_does_not_know() {
    let sb = Sandbox::new("relay-mode");
    let out = sb.run(&[
        "herdr",
        "relay",
        "--socket",
        "/nonexistent/herdr.sock",
        "--listen",
        sb.pall8t_root()
            .join("run")
            .join("x.sock")
            .to_str()
            .unwrap(),
        "--mode",
        "sometimes",
        "--log",
        sb.root.join("relay.log").to_str().unwrap(),
    ]);

    assert!(
        !out.status.success(),
        "an unknown mode must fail loudly: falling back to a default would \
         pick a policy the caller did not ask for"
    );
    assert!(
        stderr(&out).contains("sometimes"),
        "the error names the mode it could not parse: {}",
        stderr(&out)
    );
}

/// The relay end to end as `pall8t run` uses it: spawn it, read the socket
/// path off its stdout, connect to *that* path, and check policy is
/// applied and audited. A fake herdr on the upstream side stands in for
/// the host session; no real herdr is involved.
#[test]
fn the_relay_serves_and_polices_the_socket_it_announces() {
    let sb = Sandbox::new("relay-serve");
    let upstream_path = sb.root.join("h.sock");
    let upstream = std::os::unix::net::UnixListener::bind(&upstream_path).unwrap();
    std::thread::spawn(move || {
        for conn in upstream.incoming() {
            let Ok(mut conn) = conn else { continue };
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut r = BufReader::new(conn.try_clone().unwrap());
                if r.read_line(&mut line).is_ok() {
                    let _ = conn.write_all(b"{\"id\":\"r1\",\"result\":{\"ok\":true}}\n");
                }
            });
        }
    });

    let log = sb.root.join("relay.log");
    let listen = sb.pall8t_root().join("run").join("pall8t-itest.sock");
    let mut child = sb
        .command()
        .args([
            "herdr",
            "relay",
            "--socket",
            upstream_path.to_str().unwrap(),
            "--listen",
            listen.to_str().unwrap(),
            "--mode",
            "readonly",
            "--log",
            log.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut announced = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut announced)
        .unwrap();
    assert_eq!(
        announced.trim(),
        listen.to_str().unwrap(),
        "the readiness line is a contract: `pall8t run` mounts exactly the \
         path the relay prints, and cannot proceed until the socket exists"
    );
    assert!(
        listen.exists(),
        "the path is announced only once it is bound — announcing early \
         would hand `container run` a mount source that isn't there yet"
    );

    let allowed = roundtrip(&listen, r#"{"id":"r1","method":"pane.list","params":{}}"#);
    assert_eq!(
        allowed["result"]["ok"], true,
        "an inspection method is allowed in readonly and reaches the herdr \
         socket: {allowed}"
    );

    let denied = roundtrip(&listen, r#"{"id":"r2","method":"pane.split","params":{}}"#);
    assert_eq!(
        denied["error"]["code"], "sandbox_denied",
        "readonly mode must stop a mutation at the relay, and answer in \
         herdr's own error shape so the in-container CLI renders it: {denied}"
    );

    // A request line with a real payload on it: `agent.prompt` bodies and
    // graphics blobs run to kilobytes, and the relay's cap is herdr's own
    // (1 MB). A smaller cap would truncate this line mid-JSON, leaving
    // policy unable to classify it — and readonly denies what it cannot
    // classify, so a shrunken cap turns working reads into refusals.
    let padding = "x".repeat(8192);
    let big = format!(r#"{{"id":"r3","method":"pane.list","params":{{"pad":"{padding}"}}}}"#);
    let big_reply = roundtrip(&listen, &big);
    assert_eq!(
        big_reply["result"]["ok"], true,
        "an 8 KB read request is well under herdr's 1 MB line cap and must \
         cross intact: {big_reply}"
    );

    let audit = std::fs::read_to_string(&log).unwrap();
    assert!(
        audit.contains("\"allow\"") && audit.contains("\"deny\""),
        "every decision is audited on the host — that log is what makes \
         `full` mode a deliberate, reviewable opening: {audit}"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for line in audit.lines() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = entry["ts"].as_u64().expect("every entry carries a ts");
        assert!(
            ts.abs_diff(now) < 300,
            "an audit trail is only reviewable if it says *when*: {line}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn roundtrip(listen: &Path, request: &str) -> serde_json::Value {
    let mut conn = std::os::unix::net::UnixStream::connect(listen).unwrap();
    conn.write_all(request.as_bytes()).unwrap();
    conn.write_all(b"\n").unwrap();
    let mut body = String::new();
    conn.read_to_string(&mut body).unwrap();
    serde_json::from_str(body.trim().lines().next().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Against a stand-in runtime: the command lines pall8t builds, and the
// paths that only run once `container` answers.
// ---------------------------------------------------------------------------

/// Literal shape of `container list --all --format json` (apple/container
/// 1.0.0): `status` is a nested object. Two pall8t containers and one
/// container pall8t did not start.
const CONTAINERS_JSON: &str = r#"[
  { "id": "pall8t-x-1",
    "configuration": { "id": "pall8t-x-1", "image": { "reference": "pall8t-x:501-20-aaa" } },
    "status": { "state": "running", "networks": [], "startedDate": "2026-07-11T02:33:10Z" } },
  { "id": "pall8t-x-2",
    "configuration": { "id": "pall8t-x-2", "image": { "reference": "pall8t-x:501-20-bbb" } },
    "status": { "state": "stopped", "networks": [] } },
  { "id": "someone-elses",
    "configuration": { "id": "someone-elses", "image": { "reference": "ubuntu:24.04" } },
    "status": { "state": "running", "networks": [] } }
]"#;

#[test]
fn ls_reports_only_the_containers_pall8t_started() {
    let sb = Sandbox::new("ls");
    FakeRuntime::install(
        &sb,
        "container CLI version 1.2.2 (build: release, commit: unspeci)",
        CONTAINERS_JSON,
        "[]",
    );

    let out = sb.run(&["ls"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("pall8t-x-1\trunning") && text.contains("pall8t-x-2\tstopped"),
        "each pall8t container is listed with its real state — reading the \
         nested `status.state`, not a top-level string that never exists: {text}"
    );
    assert!(
        !text.contains("someone-elses"),
        "`ls` is scoped to containers pall8t started; listing the user's own \
         containers would invite stopping one: {text}"
    );
}

#[test]
fn ls_json_is_machine_readable_and_alone_on_stdout() {
    let sb = Sandbox::new("ls-json");
    FakeRuntime::install(
        &sb,
        "container CLI version 1.2.2 (build: release, commit: unspeci)",
        CONTAINERS_JSON,
        "[]",
    );

    let out = sb.run(&["ls", "--json"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let items: serde_json::Value = serde_json::from_str(stdout(&out).trim())
        .unwrap_or_else(|e| panic!("herdr consumes this stream ({e}): {}", stdout(&out)));
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 2, "one object per pall8t container: {items:?}");
    assert_eq!(items[0]["name"], "pall8t-x-1");
    assert_eq!(
        items[0]["status"], "running",
        "the state is spelled the way the JSON contract says, not Debug-formatted"
    );
}

#[test]
fn an_outdated_runtime_is_warned_about_without_dirtying_json_output() {
    let sb = Sandbox::new("old-runtime");
    FakeRuntime::install(
        &sb,
        "container CLI version 1.0.0 (build: release, commit: abc1234)",
        "[]",
        "[]",
    );

    let out = sb.run(&["ls", "--json"]);

    assert!(out.status.success());
    assert!(
        stderr(&out).contains("apple/container 1.0.0 is older than"),
        "a runtime that leaks host env into the sandbox weakens the boundary \
         pall8t documents, and the user has to hear about it: {}",
        stderr(&out)
    );
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim())
        .expect("the warning goes to stderr so `ls --json` stays parseable");
}

#[test]
fn a_current_runtime_draws_no_version_warning() {
    let sb = Sandbox::new("new-runtime");
    FakeRuntime::current(&sb);

    let out = sb.run(&["ls"]);

    assert!(out.status.success());
    assert!(
        !stderr(&out).contains("older than"),
        "a warning users learn to scroll past stops working on the day it is \
         right — it must stay silent on a supported runtime: {}",
        stderr(&out)
    );
}

#[test]
fn stop_names_the_container_it_stopped() {
    let sb = Sandbox::new("stop");
    let fake = FakeRuntime::current(&sb);

    let out = sb.run(&["stop", "pall8t-x-1"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        fake.called("stop pall8t-x-1"),
        "the id is forwarded verbatim: {}",
        fake.argv_log()
    );
    assert!(stdout(&out).contains("stopped pall8t-x-1"));
}

#[test]
fn build_tags_the_image_from_the_containerfile_and_reports_the_tag() {
    let sb = Sandbox::new("build");
    let fake = FakeRuntime::current(&sb);
    sb.run(&["init"]);

    let out = sb.run(&["build"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let built = stdout(&out);
    assert!(
        built.starts_with("built "),
        "the tag it produced is the command's answer, and a wrapper reads it \
         off stdout: {built}"
    );
    let tag = built.trim().strip_prefix("built ").unwrap();
    assert!(
        fake.called(&format!("-t {tag}")),
        "the tag reported is the tag actually built: {}",
        fake.argv_log()
    );
    assert!(
        fake.called("--build-arg UID=") && fake.called("--build-arg GID="),
        "the image is built for the invoking user, so files it writes in the \
         mounted workspace belong to that user on the host: {}",
        fake.argv_log()
    );
    assert!(
        !fake.called("--no-cache"),
        "a plain `build` reuses the layer cache; --no-cache is the opt-in: {}",
        fake.argv_log()
    );
}

#[test]
fn build_no_cache_forwards_the_flag() {
    let sb = Sandbox::new("build-nocache");
    let fake = FakeRuntime::current(&sb);
    sb.run(&["init"]);

    let out = sb.run(&["build", "--no-cache"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        fake.called("--no-cache"),
        "`--no-cache` exists to re-run the RUN steps that fetch \"latest\"; \
         dropping it silently would serve a stale agent CLI forever: {}",
        fake.argv_log()
    );
}

/// `pall8t build` once, to learn the tag this machine's uid/gid and the
/// shipped default Containerfile produce. Hardcoding `501-20` would make
/// the pruning tests pass or fail on whose laptop they run.
fn build_once(sb: &Sandbox, fake: &FakeRuntime) -> String {
    sb.run(&["init"]);
    let out = sb.run(&["build"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    fake.clear_log();
    stdout(&out)
        .trim()
        .strip_prefix("built ")
        .expect("build reports the tag it produced")
        .to_string()
}

/// `pall8t-base:501-20` out of `pall8t-base:501-20-<hash>` — the sibling
/// tags a prune is scoped to.
fn tag_prefix(tag: &str) -> String {
    tag.rsplit_once('-')
        .expect("a tag is <base>:<uid>-<gid>-<hash>")
        .0
        .to_string()
}

#[test]
fn build_prunes_a_superseded_image_but_keeps_one_a_container_still_runs() {
    let sb = Sandbox::new("build-prune");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    let prefix = tag_prefix(&tag);
    let in_use = format!("{prefix}-inuse");
    let superseded = format!("{prefix}-old");

    fake.set_containers(&format!(
        r#"[
          {{ "id": "pall8t-x-1",
            "configuration": {{ "id": "pall8t-x-1", "image": {{ "reference": "{in_use}" }} }},
            "status": {{ "state": "running", "networks": [] }} }}
        ]"#
    ));
    fake.set_images(&[
        tag.clone(),
        superseded.clone(),
        in_use.clone(),
        "ubuntu:24.04".to_string(),
    ]);

    let out = sb.run(&["build"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        fake.called(&format!("image delete {superseded}")),
        "a superseded build under the same base, for this uid/gid, and unused \
         — that is exactly what pruning is for: {}",
        fake.argv_log()
    );
    assert!(
        !fake.called(&format!("image delete {in_use}")),
        "an existing container still runs that image; deleting it out from \
         under a live container is the failure this check exists to prevent: {}",
        fake.argv_log()
    );
    assert!(
        !fake.called(&format!("image delete {tag}")),
        "the tag just built is the one thing a prune must never take: {}",
        fake.argv_log()
    );
    assert!(
        !fake.called("image delete ubuntu:24.04"),
        "pruning is scoped to images pall8t built for this user: {}",
        fake.argv_log()
    );
}

#[test]
fn build_skips_pruning_when_an_image_in_use_cannot_be_determined() {
    let sb = Sandbox::new("build-indeterminate");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    let superseded = format!("{}-old", tag_prefix(&tag));

    // A listing entry carrying no image reference: pall8t cannot tell what
    // that container runs, so every candidate becomes unsafe to delete.
    fake.set_containers(
        r#"[
          { "id": "pall8t-x-1", "configuration": { "id": "pall8t-x-1" },
            "status": { "state": "running", "networks": [] } }
        ]"#,
    );
    fake.set_images(&[tag.clone(), superseded.clone()]);

    let out = sb.run(&["build"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        !fake.called("image delete"),
        "indeterminate in-use refs must skip the prune entirely rather than \
         guess — {superseded} is prunable on every other criterion: {}",
        fake.argv_log()
    );
    assert!(
        stderr(&out).contains("skipping prune"),
        "and say so, or a user wonders why old images pile up: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------
// The launch path: what `pall8t run` hands the runtime, and the herdr
// bridge it builds on the way there.
// ---------------------------------------------------------------------------

/// Installs a stand-in host `herdr` CLI and returns its path. It answers
/// `--version` (which is how pall8t decides *which* Linux build the
/// sandbox needs), replays whatever JSON the test left for the two list
/// queries pall8t parses, can be told to withhold `agent get` for a few
/// probes, and accepts everything else, recording argv.
fn fake_host_herdr(sb: &Sandbox, version: &str) -> PathBuf {
    let path = sb.root.join("herdr");
    let root = sb.root.display();
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
PATH=/bin:/usr/bin
printf '%s\n' "$*" >> "{root}/herdr-argv.log"
case "$1 $2" in
  "--version "*) echo "herdr {version}" ;;
  "tab list") cat "{root}/herdr-tab-list.json" ;;
  "agent list") cat "{root}/herdr-agent-list.json" ;;
  "agent rename")
    # A name another run took since the pre-exec scan: the test names it
    # in rename-taken, and herdr's own `agent_name_taken` comes back.
    taken=$(cat "{root}/rename-taken" 2>/dev/null || echo "")
    if [ -n "$taken" ] && [ "$4" = "$taken" ]; then
      echo "{{\"id\":\"cli:agent:rename\",\"error\":{{\"code\":\"agent_name_taken\",\"message\":\"agent name $4 is already used; candidates: terminal_id=t1\"}}}}" >&2
      exit 1
    fi
    ;;
  "agent get")
    # herdr does not recognize the sandboxed agent the instant the run
    # starts. A test can say how many probes it takes by writing the count
    # to agent-get-ready; with no such file the first probe succeeds.
    n=$(cat "{root}/agent-get-count" 2>/dev/null || echo 0)
    n=$((n+1)); echo "$n" > "{root}/agent-get-count"
    need=$(cat "{root}/agent-get-ready" 2>/dev/null || echo 1)
    if [ "$n" -lt "$need" ]; then
      echo '{{"id":"cli:agent:get","error":{{"code":"agent_not_found","message":"agent target w13:p3 not found"}}}}' >&2
      exit 1
    fi
    ;;
esac
exit 0
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Waits for `needle` to show up in `path`, and returns the file's
/// contents either way. The agent half of herdr naming runs in a detached
/// child that outlives `pall8t run`, so its effects land *after* the
/// command returns; a deadline turns "the child was never spawned" into a
/// failed assertion instead of a hung suite.
fn wait_for_line(path: &Path, needle: &str, limit: std::time::Duration) -> String {
    wait_until(limit, || {
        std::fs::read_to_string(path).is_ok_and(|body| body.contains(needle))
    });
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Seeds `~/.pall8t/tools/herdr/<version>/herdr` and the sha256 sidecar
/// that records it, so the bridge finds a *verified* cached Linux binary
/// and never reaches for the network. The sidecar is the full sha256 of
/// the file, stored one directory up from the one that gets mounted.
fn seed_verified_linux_herdr(sb: &Sandbox, version: &str) {
    let root = sb.pall8t_root().join("tools").join("herdr");
    let dir = root.join(version);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("herdr");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(&bin)
        .output()
        .unwrap();
    let digest = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    std::fs::write(root.join(format!("{version}.sha256")), digest).unwrap();
}

#[test]
fn run_hands_the_runtime_the_workspace_mount_and_the_configured_command() {
    let sb = Sandbox::new("run-plain");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));
    fake.vanish_after_image_list();

    let out = sb.run(&["run"]);

    assert!(
        !out.status.success(),
        "the runtime disappeared before the launch; that must be an error, \
         not a silent success"
    );
    assert!(
        stderr(&out).contains("failed to exec `container`"),
        "and the error must say the launch itself failed, not something \
         earlier: {}",
        stderr(&out)
    );
    assert!(
        !fake.called("build "),
        "the image for this Containerfile already exists, so `run` must not \
         rebuild it: {}",
        fake.argv_log()
    );
}

/// The `run` command line pall8t handed the fake runtime.
fn run_line(fake: &FakeRuntime) -> String {
    fake.argv_log()
        .lines()
        .find(|l| l.starts_with("run "))
        .expect("pall8t must reach `container run`")
        .to_string()
}

/// `[container] ssh` has to survive the whole trip — config file, the
/// `--ssh` override, `ssh_enabled`, `RunSpec`, `run_argv` — and the only
/// place its effect is observable is the argv pall8t hands the runtime.
/// The unit tests cover each link; this one pins that they are actually
/// joined, so a wiring slip (`let ssh = false;` in `cmd_run`) has
/// somewhere to go red. The runtime is left in place rather than vanished:
/// the fake logs the `run` line and exits, which is the launch this needs
/// to read.
#[test]
fn ssh_forwarding_travels_from_config_to_the_runtimes_argv() {
    let sb = Sandbox::new("run-ssh-argv");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));

    let cases: [(&str, &[&str], bool, &str); 4] = [
        (
            "",
            &["run"],
            false,
            "forwarding is opt-in: a run that never asked for it must not be \
             handed the user's agent",
        ),
        (
            "",
            &["run", "--ssh"],
            true,
            "`pall8t run --ssh` is the one-run way in, with nothing in the \
             config file saying anything",
        ),
        (
            "[container]\nssh = true\n",
            &["run"],
            true,
            "and `[container] ssh = true` is the standing way in — the form \
             the README documents",
        ),
        (
            "[container]\nssh = true\n",
            &["run", "--ssh=false"],
            false,
            "`--ssh=false` beats a config that switched it on: one run \
             without handing over the agent, config left alone",
        ),
    ];

    for (config, args, expected, why) in cases {
        sb.write_project_config(config);
        fake.clear_log();
        sb.run(args);
        let line = run_line(&fake);
        assert_eq!(line.contains("--ssh"), expected, "{why}. argv was: {line}");
    }
}

/// The other half of the same wiring: when forwarding is on but the host
/// has no agent behind `SSH_AUTH_SOCK`, the run has to say so. The runtime
/// forwards nothing in that case and still sets `SSH_AUTH_SOCK` inside the
/// guest, so this warning is the only thing standing between the user and
/// an unexplained `ssh` connect failure.
#[test]
fn the_ssh_warning_says_which_way_the_hosts_agent_is_missing() {
    let sb = Sandbox::new("run-ssh-warn");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));

    // The harness clears the environment, so this run genuinely has no
    // SSH_AUTH_SOCK — no need to unset one.
    let unset = sb.run(&["run", "--ssh"]);
    assert!(
        stderr(&unset).contains("SSH_AUTH_SOCK is unset on the host"),
        "forwarding with no agent at all must say so on stderr: {}",
        stderr(&unset)
    );

    let off = sb.run(&["run"]);
    assert!(
        !stderr(&off).contains("SSH_AUTH_SOCK"),
        "and a run that never asked to forward has nothing to warn about: {}",
        stderr(&off)
    );

    // The case a presence-only check misses: a path still exported for a
    // socket that died with its agent.
    let dead = sb.root.join("dead-agent.sock");
    let stale = sb
        .command()
        .args(["run", "--ssh"])
        .env("SSH_AUTH_SOCK", &dead)
        .output()
        .unwrap();
    assert!(
        stderr(&stale).contains(&format!("points at {}", dead.display())),
        "a socket path with nothing behind it must be named, not treated as \
         a working agent: {}",
        stderr(&stale)
    );
}

/// The whole bridge, assembled: a herdr pane's environment, a host herdr
/// CLI, a verified cached Linux build, and a real socket to forward to.
/// `pall8t run` must announce the pane's agent to herdr, spawn the relay,
/// and mount the relay's socket into the sandbox — the transport this PR
/// introduced.
#[test]
fn a_run_inside_a_herdr_pane_builds_the_socket_bridge() {
    let sb = Sandbox::new("run-herdr");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));

    let herdr_bin = fake_host_herdr(&sb, "0.8.2");
    seed_verified_linux_herdr(&sb, "0.8.2");
    let herdr_sock = sb.root.join("herdr.sock");
    let host_herdr = std::os::unix::net::UnixListener::bind(&herdr_sock).unwrap();
    fake.vanish_after_image_list();

    let out = sb
        .command()
        .args(["run", "--", "claude"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "pane_42")
        .env("HERDR_TAB_ID", "tab_7")
        .env("HERDR_WORKSPACE_ID", "ws_1")
        .env("HERDR_SOCKET_PATH", &herdr_sock)
        .env("HERDR_BIN_PATH", &herdr_bin)
        .output()
        .unwrap();

    let err = stderr(&out);
    assert!(
        err.contains("herdr bridge active"),
        "with a pane, a socket, and a cached Linux binary all present, the \
         bridge must come up — a warning here means it silently degraded: {err}"
    );

    let relay_sockets: Vec<_> = std::fs::read_dir(sb.pall8t_root().join("run"))
        .expect("the relay creates its own run directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .collect();
    assert_eq!(
        relay_sockets.len(),
        1,
        "exactly one socket for this run: it is bound before `container run` \
         is told to mount it, and it is the bridge's whole transport: {relay_sockets:?}"
    );

    let staged: Vec<PathBuf> = sb
        .pall8t_root()
        .join("tools")
        .join("herdr-run")
        .read_dir()
        .expect("the per-run copy directory exists")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(
        staged.len(),
        1,
        "one private directory for this run: the shared cache is never the \
         mount source, so one concurrent sandbox cannot overwrite the binary \
         another is executing: {staged:?}"
    );
    let staged_bin = staged[0].join("herdr");
    assert!(
        staged_bin.is_file(),
        "and it holds the binary itself — an empty directory would mount \
         nothing and leave the sandbox without a herdr CLI: {staged:?}"
    );
    assert_eq!(
        std::fs::read(&staged_bin).unwrap(),
        std::fs::read(
            sb.pall8t_root()
                .join("tools")
                .join("herdr")
                .join("0.8.2")
                .join("herdr")
        )
        .unwrap(),
        "the copy must be of the *verified* cached build, byte for byte"
    );

    let herdr_calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        herdr_calls.contains("pane report-metadata"),
        "the pane's agent identity is announced to herdr, or the pane shows \
         no agent at all: {herdr_calls}"
    );
    assert!(
        !herdr_calls.contains("rename") && !herdr_calls.contains("tab list"),
        "and with [herdr] auto_rename unset nothing is named — naming is \
         opt-in, so a config that never asked for it must not even look at \
         the tab list (issue #71): {herdr_calls}"
    );
    assert!(
        err.contains("failed to exec `container`"),
        "everything above happens on the way to the launch, which then \
         reports the missing runtime: {err}"
    );
    drop(host_herdr);
}

/// One opted-in `pall8t run` in herdr pane `w13:p3`, whose tab `w13:t2` is
/// the sole tab of `w13` — so herdr's own auto label for it is `"1"` and
/// `tab_label` decides whether pall8t sees the label as its own to take.
/// The tab's *number* is 2 while its position is 1, which is what makes
/// the expected name `demo-2` rather than `demo-1`.
///
/// Returns the sandbox (for the log and argv files the detached agent
/// namer writes after the run returns) and the run's stderr.
fn opted_in_naming_run(sandbox: &str, tab_label: &str) -> (Sandbox, String) {
    let sb = Sandbox::new(sandbox);
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));
    let herdr_bin = fake_host_herdr(&sb, "0.8.2");
    std::fs::write(
        sb.root.join("herdr-tab-list.json"),
        format!(
            r#"{{"id":"cli:tab:list","result":{{"tabs":[{{"agent_status":"unknown","focused":true,"label":"{tab_label}","number":2,"pane_count":1,"tab_id":"w13:t2","workspace_id":"w13"}}],"type":"tab_list"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        sb.root.join("herdr-agent-list.json"),
        r#"{"id":"cli:agent:list","result":{"agents":[],"type":"agent_list"}}"#,
    )
    .unwrap();
    sb.write_project_config(
        "[herdr]\nsandbox = \"off\"\nauto_rename = true\nagent_name = \"demo\"\n",
    );
    fake.vanish_after_image_list();

    let out = sb
        .command()
        .args(["run", "--", "claude"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w13:p3")
        .env("HERDR_TAB_ID", "w13:t2")
        .env("HERDR_WORKSPACE_ID", "w13")
        .env("HERDR_BIN_PATH", &herdr_bin)
        .output()
        .unwrap();
    let err = stderr(&out);
    (sb, err)
}

/// Naming, opted into (issue #71). `sandbox = "off"` on purpose: naming is
/// about herdr's view of the pane, not about the bridge, so it has to
/// happen with the bridge switched off entirely — the mode that spawns no
/// relay, and the reason the agent half is its own child rather than the
/// relay's job.
#[test]
fn an_opted_in_run_names_the_tab_immediately_and_the_agent_after_the_exec() {
    // A tab still on herdr's own auto label ("1") is pall8t's to rename.
    let (sb, err) = opted_in_naming_run("run-naming", "1");
    assert!(
        err.contains(r#"naming this tab "demo-2""#)
            && err.contains("its agent takes the same name"),
        "the run says what it named, with [herdr] agent_name overriding the \
         directory basename and the tab's number as the suffix: {err}"
    );
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        calls.contains("tab rename w13:t2 demo-2"),
        "the tab is renamed before the exec — it needs no detected agent, so \
         the human sees the right label from the moment the run starts: {calls}"
    );

    // The agent half cannot run here: at this point the pane's agent is
    // still pall8t itself. It belongs to the detached child, which reports
    // to its own log because the pane's terminal now belongs to the agent.
    let log = wait_for_line(
        &sb.pall8t_root().join("logs").join("herdr-naming.log"),
        "named the agent",
        std::time::Duration::from_secs(10),
    );
    assert!(
        log.contains(r#"named the agent "demo-2""#),
        "the agent gets the same name as the tab, from a process that \
         outlives the exec: {log}"
    );
    // Re-read: the child runs after `pall8t run` has returned, so its
    // calls land in the log only once the wait above has seen it work.
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        calls.contains("agent rename w13:p3 demo-2"),
        "and it is a real `agent rename` on the pane that does it — the log \
         line alone would still be written by a rename that never happened: \
         {calls}"
    );
    assert!(
        calls.contains("agent get w13:p3"),
        "which it only attempts once herdr reports an agent in the pane: {calls}"
    );
    assert!(
        calls.contains("agent list"),
        "and the name was checked against the live agents first, so two runs \
         that would collide get distinct names: {calls}"
    );
}

/// The other half of naming's tab rule: a label the human chose is never
/// clobbered, while the agent is still named. Without this, forcing the
/// "is this tab still on herdr's own label?" check to `true` — renaming
/// every tab, including yours — passes the whole suite (a mutant
/// `cargo mutants` caught).
#[test]
fn a_tab_the_human_labeled_keeps_its_label_and_the_agent_is_still_named() {
    // "release work" is neither herdr's auto label nor a name pall8t
    // could have written here — only a human types it.
    let (sb, err) = opted_in_naming_run("run-naming-mine", "release work");
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        !calls.contains("tab rename"),
        "a label the human chose must survive the run untouched: {calls}"
    );
    assert!(
        err.contains(r#"this tab keeps its label "release work""#)
            && err.contains(r#"address the agent as "demo-2""#),
        "and the run must say so rather than claiming a tab it did not name — \
         naming both strings is what shows the human that the label they will \
         read off the tab is not the name that reaches this agent: {err}"
    );
    let log = wait_for_line(
        &sb.pall8t_root().join("logs").join("herdr-naming.log"),
        "named the agent",
        std::time::Duration::from_secs(10),
    );
    assert!(
        log.contains(r#"named the agent "demo-2""#),
        "the agent half is independent of the tab half and still runs: {log}"
    );
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        calls.contains("agent rename w13:p3 demo-2") && !calls.contains("tab rename"),
        "the agent really is renamed, and the tab really is not: {calls}"
    );
}

/// The agent half on its own, driven through the hidden subcommand the
/// run spawns.
///
/// This is the only place the *waiting* is exercised: herdr does not
/// recognize the sandboxed agent the instant `pall8t run` starts — it
/// only does once the argv0 hint takes effect, after the exec — so the
/// namer has to keep asking. In the launch tests above the run is already
/// over by the time the child looks, which is why the wait has to be
/// tested from a parent that stays alive.
#[test]
fn the_agent_namer_waits_for_herdr_to_detect_the_agent_then_stays_out_of_the_way() {
    let sb = Sandbox::new("namer-waits");
    let herdr_bin = fake_host_herdr(&sb, "0.8.2");
    // Two probes come back `agent_not_found`, as a pane whose agent herdr
    // has not identified yet really does; the third finds it.
    std::fs::write(sb.root.join("agent-get-ready"), "3").unwrap();
    let log = sb.root.join("naming.log");

    let mut child = sb
        .command()
        .args([
            "herdr",
            "name-agent",
            "--pane",
            "w13:p3",
            "--name",
            "demo-2",
            "--herdr-bin",
            herdr_bin.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let body = wait_for_line(&log, "named the agent", std::time::Duration::from_secs(30));
    assert!(
        body.contains(r#"named the agent "demo-2""#),
        "an agent herdr has not recognized yet must be waited for, not \
         treated as absent — dropping it is the difference between a named \
         agent and none at all: {body}"
    );
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert_eq!(
        calls.matches("agent get w13:p3").count(),
        3,
        "it asked until herdr answered, rather than once: {calls}"
    );
    assert!(
        calls.contains("agent rename w13:p3 demo-2"),
        "and then renamed the pane's agent for real: {calls}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the namer outlives its work on purpose: it is a child of a run \
         that has exec'd into the `container` client, which reaps nothing, \
         so exiting here would leave a <defunct> entry for the whole session"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// The race the pre-exec collision scan cannot close: another run takes
/// the free name between that scan and the rename. The namer walks the
/// counter on — and drags the tab label along, or the tab would advertise
/// a name that resolves to somebody else's agent.
#[test]
fn a_name_taken_since_the_scan_moves_the_agent_and_its_tab_label_together() {
    let sb = Sandbox::new("namer-collision");
    let herdr_bin = fake_host_herdr(&sb, "0.8.2");
    std::fs::write(sb.root.join("rename-taken"), "demo-2").unwrap();
    let log = sb.root.join("naming.log");

    let mut child = sb
        .command()
        .args([
            "herdr",
            "name-agent",
            "--pane",
            "w13:p3",
            "--name",
            "demo-2",
            // Passed only when pall8t labeled this tab itself, so the
            // relabel can never touch a label a human chose.
            "--tab",
            "w13:t2",
            "--herdr-bin",
            herdr_bin.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let body = wait_for_line(
        &log,
        "relabeled the tab",
        std::time::Duration::from_secs(30),
    );
    let calls = std::fs::read_to_string(sb.root.join("herdr-argv.log")).unwrap_or_default();
    assert!(
        calls.contains("agent rename w13:p3 demo-2")
            && calls.contains("agent rename w13:p3 demo-2-2"),
        "the taken name is tried, then extended — giving up would leave the \
         pane addressable only by its pane id: {calls}"
    );
    assert!(
        calls.contains("tab rename w13:t2 demo-2-2"),
        "and the tab follows the name the agent actually got: a tab reading \
         demo-2 would send `herdr agent prompt demo-2` to another run's \
         agent: {calls}"
    );
    assert!(
        body.contains(r#"named the agent "demo-2-2""#)
            && body.contains(r#"relabeled the tab "demo-2-2""#),
        "both halves are reported to the log, since nothing is watching the \
         pane at this point: {body}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_run_without_a_usable_herdr_socket_degrades_instead_of_failing() {
    let sb = Sandbox::new("run-nobridge");
    let fake = FakeRuntime::current(&sb);
    let tag = build_once(&sb, &fake);
    fake.set_images(std::slice::from_ref(&tag));
    fake.vanish_after_image_list();

    // A pane environment that names no socket: `prepare_bridge` cannot
    // build anything, and that must not take the run down with it.
    let out = sb
        .command()
        .args(["run"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "pane_42")
        .output()
        .unwrap();

    let err = stderr(&out);
    assert!(
        err.contains("herdr bridge disabled"),
        "a bridge that cannot be built is announced, not swallowed: {err}"
    );
    assert!(
        err.contains("failed to exec `container`"),
        "and the run continues to the launch regardless — the bridge is a \
         convenience, never a precondition: {err}"
    );
}

/// Spawns a relay from a shell that lives for `parent_lifetime_secs` and
/// then exits, so the test controls exactly when the relay is orphaned.
/// Returns the shell's handle and the socket the relay is told to bind.
///
/// `/bin/sleep` by absolute path: the sandbox hands its children an empty
/// `PATH`, and a bare `sleep` would simply not be found — the shell would
/// exit at once and every one of these tests would silently become the
/// already-orphaned case.
fn spawn_orphanable_relay(sb: &Sandbox, name: &str, parent_lifetime_secs: u32) -> (Child, PathBuf) {
    let listen = sb.pall8t_root().join("run").join(format!("{name}.sock"));
    let log = sb.root.join("relay.log");
    let child = sb
        .sh_command()
        .arg("-c")
        .arg(format!(
            "{} herdr relay --socket /nonexistent/herdr.sock --listen {} \
             --mode full --log {} >/dev/null 2>&1 & /bin/sleep {parent_lifetime_secs}",
            env!("CARGO_BIN_EXE_pall8t"),
            listen.display(),
            log.display(),
        ))
        .spawn()
        .unwrap();
    (child, listen)
}

/// The relay's lifetime is the run's. Nothing supervises it: it polls for
/// reparenting and exits once the pall8t process that spawned it is gone
/// (`pall8t run` becomes the `container` client via exec, keeping the same
/// pid, so the relay outlives the exec but not the session). Were that
/// check dropped or inverted, every run would leave a relay behind — each
/// holding a socket and a policy-checked path into the host herdr session.
#[test]
fn the_relay_exits_once_the_run_that_spawned_it_is_gone() {
    let sb = Sandbox::new("relay-lifetime");
    let (mut parent, listen) = spawn_orphanable_relay(&sb, "pall8t-served", 8);
    let alive = || std::os::unix::net::UnixStream::connect(&listen).is_ok();

    assert!(
        wait_until(std::time::Duration::from_secs(10), alive),
        "while the run that spawned it is alive, the relay serves"
    );

    // Past two poll intervals with the parent still there. Without this the
    // test cannot tell "exits when the run ends" from "exits on a timer" —
    // an inverted comparison, or one against a pid that was never the
    // parent's, kills the relay mid-session while the sandbox is still
    // using it, and every assertion below would still pass.
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(
        alive(),
        "and it must keep serving for as long as that run lives — a bridge \
         that drops out from under a working sandbox is worse than one that \
         leaks"
    );

    parent.wait().unwrap();

    assert!(
        wait_until(std::time::Duration::from_secs(30), || socket_is_dead(
            &listen
        )),
        "only then does it stop: a relay that outlives its run is a leaked \
         process bridging a sandbox that no longer exists"
    );
}

/// The same guarantee at the one moment it is easiest to lose: the parent
/// only has to live long enough to read the readiness line, so it can
/// already be gone by the time the relay looks at its own parent. Reading
/// `getppid()` at that point yields the reparent target, and comparing it
/// against itself is a condition that can never become true — the relay
/// would serve forever. A `pall8t run` that fails right after reading the
/// line reaches exactly this state.
#[test]
fn the_relay_does_not_outlive_a_run_that_was_already_gone() {
    let sb = Sandbox::new("relay-orphan");
    // `sleep 0`: the shell backgrounds the relay and exits at once, so the
    // relay is orphaned before it can look.
    let (mut parent, listen) = spawn_orphanable_relay(&sb, "pall8t-orphan", 0);
    parent.wait().unwrap();

    assert!(
        wait_until(std::time::Duration::from_secs(10), || listen.exists()),
        "the relay still got as far as binding — the socket file it left \
         behind is what proves this test observed a relay at all, rather \
         than one that never started"
    );
    assert!(
        wait_until(std::time::Duration::from_secs(30), || socket_is_dead(
            &listen
        )),
        "but nothing may still be serving it: with no run left to bridge, \
         staying up leaks a process for as long as the machine is on"
    );
}

/// Bound, but with nothing listening: the socket file is still there (the
/// relay does not unlink it) and a connect is refused.
fn socket_is_dead(path: &Path) -> bool {
    matches!(
        std::os::unix::net::UnixStream::connect(path).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
    )
}

/// Polls `cond` until it holds or `limit` elapses. Returns whether it held
/// — so a caller asserts on the answer instead of blocking the suite.
fn wait_until(limit: std::time::Duration, cond: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if cond() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
