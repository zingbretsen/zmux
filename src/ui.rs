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
    // For text input modes, take over the full tab bar
    if matches!(app.mode, Mode::Rename) {
        let line = Line::from(vec![
            Span::styled(" rename: ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(format!("{}_", app.rename_buf), Style::default().fg(Color::White)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if matches!(app.mode, Mode::BranchInput) {
        let line = Line::from(vec![
            Span::styled(" branch: ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(format!("{}_", app.rename_buf), Style::default().fg(Color::White)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

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

    // Windows - with horizontal scrolling to keep active tab visible
    let prefix_width: usize = spans.iter().map(|s| s.content.len()).sum();
    let suffix_width = if nav { 6 } else { 0 }; // " [NAV]"
    let avail_width = (area.width as usize).saturating_sub(prefix_width + suffix_width);

    // Calculate the character width of each window tab (including separator)
    let tab_widths: Vec<usize> = app.windows.iter().enumerate().map(|(i, entry)| {
        entry.name.len() + if i > 0 { 3 } else { 0 } // " | " separator
    }).collect();

    // Find the range of tabs to display, ensuring active tab is visible
    let (start, end) = visible_tab_range(&tab_widths, active_win_idx, avail_width);

    if start > 0 {
        spans.push(Span::styled("< ", Style::default().fg(Color::DarkGray)));
    }
    for i in start..end {
        if i > start {
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
        spans.push(Span::styled(app.windows[i].name.clone(), style));
        // AI status indicator
        if let Some(ref ai) = app.windows[i].ai_status {
            let (symbol, color) = match ai {
                crate::ai_detect::AiStatus::Running { .. } => ("●", Color::Green),
                crate::ai_detect::AiStatus::Idle { .. } => ("◐", Color::Yellow),
                crate::ai_detect::AiStatus::Finished { .. } => ("○", Color::DarkGray),
            };
            spans.push(Span::styled(symbol, Style::default().fg(color)));
        }
    }
    if end < app.windows.len() {
        spans.push(Span::styled(" >", Style::default().fg(Color::DarkGray)));
    }

    if nav {
        spans.push(Span::styled(" [NAV]", Style::default().fg(Color::Red).bold()));
    }
    if matches!(app.mode, Mode::AiNav) {
        spans.push(Span::styled(" [AI]", Style::default().fg(Color::Green).bold()));
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

/// Given tab widths (each including its separator), find the [start, end) range
/// that fits within `avail` columns and includes `active`.
fn visible_tab_range(widths: &[usize], active: usize, avail: usize) -> (usize, usize) {
    if widths.is_empty() {
        return (0, 0);
    }

    // Width of displaying tabs [start..end) — first tab in range has no separator
    let range_width = |start: usize, end: usize| -> usize {
        let mut w = 0;
        for i in start..end {
            w += if i == start {
                widths[i] - if i > 0 { 3 } else { 0 } // strip separator for first visible
            } else {
                widths[i]
            };
        }
        // Account for scroll indicators
        if start > 0 { w += 2; } // "< "
        if end < widths.len() { w += 2; } // " >"
        w
    };

    // Start with active tab, then expand outward
    let mut start = active;
    let mut end = active + 1;

    if range_width(start, end) > avail {
        return (start, end); // active tab alone doesn't fit, show it anyway
    }

    loop {
        let prev_start = start;
        let prev_end = end;
        // Try expanding right
        if end < widths.len() && range_width(start, end + 1) <= avail {
            end += 1;
        }
        // Try expanding left
        if start > 0 && range_width(start - 1, end) <= avail {
            start -= 1;
        }
        if start == prev_start && end == prev_end {
            break;
        }
    }

    (start, end)
}
