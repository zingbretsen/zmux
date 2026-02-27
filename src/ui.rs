use crate::app::{App, Mode, TabLevel};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    draw_tab_bar(f, app, chunks[0]);

    let parser = app.parser.lock().unwrap();
    let pseudo_term = PseudoTerminal::new(parser.screen());
    f.render_widget(pseudo_term, chunks[1]);
}

fn draw_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let nav = matches!(app.mode, Mode::Nav);

    let active_proj_idx = app.active_project
        .and_then(|id| app.projects.iter().position(|e| e.id == id))
        .unwrap_or(0);
    let active_grp_idx = app.active_group
        .and_then(|id| app.groups.iter().position(|e| e.id == id))
        .unwrap_or(0);
    let active_win_idx = app.active_window
        .and_then(|id| app.windows.iter().position(|e| e.id == id))
        .unwrap_or(0);

    let mut spans: Vec<Span> = Vec::new();

    // Project
    let proj_name = app.projects.get(active_proj_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let proj_style = if nav && app.tab_focus == TabLevel::Project {
        Style::default().fg(Color::Black).bg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::Cyan)
    };
    spans.push(Span::styled(format!(" {}", proj_name), proj_style));
    spans.push(Span::styled(" > ", Style::default().fg(Color::DarkGray)));

    // Group
    let grp_name = app.groups.get(active_grp_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let grp_style = if nav && app.tab_focus == TabLevel::Group {
        Style::default().fg(Color::Black).bg(Color::Green).bold()
    } else {
        Style::default().fg(Color::Green)
    };
    spans.push(Span::styled(grp_name.to_string(), grp_style));
    spans.push(Span::styled(" > ", Style::default().fg(Color::DarkGray)));

    // Windows
    for (i, entry) in app.windows.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        let style = if i == active_win_idx {
            if nav && app.tab_focus == TabLevel::Window {
                Style::default().fg(Color::Black).bg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::Yellow).bold()
            }
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(entry.name.clone(), style));
    }

    if nav {
        spans.push(Span::styled(" [NAV]", Style::default().fg(Color::Red).bold()));
    }

    // Status message (shown for 3 seconds)
    if let Some((ref msg, ref when)) = app.status_message {
        if when.elapsed() < Duration::from_secs(3) {
            spans.push(Span::styled(
                format!("  {}", msg),
                Style::default().fg(Color::Magenta).italic(),
            ));
        }
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
