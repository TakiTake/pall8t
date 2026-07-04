use crate::{config, container, filer, spawn};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Projects,
    Files,
}

pub enum Mode {
    Normal,
    AddProject(String),
    ConfirmDelete,
    Filter(String),
    Logs {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
}

pub struct ProjectRow {
    pub entry: config::ProjectEntry,
    pub state: container::State,
    pub container: String,
}

/// Everything the worker needs to build/run a project container.
#[derive(Clone)]
pub struct SpawnCtx {
    pub name: String,
    pub path: PathBuf,
    pub image_override: Option<String>,
    pub image_base: String,
    pub containerfile: Option<PathBuf>,
    pub cpus: u32,
    pub memory: String,
}

pub enum Job {
    Refresh,
    EnsureAndSpawn { ctx: SpawnCtx, claude: bool },
    Build(SpawnCtx),
    Toggle { state: container::State, ctx: SpawnCtx },
    DeleteContainer(String),
    Logs(String),
}

pub enum Msg {
    Containers(Vec<(String, container::State)>),
    Status(String),
    Done(String),
    Error(String),
    Logs { name: String, text: String },
}

pub struct App {
    pub config: config::Config,
    pub rows: Vec<ProjectRow>,
    pub selected: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub tree: Option<filer::FileTree>,
    pub preview: Vec<String>,
    pub preview_title: String,
    pub status: String,
    pub busy: bool,
    pub should_quit: bool,
    jobs: Sender<Job>,
    msgs: Receiver<Msg>,
}

impl App {
    pub fn new(path_arg: Option<PathBuf>) -> Result<Self> {
        let mut cfg = config::load()?;
        let mut select = 0usize;
        if let Some(p) = path_arg {
            let abs = std::fs::canonicalize(&p)
                .with_context(|| format!("cannot resolve path: {}", p.display()))?;
            if let Some(i) = cfg.projects.iter().position(|e| e.path == abs) {
                select = i;
            } else {
                let name = abs
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project".to_string());
                cfg.projects.push(config::ProjectEntry {
                    name,
                    path: abs,
                    image: None,
                    containerfile: None,
                });
                config::save(&cfg)?;
                select = cfg.projects.len() - 1;
            }
        }

        let (uid, gid) = container::host_ids();
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        thread::spawn(move || worker(job_rx, msg_tx, uid, gid));

        let rows: Vec<ProjectRow> = cfg
            .projects
            .iter()
            .map(|e| ProjectRow {
                container: container::container_name(&e.path),
                state: container::State::Absent,
                entry: e.clone(),
            })
            .collect();

        let mut app = Self {
            config: cfg,
            rows,
            selected: select,
            focus: Focus::Projects,
            mode: Mode::Normal,
            tree: None,
            preview: Vec::new(),
            preview_title: String::new(),
            status: "reconciling with `container list`…".to_string(),
            busy: false,
            should_quit: false,
            jobs: job_tx,
            msgs: msg_rx,
        };
        app.load_tree();
        Ok(app)
    }

    pub fn request_refresh(&self) {
        let _ = self.jobs.send(Job::Refresh);
    }

    pub fn drain_worker(&mut self) {
        while let Ok(msg) = self.msgs.try_recv() {
            match msg {
                Msg::Containers(list) => {
                    for row in &mut self.rows {
                        row.state = list
                            .iter()
                            .find(|(name, _)| *name == row.container)
                            .map(|(_, s)| *s)
                            .unwrap_or(container::State::Absent);
                    }
                    if self.status.starts_with("reconciling") {
                        self.status.clear();
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
                Msg::Error(e) => {
                    self.status = format!("error: {e}");
                    self.busy = false;
                }
                Msg::Logs { name, text } => {
                    self.mode = Mode::Logs {
                        title: name,
                        lines: text.lines().map(|s| s.to_string()).collect(),
                        scroll: 0,
                    };
                    self.busy = false;
                }
            }
        }
    }

    fn selected_row(&self) -> Option<&ProjectRow> {
        self.rows.get(self.selected)
    }

    fn spawn_ctx(&self) -> Option<SpawnCtx> {
        self.selected_row().map(|row| SpawnCtx {
            name: row.container.clone(),
            path: row.entry.path.clone(),
            image_override: row.entry.image.clone(),
            image_base: self.config.default_image.clone(),
            containerfile: row.entry.containerfile.clone(),
            cpus: self.config.cpus,
            memory: self.config.memory.clone(),
        })
    }

    fn load_tree(&mut self) {
        self.preview.clear();
        self.preview_title.clear();
        self.tree = self
            .selected_row()
            .filter(|r| r.entry.path.is_dir())
            .map(|r| filer::FileTree::new(r.entry.path.clone()));
    }

    fn update_preview(&mut self) {
        let node = self
            .tree
            .as_ref()
            .and_then(|t| t.selected_node())
            .cloned();
        match node {
            Some(n) if !n.is_dir => {
                self.preview = filer::preview(&n.path);
                self.preview_title = n.name;
            }
            _ => {
                self.preview.clear();
                self.preview_title.clear();
            }
        }
    }

    fn select_project(&mut self, index: usize) {
        if index < self.rows.len() && index != self.selected {
            self.selected = index;
            self.load_tree();
        }
    }

    fn add_project(&mut self, input: &str) {
        let raw = input.trim();
        if raw.is_empty() {
            return;
        }
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            match dirs::home_dir() {
                Some(h) => h.join(rest),
                None => PathBuf::from(raw),
            }
        } else {
            PathBuf::from(raw)
        };
        let abs = match std::fs::canonicalize(&expanded) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("cannot add {}: {e}", expanded.display());
                return;
            }
        };
        if self.config.projects.iter().any(|e| e.path == abs) {
            self.status = "project already exists".to_string();
            return;
        }
        let name = abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
        let entry = config::ProjectEntry {
            name,
            path: abs,
            image: None,
            containerfile: None,
        };
        self.config.projects.push(entry.clone());
        if let Err(e) = config::save(&self.config) {
            self.status = format!("config save failed: {e}");
        }
        self.rows.push(ProjectRow {
            container: container::container_name(&entry.path),
            state: container::State::Absent,
            entry,
        });
        self.selected = self.rows.len() - 1;
        self.load_tree();
        self.request_refresh();
    }

    fn remove_project(&mut self, delete_container: bool) {
        if self.selected >= self.rows.len() {
            return;
        }
        let row = self.rows.remove(self.selected);
        self.config.projects.retain(|e| e.path != row.entry.path);
        if let Err(e) = config::save(&self.config) {
            self.status = format!("config save failed: {e}");
        } else {
            self.status = format!("removed {}", row.entry.name);
        }
        if delete_container && row.state != container::State::Absent {
            let _ = self.jobs.send(Job::DeleteContainer(row.container));
            self.busy = true;
        }
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        self.load_tree();
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
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
            Mode::ConfirmDelete => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.remove_project(true),
                KeyCode::Char('n') | KeyCode::Char('N') => self.remove_project(false),
                _ => {}
            },
            Mode::Filter(mut input) => match code {
                KeyCode::Enter => {}
                KeyCode::Esc => {
                    if let Some(tree) = self.tree.as_mut() {
                        tree.filter.clear();
                        tree.rebuild();
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.apply_filter(&input);
                    self.mode = Mode::Filter(input);
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.apply_filter(&input);
                    self.mode = Mode::Filter(input);
                }
                _ => self.mode = Mode::Filter(input),
            },
            Mode::Logs {
                title,
                lines,
                scroll,
            } => match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => {}
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
            Mode::Normal => self.on_key_normal(code, mods),
        }
    }

    fn apply_filter(&mut self, input: &str) {
        if let Some(tree) = self.tree.as_mut() {
            tree.filter = input.to_string();
            tree.rebuild();
        }
        self.update_preview();
    }

    fn on_key_normal(&mut self, code: KeyCode, _mods: KeyModifiers) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Projects => Focus::Files,
                    Focus::Files => Focus::Projects,
                };
            }
            KeyCode::Char('r') => {
                self.status = "refreshing…".to_string();
                self.request_refresh();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => match self.focus {
                Focus::Projects => self.select_project(0),
                Focus::Files => {
                    if let Some(t) = self.tree.as_mut() {
                        t.top();
                    }
                    self.update_preview();
                }
            },
            KeyCode::Char('G') => match self.focus {
                Focus::Projects => self.select_project(self.rows.len().saturating_sub(1)),
                Focus::Files => {
                    if let Some(t) = self.tree.as_mut() {
                        t.bottom();
                    }
                    self.update_preview();
                }
            },
            KeyCode::Char('h') | KeyCode::Left => {
                if self.focus == Focus::Files {
                    if let Some(t) = self.tree.as_mut() {
                        t.collapse();
                    }
                    self.update_preview();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if self.focus == Focus::Files {
                    if let Some(t) = self.tree.as_mut() {
                        t.expand();
                    }
                    self.update_preview();
                }
            }
            KeyCode::Enter => match self.focus {
                Focus::Projects => self.spawn_tab(false),
                Focus::Files => {
                    if let Some(t) = self.tree.as_mut() {
                        t.toggle();
                    }
                    self.update_preview();
                }
            },
            KeyCode::Char('c') => self.spawn_tab(true),
            KeyCode::Char('s') => {
                let state = self.selected_row().map(|r| r.state);
                if let (Some(state), Some(ctx)) = (state, self.spawn_ctx()) {
                    self.status = "…".to_string();
                    self.busy = true;
                    let _ = self.jobs.send(Job::Toggle { state, ctx });
                }
            }
            KeyCode::Char('b') => {
                if let Some(ctx) = self.spawn_ctx() {
                    self.busy = true;
                    let _ = self.jobs.send(Job::Build(ctx));
                }
            }
            KeyCode::Char('L') => {
                let info = self.selected_row().map(|r| (r.state, r.container.clone()));
                if let Some((state, name)) = info {
                    if state == container::State::Absent {
                        self.status = "no container yet".to_string();
                    } else {
                        self.busy = true;
                        let _ = self.jobs.send(Job::Logs(name));
                    }
                }
            }
            KeyCode::Char('a') => self.mode = Mode::AddProject(String::new()),
            KeyCode::Char('d') => {
                if !self.rows.is_empty() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('/') => {
                if self.focus == Focus::Files && self.tree.is_some() {
                    self.mode = Mode::Filter(String::new());
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i64) {
        match self.focus {
            Focus::Projects => {
                if self.rows.is_empty() {
                    return;
                }
                let len = self.rows.len() as i64;
                let next = (self.selected as i64 + delta).clamp(0, len - 1) as usize;
                self.select_project(next);
            }
            Focus::Files => {
                if let Some(t) = self.tree.as_mut() {
                    t.move_by(delta);
                }
                self.update_preview();
            }
        }
    }

    fn spawn_tab(&mut self, claude: bool) {
        if let Some(ctx) = self.spawn_ctx() {
            self.status = "preparing container…".to_string();
            self.busy = true;
            let _ = self.jobs.send(Job::EnsureAndSpawn { ctx, claude });
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
        Job::Refresh => {
            let list = container::list_all()?;
            let _ = msgs.send(Msg::Containers(list));
        }
        Job::EnsureAndSpawn { ctx, claude } => {
            ensure_running(&ctx, msgs, uid, gid)?;
            let cmd = container::exec_shell_command(&ctx.name, claude);
            let result = spawn::spawn_tab(&cmd)?;
            refresh(msgs);
            let _ = msgs.send(Msg::Done(result));
        }
        Job::Build(ctx) => {
            let tag = build_image(&ctx, msgs, uid, gid)?;
            let _ = msgs.send(Msg::Done(format!("image built: {tag}")));
        }
        Job::Toggle { state, ctx } => {
            match state {
                container::State::Running => {
                    let _ = msgs.send(Msg::Status(format!("stopping {}…", ctx.name)));
                    container::stop(&ctx.name)?;
                }
                container::State::Stopped => {
                    let _ = msgs.send(Msg::Status(format!("starting {}…", ctx.name)));
                    container::start(&ctx.name)?;
                }
                container::State::Absent => {
                    ensure_running(&ctx, msgs, uid, gid)?;
                }
            }
            refresh(msgs);
            let _ = msgs.send(Msg::Done("done".to_string()));
        }
        Job::DeleteContainer(name) => {
            let _ = msgs.send(Msg::Status(format!("deleting {name}…")));
            let _ = container::stop(&name);
            container::delete(&name)?;
            refresh(msgs);
            let _ = msgs.send(Msg::Done(format!("deleted {name}")));
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

fn resolve_image(ctx: &SpawnCtx, uid: u32, gid: u32) -> String {
    ctx.image_override
        .clone()
        .unwrap_or_else(|| container::image_tag(&ctx.image_base, uid, gid))
}

fn build_image(ctx: &SpawnCtx, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<String> {
    let tag = resolve_image(ctx, uid, gid);
    let containerfile = match &ctx.containerfile {
        Some(cf) => {
            if cf.is_absolute() {
                cf.clone()
            } else {
                ctx.path.join(cf)
            }
        }
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

/// Absent → build image if missing → run; Stopped → start; Running → no-op.
fn ensure_running(ctx: &SpawnCtx, msgs: &Sender<Msg>, uid: u32, gid: u32) -> Result<()> {
    let list = container::list_all()?;
    let state = list
        .iter()
        .find(|(name, _)| *name == ctx.name)
        .map(|(_, s)| *s)
        .unwrap_or(container::State::Absent);
    match state {
        container::State::Running => {}
        container::State::Stopped => {
            let _ = msgs.send(Msg::Status(format!("starting {}…", ctx.name)));
            container::start(&ctx.name)?;
        }
        container::State::Absent => {
            let tag = resolve_image(ctx, uid, gid);
            if !container::image_exists(&tag) {
                build_image(ctx, msgs, uid, gid)?;
            }
            let _ = msgs.send(Msg::Status(format!("creating {}…", ctx.name)));
            container::run_detached(&container::RunSpec {
                name: ctx.name.clone(),
                project: ctx.path.clone(),
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
