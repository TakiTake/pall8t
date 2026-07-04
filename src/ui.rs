use crate::app::{App, Focus, Mode};
use crate::container::State;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(30)])
        .split(outer[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(main[1]);

    draw_projects(f, app, main[0]);
    draw_files(f, app, right[0]);
    draw_preview(f, app, right[1]);
    draw_status(f, app, outer[1]);
    draw_keybar(f, app, outer[2]);

    match &app.mode {
        Mode::AddProject(input) => draw_input_overlay(f, "add project (path)", input),
        Mode::ConfirmDelete => draw_confirm_overlay(f, app),
        Mode::Logs {
            title,
            lines,
            scroll,
        } => draw_logs_overlay(f, title, lines, *scroll),
        _ => {}
    }
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    }
}

fn state_style(state: State) -> Style {
    match state {
        State::Running => Style::default().fg(Color::Green),
        State::Stopped => Style::default().fg(Color::Gray),
        State::Absent => Style::default().fg(DIM),
    }
}

fn draw_projects(f: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let label = row.state.label();
            let name_width = width.saturating_sub(label.len() + 1).max(4);
            let name: String = row.entry.name.chars().take(name_width).collect();
            let pad = " ".repeat(name_width.saturating_sub(name.chars().count()) + 1);
            ListItem::new(Line::from(vec![
                Span::raw(name),
                Span::raw(pad),
                Span::styled(label, state_style(row.state)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" PROJECTS ")
                .border_style(pane_border(app.focus == Focus::Projects)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let title = match &app.tree {
        Some(tree) => format!(" FILES  {} ", tilde(&tree.root.to_string_lossy())),
        None => " FILES ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(pane_border(app.focus == Focus::Files));

    match &app.tree {
        Some(tree) => {
            let items: Vec<ListItem> = tree
                .visible
                .iter()
                .map(|node| {
                    let indent = "  ".repeat(node.depth);
                    let (arrow, style) = if node.is_dir {
                        (
                            if node.expanded { "▾ " } else { "▸ " },
                            Style::default().fg(Color::Blue),
                        )
                    } else {
                        ("", Style::default())
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(format!("{arrow}{}", node.name), style),
                    ]))
                })
                .collect();
            let mut state = ListState::default();
            if !tree.visible.is_empty() {
                state.select(Some(tree.selected));
            }
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, area, &mut state);
            if matches!(app.mode, Mode::Filter(_)) {
                let hint = format!(" filter: {}▏", tree.filter);
                let hint_area = Rect {
                    x: area.x + 1,
                    y: area.y + area.height.saturating_sub(1),
                    width: (hint.chars().count() as u16 + 1).min(area.width.saturating_sub(2)),
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(hint).style(Style::default().fg(Color::Yellow)),
                    hint_area,
                );
            }
        }
        None => {
            f.render_widget(
                Paragraph::new("no project selected — press `a` to add one")
                    .style(Style::default().fg(DIM))
                    .block(block),
                area,
            );
        }
    }
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.preview_title.is_empty() {
        " PREVIEW ".to_string()
    } else {
        format!(" PREVIEW  {} ", app.preview_title)
    };
    let text: Vec<Line> = app
        .preview
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(DIM)),
        ),
        area,
    );
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let line = if !app.status.is_empty() {
        let style = if app.status.starts_with("error") {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Yellow)
        };
        Line::from(vec![
            Span::raw(" "),
            Span::styled(app.status.clone(), style),
        ])
    } else if let Some(row) = app.rows.get(app.selected) {
        Line::from(vec![
            Span::raw(format!(" {}: ", row.entry.name)),
            Span::styled(row.state.label(), state_style(row.state)),
            Span::styled(
                format!(
                    "  ({})  cpu {}  mem {}",
                    row.container, app.config.cpus, app.config.memory
                ),
                Style::default().fg(DIM),
            ),
        ])
    } else {
        Line::from(Span::styled(
            " no projects — press `a` to add one",
            Style::default().fg(DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keybar(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match app.mode {
        Mode::AddProject(_) => &[("⏎", "confirm"), ("esc", "cancel")],
        Mode::ConfirmDelete => &[("y", "remove + delete container"), ("n", "remove entry only"), ("esc", "cancel")],
        Mode::Logs { .. } => &[("j/k", "scroll"), ("g/G", "top/bottom"), ("q/esc", "close")],
        Mode::Filter(_) => &[("⏎", "apply"), ("esc", "clear")],
        Mode::Normal => &[
            ("⏎", "shell tab"),
            ("c", "claude tab"),
            ("s", "start/stop"),
            ("b", "build"),
            ("L", "logs"),
            ("a", "add"),
            ("d", "remove"),
            ("Tab", "focus"),
            ("q", "quit"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}   "),
            Style::default().fg(DIM),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input_overlay(f: &mut Frame, title: &str, input: &str) {
    let area = centered_rect(f.area(), 60, 3);
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

fn draw_confirm_overlay(f: &mut Frame, app: &App) {
    let name = app
        .rows
        .get(app.selected)
        .map(|r| r.entry.name.clone())
        .unwrap_or_default();
    let area = centered_rect(f.area(), 64, 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(vec![
            Line::raw(format!("remove `{name}` from pall8t?")),
            Line::styled(
                "y: also delete its container   n: keep container   esc: cancel",
                Style::default().fg(DIM),
            ),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" remove project ")
                .border_style(Style::default().fg(Color::Red)),
        ),
        area,
    );
}

fn draw_logs_overlay(f: &mut Frame, title: &str, lines: &[String], scroll: usize) {
    let full = f.area();
    let width = (full.width as u32 * 85 / 100) as u16;
    let height = (full.height as u32 * 80 / 100) as u16;
    let area = centered_rect(full, width, height);
    f.render_widget(Clear, area);
    let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.as_str())).collect();
    f.render_widget(
        Paragraph::new(text)
            .scroll((scroll as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" logs: {title} "))
                    .border_style(Style::default().fg(ACCENT)),
            ),
        area,
    );
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

fn tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}
