use crate::config::{self, Config, ProjectEntry};
use crate::container::{self, State};
use crate::detect::{self, AgentPatterns, TabKind, TabState};
use crate::mux::{self, Tab};
use crate::workspace;
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub enum Mode {
    Normal,
    Prefix,
    AddProject(String),
    ConfirmClose,
    ConfirmQuit,
    Help,
    Logs {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
}

pub struct ProjectRow {
    pub entry: ProjectEntry,
    pub workspace: PathBuf,
    pub container: String,
    pub state: State,
}

/// Snapshot of everything the worker needs for one project.
#[derive(Clone)]
pub struct Ctx {
    pub idx: usize,
    pub entry: ProjectEntry,
    pub container: String,
    pub workspace: PathBuf,
    pub image_base: String,
    pub cpus: u32,
    pub memory: String,
}

pub enum Job {
    Refresh,
    Seed(Ctx),
    Ensure { ctx: Ctx, kind: TabKind },
    Toggle { ctx: Ctx, state: State },
    StopIdle(Ctx),
    Build(Ctx),
    Logs(String),
}

pub enum Msg {
    Containers(Vec<(String, State)>),
    Status(String),
    Done(String),
    Warning(String),
    Error(String),
    Seeded { idx: usize, summary: String },
    Ready { idx: usize, kind: TabKind },
    Logs { name: String, text: String },
}

pub struct App {
    pub config: Config,
    pub prefix_char: char,
    patterns: AgentPatterns,
    pub projects: Vec<ProjectRow>,
    pub current_project: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: Option<usize>,
    pub sidebar: bool,
    pub mode: Mode,
    pub status: String,
    pub busy: bool,
    pub should_quit: bool,
    term_rows: u16,
    term_cols: u16,
    auto_agent_tab: Option<usize>,
    jobs: Sender<Job>,
    msgs: Receiver<Msg>,
    last_refresh: Instant,
}

impl App {
    pub fn new(path_arg: Option<PathBuf>) -> Result<Self> {
        let mut cfg = config::load()?;
        let mut current = 0usize;
        let mut auto_agent_tab = None;

        if let Some(p) = path_arg {
            let abs = std::fs::canonicalize(&p)
                .with_context(|| format!("cannot resolve path: {}", p.display()))?;
            let name = abs
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());
            if let Some(i) = cfg.projects.iter().position(|e| e.repos.contains(&abs)) {
                current = i;
            } else if let Some(i) = cfg.projects.iter().position(|e| e.name == name) {
                // Same project name: register this repo with it.
                cfg.projects[i].repos.push(abs);
                config::save(&cfg)?;
                current = i;
            } else {
                cfg.projects.push(ProjectEntry {
                    name,
                    repos: vec![abs],
                    path: None,
                    image: None,
                    containerfile: None,
                });
                config::save(&cfg)?;
                current = cfg.projects.len() - 1;
            }
            auto_agent_tab = Some(current);
        }

        let (uid, gid) = container::host_ids();
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        thread::spawn(move || worker(job_rx, msg_tx, uid, gid));

        let projects: Vec<ProjectRow> = cfg
            .projects
            .iter()
            .map(|e| {
                let ws = workspace::workspace_path(&cfg.workspace_root, &e.name);
                ProjectRow {
                    container: container::container_name(&e.name, &ws),
                    workspace: ws,
                    state: State::Absent,
                    entry: e.clone(),
                }
            })
            .collect();

        let prefix_char = config::parse_prefix(&cfg.prefix);
        let patterns = AgentPatterns::from_config(&cfg);

        let app = Self {
            config: cfg,
            prefix_char,
            patterns,
            projects,
            current_project: current,
            tabs: Vec::new(),
            active_tab: None,
            sidebar: true,
            mode: Mode::Normal,
            status: String::new(),
            busy: false,
            should_quit: false,
            term_rows: 24,
            term_cols: 80,
            auto_agent_tab,
            jobs: job_tx,
            msgs: msg_rx,
            last_refresh: Instant::now(),
        };
        app.jobs.send(Job::Refresh).ok();
        if let Some(idx) = app.auto_agent_tab {
            if let Some(ctx) = app.ctx(idx) {
                app.jobs.send(Job::Seed(ctx)).ok();
            }
        }
        Ok(app)
    }

    fn ctx(&self, idx: usize) -> Option<Ctx> {
        self.projects.get(idx).map(|row| Ctx {
            idx,
            entry: row.entry.clone(),
            container: row.container.clone(),
            workspace: row.workspace.clone(),
            image_base: self.config.default_image.clone(),
            cpus: self.config.cpus,
            memory: self.config.memory.clone(),
        })
    }

    /// Inner terminal-widget size, pushed down to every PTY.
    pub fn set_term_size(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == (self.term_rows, self.term_cols) {
            return;
        }
        self.term_rows = rows;
        self.term_cols = cols;
        for tab in &mut self.tabs {
            tab.resize(rows, cols);
        }
    }

    pub fn waiting_tabs(&self) -> Vec<usize> {
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == TabState::Waiting)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn any_tab_running(&self) -> bool {
        self.tabs
            .iter()
            .any(|t| matches!(t.state, TabState::Working | TabState::Waiting))
    }

    /// Periodic work: worker messages, container refresh, state detection.
    pub fn tick(&mut self) {
        self.drain_worker();
        if self.last_refresh.elapsed() >= Duration::from_secs(2) {
            self.jobs.send(Job::Refresh).ok();
            self.last_refresh = Instant::now();
        }
        let mut notifications: Vec<String> = Vec::new();
        for tab in &mut self.tabs {
            let exited = tab.exited();
            let bottom = tab.bottom_text(12);
            let new = detect::classify(
                tab.kind,
                exited,
                tab.since_output(),
                &bottom,
                &self.patterns,
            );
            if new == TabState::Waiting && tab.state != TabState::Waiting {
                notifications.push(format!("{} is waiting for you", tab.title));
            }
            tab.state = new;
        }
        for n in notifications {
            self.notify(&n);
        }
    }

    fn notify(&self, message: &str) {
        match self.config.notify.as_str() {
            "off" => {}
            "banner" => {
                let script = format!(
                    "display notification \"{}\" with title \"pall8t\"",
                    message.replace('"', "'")
                );
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(script)
                    .spawn();
            }
            _ => {
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x07");
                let _ = out.flush();
            }
        }
    }

    fn drain_worker(&mut self) {
        while let Ok(msg) = self.msgs.try_recv() {
            match msg {
                Msg::Containers(list) => {
                    for row in &mut self.projects {
                        row.state = list
                            .iter()
                            .find(|(name, _)| *name == row.container)
                            .map(|(_, s)| *s)
                            .unwrap_or(State::Absent);
                    }
                }
                Msg::Status(s) => {
                    self.status = s;
                    self.busy = true;
                }
                Msg::Done(s) => {
                    self.status = s;
                    self.busy = false;
                }
                Msg::Warning(s) => {
                    self.status = format!("⚠ {s}");
                    self.busy = false;
                }
                Msg::Error(e) => {
                    self.status = format!("error: {e}");
                    self.busy = false;
                }
                Msg::Seeded { idx, summary } => {
                    self.status = summary;
                    self.busy = false;
                    if self.auto_agent_tab == Some(idx) {
                        self.auto_agent_tab = None;
                        if let Some(ctx) = self.ctx(idx) {
                            self.busy = true;
                            self.jobs.send(Job::Ensure {
                                ctx,
                                kind: TabKind::Agent,
                            })
                            .ok();
                        }
                    }
                }
                Msg::Ready { idx, kind } => {
                    self.busy = false;
                    if let Err(e) = self.open_tab(idx, kind) {
                        self.status = format!("error: {e:#}");
                    }
                }
                Msg::Logs { name, text } => {
                    self.busy = false;
                    self.mode = Mode::Logs {
                        title: name,
                        lines: text.lines().map(|s| s.to_string()).collect(),
                        scroll: 0,
                    };
                }
            }
        }
    }

    fn open_tab(&mut self, idx: usize, kind: TabKind) -> Result<()> {
        let row = self
            .projects
            .get(idx)
            .context("project disappeared")?;
        let cmd: Vec<String> = match kind {
            TabKind::Agent => self
                .config
                .agent_command
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            TabKind::Shell => vec!["bash".to_string(), "-l".to_string()],
        };
        let title = cmd.first().cloned().unwrap_or_else(|| "tab".to_string());
        let mut argv = vec!["container".to_string()];
        argv.extend(container::exec_argv(&row.container, &row.workspace, &cmd));
        let tab = Tab::spawn(
            idx,
            kind,
            title.clone(),
            &argv,
            self.term_rows,
            self.term_cols,
        )?;
        self.tabs.push(tab);
        self.active_tab = Some(self.tabs.len() - 1);
        self.current_project = idx;
        self.status = format!("opened {title} tab");
        Ok(())
    }

    fn request_tab(&mut self, kind: TabKind) {
        if let Some(ctx) = self.ctx(self.current_project) {
            self.busy = true;
            self.status = "preparing container…".to_string();
            self.jobs.send(Job::Ensure { ctx, kind }).ok();
        } else {
            self.status = "no project — press P to add one".to_string();
        }
    }

    fn close_active_tab(&mut self, force: bool) {
        let Some(i) = self.active_tab else { return };
        let running = matches!(
            self.tabs[i].state,
            TabState::Working | TabState::Waiting
        );
        if running && !force {
            self.mode = Mode::ConfirmClose;
            return;
        }
        let mut tab = self.tabs.remove(i);
        tab.kill();
        let project = tab.project;
        self.active_tab = if self.tabs.is_empty() {
            None
        } else {
            Some(i.min(self.tabs.len() - 1))
        };
        if let Some(a) = self.active_tab {
            self.current_project = self.tabs[a].project;
        }
        // Resource optimization: when a project's last tab closes, stop its
        // container. The next tab restarts it via `container start` (cheap).
        if !self.tabs.iter().any(|t| t.project == project)
            && self.projects.get(project).map(|r| r.state) == Some(State::Running)
        {
            if let Some(ctx) = self.ctx(project) {
                self.busy = true;
                self.jobs.send(Job::StopIdle(ctx)).ok();
            }
        }
    }

    fn select_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active_tab = Some(i);
            self.current_project = self.tabs[i].project;
        }
    }

    fn cycle_tab(&mut self, delta: i64) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i64;
        let cur = self.active_tab.unwrap_or(0) as i64;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.select_tab(next);
    }

    fn jump_next_waiting(&mut self) {
        let waiting = self.waiting_tabs();
        if waiting.is_empty() {
            self.status = "no tab is waiting".to_string();
            return;
        }
        let cur = self.active_tab.unwrap_or(0);
        let next = waiting
            .iter()
            .copied()
            .find(|&i| i > cur)
            .unwrap_or(waiting[0]);
        self.select_tab(next);
    }

    fn add_project(&mut self, input: &str) {
        let paths: Vec<PathBuf> = input
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| workspace::expand_tilde(&PathBuf::from(s)))
            .collect();
        if paths.is_empty() {
            return;
        }
        let mut repos = Vec::new();
        for p in paths {
            match std::fs::canonicalize(&p) {
                Ok(abs) => repos.push(abs),
                Err(e) => {
                    self.status = format!("cannot add {}: {e}", p.display());
                    return;
                }
            }
        }
        let name = repos[0]
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
        if self.config.projects.iter().any(|e| e.name == name) {
            self.status = format!("project `{name}` already exists");
            return;
        }
        let entry = ProjectEntry {
            name,
            repos,
            path: None,
            image: None,
            containerfile: None,
        };
        self.config.projects.push(entry.clone());
        if let Err(e) = config::save(&self.config) {
            self.status = format!("config save failed: {e}");
        }
        let ws = workspace::workspace_path(&self.config.workspace_root, &entry.name);
        self.projects.push(ProjectRow {
            container: container::container_name(&entry.name, &ws),
            workspace: ws,
            state: State::Absent,
            entry,
        });
        self.current_project = self.projects.len() - 1;
        if let Some(ctx) = self.ctx(self.current_project) {
            self.busy = true;
            self.jobs.send(Job::Seed(ctx)).ok();
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if matches!(self.mode, Mode::Normal) {
            if let Some(i) = self.active_tab {
                let bytes = mux::encode_paste(text);
                self.tabs[i].write_bytes(&bytes);
            }
        } else if let Mode::AddProject(input) = &mut self.mode {
            input.push_str(text);
        }
    }

    fn is_prefix(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char(self.prefix_char)
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => {
                if self.is_prefix(code, mods) {
                    self.mode = Mode::Prefix;
                } else if self.active_tab.is_some() {
                    if let Some(bytes) = mux::encode_key(code, mods) {
                        if let Some(i) = self.active_tab {
                            self.tabs[i].write_bytes(&bytes);
                        }
                    }
                } else {
                    // No tabs: commands work without the prefix.
                    self.command_key(code);
                }
            }
            Mode::Prefix => {
                if self.is_prefix(code, mods) {
                    // prefix twice sends the prefix itself to the tab
                    if let Some(i) = self.active_tab {
                        let byte = (self.prefix_char.to_ascii_lowercase() as u8) & 0x1f;
                        self.tabs[i].write_bytes(&[byte]);
                    }
                } else {
                    self.command_key(code);
                }
            }
            Mode::AddProject(mut input) => match code {
                KeyCode::Enter => self.add_project(&input.clone()),
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::AddProject(input);
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.mode = Mode::AddProject(input);
                }
                _ => self.mode = Mode::AddProject(input),
            },
            Mode::ConfirmClose => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.close_active_tab(true),
                _ => {}
            },
            Mode::ConfirmQuit => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.quit(true),
                _ => {}
            },
            Mode::Help => {}
            Mode::Logs {
                title,
                lines,
                scroll,
            } => match code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    let max = lines.len().saturating_sub(1);
                    self.mode = Mode::Logs {
                        title,
                        lines,
                        scroll: (scroll + 1).min(max),
                    };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.mode = Mode::Logs {
                        title,
                        lines,
                        scroll: scroll.saturating_sub(1),
                    };
                }
                KeyCode::Char('g') => {
                    self.mode = Mode::Logs {
                        title,
                        lines,
                        scroll: 0,
                    };
                }
                KeyCode::Char('G') => {
                    let max = lines.len().saturating_sub(1);
                    self.mode = Mode::Logs {
                        title,
                        lines,
                        scroll: max,
                    };
                }
                _ => {
                    self.mode = Mode::Logs {
                        title,
                        lines,
                        scroll,
                    };
                }
            },
        }
    }

    /// A command key: after the prefix, or bare when no tabs exist.
    fn command_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('a') => self.request_tab(TabKind::Agent),
            KeyCode::Char('c') => self.request_tab(TabKind::Shell),
            KeyCode::Char('n') => self.jump_next_waiting(),
            KeyCode::Char('j') => self.cycle_tab(1),
            KeyCode::Char('k') => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = (c as u8 - b'1') as usize;
                self.select_tab(i);
            }
            KeyCode::Char('p') => {
                if !self.projects.is_empty() {
                    self.current_project = (self.current_project + 1) % self.projects.len();
                    let name = self.projects[self.current_project].entry.name.clone();
                    self.status = format!("project: {name}");
                }
            }
            KeyCode::Char('P') => self.mode = Mode::AddProject(String::new()),
            KeyCode::Char('x') => self.close_active_tab(false),
            KeyCode::Char('s') => {
                let state = self.projects.get(self.current_project).map(|r| r.state);
                if let (Some(state), Some(ctx)) = (state, self.ctx(self.current_project)) {
                    self.busy = true;
                    self.status = "…".to_string();
                    self.jobs.send(Job::Toggle { ctx, state }).ok();
                }
            }
            KeyCode::Char('b') => {
                if let Some(ctx) = self.ctx(self.current_project) {
                    self.busy = true;
                    self.jobs.send(Job::Build(ctx)).ok();
                }
            }
            KeyCode::Char('L') => {
                let info = self
                    .projects
                    .get(self.current_project)
                    .map(|r| (r.state, r.container.clone()));
                if let Some((state, name)) = info {
                    if state == State::Absent {
                        self.status = "no container yet".to_string();
                    } else {
                        self.busy = true;
                        self.jobs.send(Job::Logs(name)).ok();
                    }
                }
            }
            KeyCode::Char('z') => self.sidebar = !self.sidebar,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('q') => self.quit(false),
            _ => {}
        }
    }

    fn quit(&mut self, force: bool) {
        if self.any_tab_running() && !force {
            self.mode = Mode::ConfirmQuit;
            return;
        }
        for tab in &mut self.tabs {
            tab.kill();
        }
        self.should_quit = true;
    }
}

fn worker(jobs: Receiver<Job>, msgs: Sender<Msg>, uid: u32, gid: u32) {
    while let Ok(job) = jobs.recv() {
        if let Err(e) = handle_job(job, &msgs, uid, gid) {
            let _ = msgs.send(Msg::Error(format!("{e:#}")));
        }
    }
}

fn handle_job(job: Job, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<()> {
    match job {
        Job::Refresh => match container::list_all() {
            Ok(list) => {
                let _ = msgs.send(Msg::Containers(list));
            }
            Err(e) => {
                if !container::cli_available() {
                    let _ = msgs.send(Msg::Warning(
                        "`container` CLI not found — install apple/container first".to_string(),
                    ));
                } else if !container::system_running() {
                    let _ = msgs.send(Msg::Warning(
                        "container system is not running — run `container system start`"
                            .to_string(),
                    ));
                } else {
                    return Err(e);
                }
            }
        },
        Job::Seed(ctx) => {
            let _ = msgs.send(Msg::Status(format!(
                "seeding workspace for {}…",
                ctx.entry.name
            )));
            let summary = workspace::seed(&ctx.workspace, &ctx.entry)?;
            let _ = msgs.send(Msg::Seeded {
                idx: ctx.idx,
                summary,
            });
        }
        Job::Ensure { ctx, kind } => {
            workspace::seed(&ctx.workspace, &ctx.entry)?;
            ensure_running(&ctx, msgs, uid, gid)?;
            let _ = msgs.send(Msg::Ready { idx: ctx.idx, kind });
            refresh(msgs);
        }
        Job::Toggle { ctx, state } => {
            match state {
                State::Running => {
                    let _ = msgs.send(Msg::Status(format!("stopping {}…", ctx.container)));
                    container::stop(&ctx.container)?;
                }
                State::Stopped => {
                    let _ = msgs.send(Msg::Status(format!("starting {}…", ctx.container)));
                    container::start(&ctx.container)?;
                }
                State::Absent => {
                    workspace::seed(&ctx.workspace, &ctx.entry)?;
                    ensure_running(&ctx, msgs, uid, gid)?;
                }
            }
            refresh(msgs);
            let _ = msgs.send(Msg::Done("done".to_string()));
        }
        Job::StopIdle(ctx) => {
            let _ = msgs.send(Msg::Status(format!(
                "no tabs left — stopping {}…",
                ctx.container
            )));
            container::stop(&ctx.container)?;
            refresh(msgs);
            let _ = msgs.send(Msg::Done(format!("stopped {} (idle)", ctx.container)));
        }
        Job::Build(ctx) => {
            let tag = build_image(&ctx, msgs, uid, gid)?;
            let _ = msgs.send(Msg::Done(format!("image built: {tag}")));
        }
        Job::Logs(name) => {
            let text = container::logs(&name)?;
            let _ = msgs.send(Msg::Logs { name, text });
        }
    }
    Ok(())
}

fn refresh(msgs: &Sender<Msg>) {
    if let Ok(list) = container::list_all() {
        let _ = msgs.send(Msg::Containers(list));
    }
}

/// Containerfile for a project. Priority: explicit config `containerfile`
/// (workspace-relative) > auto-detected `<repo>/.pall8t/Containerfile`
/// (first source repo that has one) > None (embedded default).
fn project_containerfile(ctx: &Ctx) -> Option<PathBuf> {
    if let Some(cf) = &ctx.entry.containerfile {
        return Some(if cf.is_absolute() {
            cf.clone()
        } else {
            ctx.workspace.join(cf)
        });
    }
    ctx.entry
        .repos
        .iter()
        .map(|r| {
            workspace::expand_tilde(r)
                .join(".pall8t")
                .join("Containerfile")
        })
        .find(|p| p.is_file())
}

fn resolve_image(ctx: &Ctx, uid: u32, gid: u32) -> String {
    if let Some(img) = &ctx.entry.image {
        return img.clone();
    }
    if project_containerfile(ctx).is_some() {
        // Project-specific tag so the shared pall8t-base is not overwritten.
        let base = format!("pall8t-{}", workspace::slug(&ctx.entry.name));
        return container::image_tag(&base, uid, gid);
    }
    container::image_tag(&ctx.image_base, uid, gid)
}

fn build_image(ctx: &Ctx, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<String> {
    let tag = resolve_image(ctx, uid, gid);
    let containerfile = match project_containerfile(ctx) {
        Some(cf) => cf,
        None => container::default_containerfile_path()?,
    };
    let ctx_dir = containerfile
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = msgs.send(Msg::Status(format!(
        "building {tag} (this can take a few minutes)…"
    )));
    container::build_image(&containerfile, &ctx_dir, &tag, uid, gid)?;
    Ok(tag)
}

fn ensure_running(ctx: &Ctx, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<()> {
    std::fs::create_dir_all(&ctx.workspace)?;
    let list = container::list_all()?;
    let mut state = list
        .iter()
        .find(|(name, _)| *name == ctx.container)
        .map(|(_, s)| *s)
        .unwrap_or(State::Absent);
    let tag = resolve_image(ctx, uid, gid);

    // The resolved image can change after a container was created (e.g. a
    // .pall8t/Containerfile appeared). Recreate stopped containers; only
    // warn for running ones (they may have live exec sessions).
    if state != State::Absent {
        if let Some(current) = container::image_ref(&ctx.container) {
            if current != tag {
                if state == State::Running {
                    let _ = msgs.send(Msg::Warning(format!(
                        "{} runs outdated image {current} (want {tag}) — close its tabs, then reopen to recreate",
                        ctx.container
                    )));
                } else {
                    let _ = msgs.send(Msg::Status(format!(
                        "image changed ({current} → {tag}) — recreating {}…",
                        ctx.container
                    )));
                    // A just-stopped container can still be shutting down
                    // ("stopping" reads as non-running); stop is a no-op if
                    // already down, and delete gets a few retries.
                    let _ = container::stop(&ctx.container);
                    let mut result = Ok(());
                    for attempt in 0..5 {
                        result = container::delete(&ctx.container);
                        if result.is_ok() {
                            break;
                        }
                        if attempt < 4 {
                            std::thread::sleep(Duration::from_millis(800));
                        }
                    }
                    result?;
                    state = State::Absent;
                }
            }
        }
    }

    match state {
        State::Running => {}
        State::Stopped => {
            let _ = msgs.send(Msg::Status(format!("starting {}…", ctx.container)));
            container::start(&ctx.container)?;
        }
        State::Absent => {
            if !container::image_exists(&tag) {
                build_image(ctx, msgs, uid, gid)?;
            }
            let _ = msgs.send(Msg::Status(format!("creating {}…", ctx.container)));
            container::run_detached(&container::RunSpec {
                name: ctx.container.clone(),
                workspace: ctx.workspace.clone(),
                image: tag,
                cpus: ctx.cpus,
                memory: ctx.memory.clone(),
                uid,
                gid,
            })?;
        }
    }
    Ok(())
}
