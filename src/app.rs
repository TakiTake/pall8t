use crate::config::{self, Config, ProjectEntry};
use crate::container::{self, State};
use crate::detect::{self, AgentPatterns, TabKind, TabState};
use crate::mux::{self, Tab};
use crate::proto;
use crate::registry::{self, TabEntry};
use crate::workspace;
use anyhow::{anyhow, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub enum Mode {
    Normal,
    Prefix,
    AddProject(String),
    ConfirmClose,
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
    term_origin: (u16, u16),
    auto_agent_tab: Option<usize>,
    config_mtime: Option<SystemTime>,
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
            let abs_for_mutation = abs.clone();
            let name_for_mutation = name.clone();
            cfg = config::locked_mutate(move |c| {
                if c.projects.iter().any(|e| e.repos.contains(&abs_for_mutation)) {
                    return;
                }
                if let Some(e) = c
                    .projects
                    .iter_mut()
                    .find(|e| e.name == name_for_mutation)
                {
                    e.repos.push(abs_for_mutation);
                    return;
                }
                c.projects.push(ProjectEntry {
                    name: name_for_mutation,
                    repos: vec![abs_for_mutation],
                    path: None,
                    image: None,
                    containerfile: None,
                });
            })?;
            current = cfg
                .projects
                .iter()
                .position(|e| e.repos.contains(&abs))
                .or_else(|| cfg.projects.iter().position(|e| e.name == name))
                .unwrap_or(0);
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

        let mut app = Self {
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
            term_origin: (0, 0),
            auto_agent_tab,
            config_mtime: config::mtime(),
            jobs: job_tx,
            msgs: msg_rx,
            last_refresh: Instant::now(),
        };
        app.jobs.send(Job::Refresh).ok();
        app.reattach_existing();
        if let Some(idx) = app.auto_agent_tab {
            if let Some(ctx) = app.ctx(idx) {
                app.busy = true;
                app.jobs.send(Job::Seed(ctx)).ok();
            }
        }
        Ok(app)
    }

    /// Reconnect to holders left behind by previous instances (detach →
    /// reattach), pruning entries whose holder died.
    fn reattach_existing(&mut self) {
        let reg = registry::locked(|reg| {
            reg.tabs.retain(|t| {
                if registry::pid_alive(t.pid) {
                    true
                } else {
                    let _ = std::fs::remove_file(&t.socket);
                    let _ = std::fs::remove_file(proto::exited_marker(&t.socket));
                    false
                }
            });
            reg.clone()
        });
        let reg = match reg {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("error: {e:#}");
                return;
            }
        };
        for entry in &reg.tabs {
            let Some(project) = self
                .projects
                .iter()
                .position(|r| r.entry.name == entry.project)
            else {
                self.status = format!(
                    "tab {} belongs to unknown project `{}` — left detached",
                    entry.title, entry.project
                );
                continue;
            };
            let kind = if entry.kind == "agent" {
                TabKind::Agent
            } else {
                TabKind::Shell
            };
            match mux::attach(
                &entry.id,
                project,
                &entry.project,
                kind,
                &entry.title,
                &entry.socket,
                self.term_rows,
                self.term_cols,
            ) {
                Ok(tab) => self.tabs.push(tab),
                Err(e) => self.status = format!("reattach {} failed: {e:#}", entry.title),
            }
        }
        if !self.tabs.is_empty() {
            self.active_tab = Some(0);
            self.current_project = self.tabs[0].project;
            self.status = format!("reattached {} tab(s)", self.tabs.len());
        }
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

    /// Inner terminal-widget geometry: size is pushed down to every holder,
    /// the origin is kept for mouse hit-testing.
    pub fn set_term_area(&mut self, x: u16, y: u16, rows: u16, cols: u16) {
        self.term_origin = (x, y);
        if (rows, cols) == (self.term_rows, self.term_cols) {
            return;
        }
        self.term_rows = rows;
        self.term_cols = cols;
        for tab in &mut self.tabs {
            tab.resize(rows, cols);
        }
    }

    /// Mouse wheel in the terminal area: scroll our history, unless the app
    /// inside enabled mouse reporting — then forward the wheel (SGR).
    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let up = match ev.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        let (ox, oy) = self.term_origin;
        let inside = ev.column >= ox
            && ev.column < ox.saturating_add(self.term_cols)
            && ev.row >= oy
            && ev.row < oy.saturating_add(self.term_rows);
        if !inside {
            return;
        }
        let Some(i) = self.active_tab else { return };
        let tab = &mut self.tabs[i];
        if tab.wants_mouse() {
            let col = ev.column - ox + 1;
            let row = ev.row - oy + 1;
            let button = if up { 64 } else { 65 };
            let seq = format!("\x1b[<{button};{col};{row}M");
            tab.write_bytes(seq.as_bytes());
        } else if up {
            tab.scroll(3);
        } else {
            tab.scroll(-3);
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

    /// Periodic work: worker messages, container refresh, config reload,
    /// per-tab state detection.
    pub fn tick(&mut self) {
        self.drain_worker();
        if self.last_refresh.elapsed() >= Duration::from_secs(2) {
            self.jobs.send(Job::Refresh).ok();
            self.last_refresh = Instant::now();
            let mtime = config::mtime();
            if mtime != self.config_mtime {
                self.reload_config();
            }
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

    /// Re-read config.toml (another instance may have changed it) and remap
    /// runtime state onto the new project list.
    fn reload_config(&mut self) {
        let Ok(cfg) = config::load() else { return };
        let old_states: Vec<(String, State)> = self
            .projects
            .iter()
            .map(|r| (r.entry.name.clone(), r.state))
            .collect();
        self.projects = cfg
            .projects
            .iter()
            .map(|e| {
                let ws = workspace::workspace_path(&cfg.workspace_root, &e.name);
                let state = old_states
                    .iter()
                    .find(|(n, _)| *n == e.name)
                    .map(|(_, s)| *s)
                    .unwrap_or(State::Absent);
                ProjectRow {
                    container: container::container_name(&e.name, &ws),
                    workspace: ws,
                    state,
                    entry: e.clone(),
                }
            })
            .collect();
        self.prefix_char = config::parse_prefix(&cfg.prefix);
        self.patterns = AgentPatterns::from_config(&cfg);
        self.config = cfg;
        for tab in &mut self.tabs {
            if let Some(i) = self
                .projects
                .iter()
                .position(|r| r.entry.name == tab.project_name)
            {
                tab.project = i;
            }
        }
        if self.current_project >= self.projects.len() {
            self.current_project = self.projects.len().saturating_sub(1);
        }
        self.config_mtime = config::mtime();
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
                            self.jobs
                                .send(Job::Ensure {
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

    /// Container is running: spawn a detached holder, register it, attach.
    fn open_tab(&mut self, idx: usize, kind: TabKind) -> Result<()> {
        let (project_name, container_name, ws) = {
            let row = self.projects.get(idx).context("project disappeared")?;
            (
                row.entry.name.clone(),
                row.container.clone(),
                row.workspace.clone(),
            )
        };
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
        argv.extend(container::exec_argv(&container_name, &ws, &cmd));

        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let id = format!("t{:x}-{:x}", nanos, std::process::id());
        let socket = registry::tabs_dir()?.join(format!("{id}.sock"));

        let pid = mux::spawn_holder(&id, &socket, self.term_rows, self.term_cols, &argv)?;
        registry::locked(|reg| {
            reg.tabs.push(TabEntry {
                id: id.clone(),
                project: project_name.clone(),
                kind: match kind {
                    TabKind::Agent => "agent".to_string(),
                    TabKind::Shell => "shell".to_string(),
                },
                title: title.clone(),
                pid,
                socket: socket.clone(),
                container: container_name.clone(),
                workspace: ws.clone(),
            });
        })?;
        let tab = mux::attach(
            &id,
            idx,
            &project_name,
            kind,
            &title,
            &socket,
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
        let tab_id = tab.id.clone();
        let project_name = tab.project_name.clone();
        let project_idx = tab.project;
        self.active_tab = if self.tabs.is_empty() {
            None
        } else {
            Some(i.min(self.tabs.len() - 1))
        };
        if let Some(a) = self.active_tab {
            self.current_project = self.tabs[a].project;
        }
        // Resource optimization, multi-instance safe: stop the container
        // only when the registry shows zero live tabs for this project
        // across ALL pall8t instances (ADR-0005).
        let remaining = registry::locked(|reg| {
            reg.tabs.retain(|t| t.id != tab_id);
            reg.tabs
                .iter()
                .filter(|t| t.project == project_name && registry::pid_alive(t.pid))
                .count()
        })
        .unwrap_or(usize::MAX);
        if remaining == 0
            && self.projects.get(project_idx).map(|r| r.state) == Some(State::Running)
        {
            if let Some(ctx) = self.ctx(project_idx) {
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
        let name_check = name.clone();
        let result = config::locked_mutate(move |c| {
            if !c.projects.iter().any(|e| e.name == name_check) {
                c.projects.push(ProjectEntry {
                    name: name_check,
                    repos,
                    path: None,
                    image: None,
                    containerfile: None,
                });
            }
        });
        if let Err(e) = result {
            self.status = format!("config save failed: {e}");
            return;
        }
        self.reload_config();
        if let Some(i) = self.projects.iter().position(|r| r.entry.name == name) {
            self.current_project = i;
            if let Some(ctx) = self.ctx(i) {
                self.busy = true;
                self.jobs.send(Job::Seed(ctx)).ok();
            }
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
                    self.command_key(code);
                }
            }
            Mode::Prefix => {
                if self.is_prefix(code, mods) {
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
            // Detach: holders (and the agents inside) keep running.
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
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
            // An explicit `image` is never built — see `resolve_image`.
            if let Some(img) = &ctx.entry.image {
                let _ = msgs.send(Msg::Done(format!(
                    "project uses explicit image {img} — nothing to build"
                )));
                return Ok(());
            }
            let resolved = resolve_image(&ctx, uid, gid);
            let (tag, pruned) = build_image(&ctx, &resolved, msgs, uid, gid)?;
            let suffix = pruned.map(|p| format!(" ({p})")).unwrap_or_default();
            let _ = msgs.send(Msg::Done(format!("image built: {tag}{suffix}")));
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

/// What a project resolves to for building/running: the tag, plus (for a
/// hash-suffixed project build) the exact Containerfile and content hash
/// that produced it. Threading `containerfile`/`hash` through explicitly —
/// rather than having [`build_image`] re-derive them via
/// [`project_containerfile`]/[`container::containerfile_content_hash`] —
/// closes a TOCTOU window: the file on disk could otherwise change between
/// resolution and the build actually running, so the built image's content
/// might not match what was hashed into its tag.
struct ResolvedImage {
    tag: String,
    /// Bare `pall8t-<slug>` base whose superseded builds may be pruned
    /// after a successful build; `None` for the explicit-`image` and
    /// default-image cases, which are never pruned.
    prune_base: Option<String>,
    /// Containerfile to build against; `None` means the embedded default
    /// (explicit-`image` and no-project-Containerfile cases).
    containerfile: Option<PathBuf>,
    /// Content hash embedded in `tag`. `Some` only when `containerfile` is
    /// `Some` and its content was readable at resolve time; `None` (with
    /// `containerfile` still `Some`) means the file exists but couldn't be
    /// hashed (e.g. a transient read failure), and `tag` fell back to the
    /// unsuffixed form.
    hash: Option<String>,
}

/// Resolves the image for a project. Priority: explicit config `image`
/// (never built — see `Job::Build` and `ensure_running`, which run it
/// as-is and let the CLI pull or fail) > a project Containerfile
/// (explicit `containerfile` config or auto-detected
/// `<repo>/.pall8t/Containerfile`), tagged with a hash of its current
/// content when readable, else an unsuffixed fallback tag > the embedded
/// default Containerfile.
fn resolve_image(ctx: &Ctx, uid: u32, gid: u32) -> ResolvedImage {
    if let Some(img) = &ctx.entry.image {
        return ResolvedImage {
            tag: img.clone(),
            prune_base: None,
            containerfile: None,
            hash: None,
        };
    }
    if let Some(cf) = project_containerfile(ctx) {
        // Project-specific tag so the shared pall8t-base is not overwritten.
        let base = format!("pall8t-{}", workspace::slug(&ctx.entry.name));
        // Hash the Containerfile's working-tree contents (not its last
        // commit), so uncommitted edits are detected too, and a rebuild can
        // never poison a tag: the same content always resolves to the same
        // tag, so tag and image content always correspond. Falls back to
        // the unsuffixed tag when the file can't be read.
        return match container::containerfile_content_hash(&cf) {
            Some(hash) => ResolvedImage {
                tag: container::image_tag_hashed(&base, uid, gid, &hash),
                prune_base: Some(base),
                containerfile: Some(cf),
                hash: Some(hash),
            },
            None => ResolvedImage {
                tag: container::image_tag(&base, uid, gid),
                prune_base: None,
                containerfile: Some(cf),
                hash: None,
            },
        };
    }
    ResolvedImage {
        tag: container::image_tag(&ctx.image_base, uid, gid),
        prune_base: None,
        containerfile: None,
        hash: None,
    }
}

/// Outcome of one [`try_build`] attempt.
enum BuildAttempt {
    Done(Option<String>),
    /// The Containerfile's content no longer matches what was hashed into
    /// `resolved.tag` — the just-built image was deleted rather than kept
    /// under a misleading tag. See [`build_image`] for the retry.
    Poisoned,
}

/// Runs `container build` for `resolved.tag` against `resolved`'s exact
/// Containerfile (the embedded default when `None`) — never re-derived,
/// see [`ResolvedImage`]. For a hash-suffixed tag, re-hashes that same
/// path afterwards to confirm nothing changed mid-build; a mismatch is
/// reported as [`BuildAttempt::Poisoned`] after deleting the mistagged
/// image, unless `ctx`'s container currently runs that exact tag (the
/// manual rebuild path can poison a tag that didn't change), in which
/// case the delete is skipped and a warning sent instead. Otherwise,
/// best-effort prunes superseded builds under `resolved.prune_base`,
/// excluding whatever image `ctx`'s container currently runs (if any) —
/// see [`prune_superseded_images`].
fn try_build(
    ctx: &Ctx,
    resolved: &ResolvedImage,
    msgs: &Sender<Msg>,
    uid: u32,
    gid: u32,
) -> Result<BuildAttempt> {
    let containerfile = match &resolved.containerfile {
        Some(cf) => cf.clone(),
        None => container::default_containerfile_path()?,
    };
    let ctx_dir = containerfile
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = msgs.send(Msg::Status(format!(
        "building {} (this can take a few minutes)…",
        resolved.tag
    )));
    container::build_image(&containerfile, &ctx_dir, &resolved.tag, uid, gid)?;

    let in_use = container::image_ref(&ctx.container);

    if let Some(hash) = &resolved.hash {
        match container::containerfile_content_hash(&containerfile) {
            Some(fresh) if fresh != *hash => {
                if in_use
                    .as_deref()
                    .is_some_and(|u| container::ref_matches(u, &resolved.tag))
                {
                    let _ = msgs.send(Msg::Warning(format!(
                        "mistagged image {} left in place — container {} currently uses it \
                         (it will be superseded on the next build)",
                        resolved.tag, ctx.container
                    )));
                } else if let Err(e) = container::image_delete(&resolved.tag) {
                    let _ = msgs.send(Msg::Warning(format!(
                        "could not delete poisoned tag {}: {e:#}",
                        resolved.tag
                    )));
                }
                return Ok(BuildAttempt::Poisoned);
            }
            Some(_) => {}
            None => {
                let _ = msgs.send(Msg::Warning(format!(
                    "could not re-read {} after building {} to confirm its tag — continuing",
                    containerfile.display(),
                    resolved.tag
                )));
            }
        }
    }

    let pruned = resolved.prune_base.as_deref().and_then(|base| {
        prune_superseded_images(base, &resolved.tag, uid, gid, in_use.as_deref(), msgs)
    });
    Ok(BuildAttempt::Done(pruned))
}

/// Builds the image for `resolved` (see [`resolve_image`]), retrying ONCE
/// against freshly re-resolved content if the Containerfile changed
/// during the first attempt (see [`try_build`]'s `Poisoned` case) — a
/// bounded retry, so a Containerfile edited faster than it can be built
/// fails loudly instead of looping forever. Returns the tag actually
/// built (which can differ from `resolved.tag` after a retry) and the
/// prune summary, if any.
fn build_image(
    ctx: &Ctx,
    resolved: &ResolvedImage,
    msgs: &Sender<Msg>,
    uid: u32,
    gid: u32,
) -> Result<(String, Option<String>)> {
    if let BuildAttempt::Done(pruned) = try_build(ctx, resolved, msgs, uid, gid)? {
        return Ok((resolved.tag.clone(), pruned));
    }
    let retry = resolve_image(ctx, uid, gid);
    match try_build(ctx, &retry, msgs, uid, gid)? {
        BuildAttempt::Done(pruned) => Ok((retry.tag, pruned)),
        BuildAttempt::Poisoned => Err(anyhow!(
            "Containerfile for {} keeps changing during build — wait for it to settle and try again",
            ctx.entry.name
        )),
    }
}

/// Deletes superseded builds under `base` for this `uid`/`gid`, keeping
/// `keep_tag` and skipping `in_use` (the image `ctx`'s container currently
/// runs, if any — deleting it out from under a live/stopped container
/// would break it; a later build will prune it once the container is
/// recreated against the new tag). See [`container::prunable_images`] for
/// the matching/dedup rules. Best-effort: a failure to list or delete
/// images is reported as a warning but never aborts the build that just
/// succeeded. Returns a short summary (e.g. "pruned 2 superseded", "pruned
/// 1, failed to prune 1") for the caller to surface, or `None` if there
/// was nothing to prune.
fn prune_superseded_images(
    base: &str,
    keep_tag: &str,
    uid: u32,
    gid: u32,
    in_use: Option<&str>,
    msgs: &Sender<Msg>,
) -> Option<String> {
    match container::prunable_images(base, keep_tag, uid, gid, in_use) {
        Ok(tags) => {
            let (mut pruned, mut failed) = (0u32, 0u32);
            for old in tags {
                match container::image_delete(&old) {
                    Ok(()) => pruned += 1,
                    Err(e) => {
                        failed += 1;
                        let _ = msgs.send(Msg::Warning(format!(
                            "could not prune superseded image {old}: {e:#}"
                        )));
                    }
                }
            }
            match (pruned, failed) {
                (0, 0) => None,
                (p, 0) => Some(format!("pruned {p} superseded")),
                (0, f) => Some(format!("failed to prune {f}")),
                (p, f) => Some(format!("pruned {p}, failed to prune {f}")),
            }
        }
        Err(e) => {
            let _ = msgs.send(Msg::Warning(format!(
                "could not list images to prune under {base}: {e:#}"
            )));
            None
        }
    }
}

fn ensure_running(ctx: &Ctx, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<()> {
    std::fs::create_dir_all(&ctx.workspace)?;
    let list = container::list_all()?;
    let mut state = list
        .iter()
        .find(|(name, _)| *name == ctx.container)
        .map(|(_, s)| *s)
        .unwrap_or(State::Absent);
    let resolved = resolve_image(ctx, uid, gid);

    // A project Containerfile that exists but is transiently unreadable
    // (e.g. an editor's atomic-save window) makes `resolved` fall back to
    // an unsuffixed tag (see `resolve_image`). Treating that fallback as a
    // real image change would recreate a perfectly good stopped container
    // and kick off a rebuild for nothing, so skip the staleness check in
    // that case and leave the container as-is. A Containerfile
    // disappearing entirely (`containerfile.is_none()`) is a legitimate
    // switch back to the default image and must still recreate.
    let transient_read_failure = resolved.containerfile.is_some() && resolved.hash.is_none();

    // The resolved image can change after a container was created (e.g. a
    // .pall8t/Containerfile appeared). Recreate stopped containers; only
    // warn for running ones (they may have live exec sessions). The
    // comparison is qualification/digest-aware (see `container::ref_matches`),
    // since `current` (from `container inspect`) can be registry- or
    // digest-qualified even when `resolved.tag` never is.
    if state != State::Absent && !transient_read_failure {
        if let Some(current) = container::image_ref(&ctx.container) {
            if !container::ref_matches(&current, &resolved.tag) {
                if state == State::Running {
                    let _ = msgs.send(Msg::Warning(format!(
                        "{} runs outdated image {current} (want {}) — close its tabs, then reopen to recreate",
                        ctx.container, resolved.tag
                    )));
                } else {
                    let _ = msgs.send(Msg::Status(format!(
                        "image changed ({current} → {}) — recreating {}…",
                        resolved.tag, ctx.container
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
            // An explicit `image` is never built (see `resolve_image`) —
            // hand it straight to `run_detached`, which lets the CLI pull
            // it or fail with a clear error, rather than silently building
            // and running the wrong (pall8t Containerfile) image under
            // the user's chosen reference.
            let run_tag = if ctx.entry.image.is_some() || container::image_exists(&resolved.tag) {
                resolved.tag.clone()
            } else {
                let (tag, pruned) = build_image(ctx, &resolved, msgs, uid, gid)?;
                if let Some(p) = pruned {
                    let _ = msgs.send(Msg::Status(format!("built {tag} ({p})")));
                }
                tag
            };
            let _ = msgs.send(Msg::Status(format!("creating {}…", ctx.container)));
            container::run_detached(&container::RunSpec {
                name: ctx.container.clone(),
                workspace: ctx.workspace.clone(),
                image: run_tag,
                cpus: ctx.cpus,
                memory: ctx.memory.clone(),
                uid,
                gid,
            })?;
        }
    }
    Ok(())
}
