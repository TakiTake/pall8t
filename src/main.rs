mod app;
mod config;
mod container;
mod detect;
mod mux;
mod ui;
mod workspace;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Run AI coding agents in apple/container sandboxes — a minimal agent
/// multiplexer TUI.
#[derive(Parser)]
#[command(name = "pall8t", version)]
struct Cli {
    /// Repo directory to add as a single-repo project and open (e.g. `pall8t .`)
    path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut app = app::App::new(cli.path)?;

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        original_hook(info);
    }));

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
) -> Result<()> {
    loop {
        app.tick();
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            handle_event(app, event::read()?);
            // Drain any burst (fast typing, paste) before redrawing.
            while event::poll(Duration::from_millis(0))? {
                handle_event(app, event::read()?);
            }
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_event(app: &mut app::App, ev: Event) {
    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            app.on_key(key.code, key.modifiers);
        }
        Event::Paste(text) => app.on_paste(&text),
        _ => {}
    }
}
