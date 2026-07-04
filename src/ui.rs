use crate::app::{App, Mode};
use crate::container::State;
use crate::detect::TabState;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const ALERT: Color = Color::Red;

pub fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let (sidebar_area, term_area) = if app.sidebar {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(20)])
            .split(outer[1]);
        (Some(cols[0]), cols[1])
    } else {
        (None, outer[1])
    };

    app.set_term_size(term_area.height, term_area.width);

    draw_header(f, app, outer[0]);
    if let Some(area) = sidebar_area {
        draw_sidebar(f, app, area);
    }
    draw_terminal(f, app, term_area);
    draw_status(f, app, outer[2]);
    draw_keybar(f, app, outer[3]);

    match &app.mode {
        Mode::Help => draw_help(f, app),
        Mode::AddProject(input) => draw_input_overlay(f, "add project (repo paths, comma-separated)", input),
        Mode::ConfirmClose => draw_confirm(f, "close tab?", "the process inside is still running — y: close  esc: cancel"),
        Mode::ConfirmQuit => draw_confirm(f, "quit pall8t?", "tabs are working/waiting; exec sessions will end (containers keep running) — y: quit  esc: cancel"),
        Mode::Logs { title, lines, scroll } => draw_logs(f, title, lines, *scroll),
        _ => {}
    }
}

fn state_color(state: TabState) -> Color {
    match state {
        TabState::Working => Color::Yellow,
        TabState::Waiting => ALERT,
        TabState::Idle => DIM,
        TabState::Done => Color::Blue,
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let line = match app.active_tab.and_then(|i| app.tabs.get(i)) {
        Some(tab) => {
            let project = app
                .projects
                .get(tab.project)
                .map(|r| r.entry.name.clone())
                .unwrap_or_default();
            let container = app
                .projects
                .get(tab.project)
                .map(|r| r.container.clone())
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(
                    format!(" {project} / {} ", tab.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(tab.state.label(), Style::default().fg(state_color(tab.state))),
                Span::styled(format!("  ({container})"), Style::default().fg(DIM)),
            ])
        }
        None => Line::from(Span::styled(
            " pall8t — no tabs yet",
            Style::default().fg(DIM),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " PROJECTS / TABS",
        Style::default().fg(DIM),
    )));
    for (pi, row) in app.projects.iter().enumerate() {
        let marker = if pi == app.current_project { "▸" } else { " " };
        let cstate = match row.state {
            State::Running => Span::styled("●", Style::default().fg(Color::Green)),
            State::Stopped => Span::styled("○", Style::default().fg(DIM)),
            State::Absent => Span::styled("·", Style::default().fg(DIM)),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{marker} {} ", row.entry.name)),
            cstate,
        ]));
        let mut n = 0usize;
        for (ti, tab) in app.tabs.iter().enumerate() {
            if tab.project != pi {
                continue;
            }
            n += 1;
            let active = app.active_tab == Some(ti);
            let style = if active {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Gray)
            };
            let label = format!("   {n} {}", tab.title);
            let width = area.width.saturating_sub(2) as usize;
            let state = tab.state.label();
            let pad = width
                .saturating_sub(label.width() + state.width() + 1)
                .max(1);
            lines.push(Line::from(vec![
                Span::styled(label, style),
                Span::raw(" ".repeat(pad)),
                Span::styled(state, Style::default().fg(state_color(tab.state))),
            ]));
        }
    }
    if app.projects.is_empty() {
        lines.push(Line::from(Span::styled(
            " (press P to add a project)",
            Style::default().fg(DIM),
        )));
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(DIM));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_terminal(f: &mut Frame, app: &App, area: Rect) {
    match app.active_tab.and_then(|i| app.tabs.get(i)) {
        Some(tab) => {
            let cursor = {
                let buf = f.buffer_mut();
                tab.with_screen(|screen| {
                    render_screen(screen, area, buf);
                    (screen.cursor_position(), screen.hide_cursor())
                })
            };
            if matches!(app.mode, Mode::Normal) {
                if let Some(((row, col), hidden)) = cursor {
                    if !hidden && row < area.height && col < area.width {
                        f.set_cursor_position(Position::new(area.x + col, area.y + row));
                    }
                }
            }
        }
        None => {
            let prefix = app.prefix_char;
            let text = vec![
                Line::raw(""),
                Line::from(Span::styled(
                    format!("  ^{prefix} a — new agent tab    ^{prefix} c — new shell tab"),
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    format!("  ^{prefix} ? — all keys"),
                    Style::default().fg(DIM),
                )),
            ];
            f.render_widget(Paragraph::new(text), area);
        }
    }
}

fn conv_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    for row in 0..rows.min(area.height) {
        let mut skip = false;
        for col in 0..cols.min(area.width) {
            if skip {
                skip = false;
                continue;
            }
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let contents = cell.contents();
            let symbol: &str = if contents.is_empty() { " " } else { &contents };
            let mut style = Style::default();
            if let Some(fg) = conv_color(cell.fgcolor()) {
                style = style.fg(fg);
            }
            if let Some(bg) = conv_color(cell.bgcolor()) {
                style = style.bg(bg);
            }
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if let Some(target) = buf.cell_mut(Position::new(area.x + col, area.y + row)) {
                target.set_symbol(symbol);
                target.set_style(style);
            }
            if symbol.width() > 1 {
                skip = true;
            }
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let waiting = app.waiting_tabs();
    let line = if !waiting.is_empty() {
        let first = waiting[0];
        let name = app
            .tabs
            .get(first)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        Line::from(Span::styled(
            format!(
                " ⚠ {} tab(s) waiting for you ({name}) — press ^{} n to jump",
                waiting.len(),
                app.prefix_char
            ),
            Style::default().fg(ALERT),
        ))
    } else if !app.status.is_empty() {
        let style = if app.status.starts_with("error") {
            Style::default().fg(ALERT)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let spinner = if app.busy { "⋯ " } else { "" };
        Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{spinner}{}", app.status), style),
        ])
    } else if let Some(row) = app.projects.get(app.current_project) {
        Line::from(vec![
            Span::raw(format!(" {} ", row.entry.name)),
            Span::styled(row.state.label(), Style::default().fg(DIM)),
            Span::styled(
                format!("  {} repo(s)  {}", row.entry.repos.len(), row.workspace.display()),
                Style::default().fg(DIM),
            ),
        ])
    } else {
        Line::from(Span::styled(
            " no projects — press P to add one",
            Style::default().fg(DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keybar(f: &mut Frame, app: &App, area: Rect) {
    let prefix = format!("^{}", app.prefix_char);
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    if matches!(app.mode, Mode::Prefix) {
        spans.push(Span::styled(
            format!("{prefix} …"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    } else {
        spans.push(Span::styled(prefix, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
        spans.push(Span::raw("  "));
    }
    let keys: &[(&str, &str)] = &[
        ("a", "agent"),
        ("c", "shell"),
        ("n", "next waiting"),
        ("j/k 1-9", "tabs"),
        ("p/P", "project"),
        ("x", "close"),
        ("z", "sidebar"),
        ("?", "help"),
        ("q", "quit"),
    ];
    for (key, label) in keys {
        let key_color = if *key == "n" { ALERT } else { ACCENT };
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(key_color),
        ));
        spans.push(Span::styled(
            format!(" {label}  "),
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered_rect(full: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(full.width.saturating_sub(2));
    let h = height.min(full.height.saturating_sub(2));
    Rect {
        x: full.x + (full.width.saturating_sub(w)) / 2,
        y: full.y + (full.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_input_overlay(f: &mut Frame, title: &str, input: &str) {
    let area = centered_rect(f.area(), 64, 3);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(format!("{input}▏")).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "))
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

fn draw_confirm(f: &mut Frame, title: &str, body: &str) {
    let area = centered_rect(f.area(), 70, 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(vec![Line::raw(body.to_string())])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} "))
                    .border_style(Style::default().fg(ALERT)),
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
        area,
    );
}

fn draw_help(f: &mut Frame, app: &App) {
    let p = app.prefix_char;
    let area = centered_rect(f.area(), 56, 16);
    f.render_widget(Clear, area);
    let rows = [
        ("a", "new agent tab (current project)"),
        ("c", "new shell tab"),
        ("n", "jump to next waiting tab"),
        ("j / k", "next / previous tab"),
        ("1-9", "jump to tab N"),
        ("p", "cycle project"),
        ("P", "add project"),
        ("x", "close tab"),
        ("s", "start/stop container"),
        ("b", "rebuild image"),
        ("L", "container logs"),
        ("z", "toggle sidebar"),
        ("q", "quit"),
    ];
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("press ctrl+{p}, release, then:"),
        Style::default().fg(DIM),
    ))];
    for (key, desc) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<7}"), Style::default().fg(ACCENT)),
            Span::raw(desc),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  (any key closes this help)",
        Style::default().fg(DIM),
    )));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" pall8t keys ")
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

fn draw_logs(f: &mut Frame, title: &str, lines: &[String], scroll: usize) {
    let full = f.area();
    let width = (full.width as u32 * 85 / 100) as u16;
    let height = (full.height as u32 * 80 / 100) as u16;
    let area = centered_rect(full, width, height);
    f.render_widget(Clear, area);
    let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.as_str())).collect();
    f.render_widget(
        Paragraph::new(text).scroll((scroll as u16, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" logs: {title} — j/k scroll, q close "))
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}
