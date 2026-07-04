mod app;
mod config;
mod container;
mod filer;
mod spawn;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Claude in an apple/container — per-project sandboxed dev containers.
#[derive(Parser)]
#[command(name = "pall8t", version)]
struct Cli {
    /// Project directory to add and select (e.g. `pall8t .`)
    path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut app = app::App::new(cli.path)?;

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
) -> Result<()> {
    let tick = Duration::from_secs(2);
    let mut last_tick = Instant::now();
    app.request_refresh();
    loop {
        app.drain_worker();
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code, key.modifiers);
                }
            }
        }
        if last_tick.elapsed() >= tick {
            app.request_refresh();
            last_tick = Instant::now();
        }
        if app.should_quit {
            return Ok(());
        }
    }
}
