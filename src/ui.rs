use crate::app::{App, Mode, TabLevel, TreeItem};
use crate::protocol::{LayoutMode, SplitDir, SplitTree};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

pub enum TabClick {
    Project,
    Group,
    Window(usize),
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    // Outer border to indicate we're inside zmux
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title_bottom(Line::from(" zmux ").right_aligned());
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    draw_tab_bar(f, app, chunks[0]);

    if app.is_tiled() {
        draw_tiled(f, app, chunks[1]);
    } else {
        // Stacked mode: show single active window
        if let Some(wid) = app.active_window {
            if let Some(parser) = app.parser_for(wid) {
                let parser = parser.lock().unwrap();
                let pseudo_term = PseudoTerminal::new(parser.screen());
                f.render_widget(pseudo_term, chunks[1]);
            }
        }
    }

    // Copy mode overlay: cursor and selection
    if matches!(app.mode, Mode::Copy) {
        draw_copy_overlay(f, app, chunks[1]);
    }

    if matches!(app.mode, Mode::Help) {
        draw_help(f, area);
    }
    if matches!(app.mode, Mode::BranchInput) {
        draw_branch_picker(f, app, area);
    }
    if matches!(app.mode, Mode::PresetInput) {
        draw_preset_picker(f, app, area);
    }
    if matches!(app.mode, Mode::TreeNav) {
        draw_tree_nav(f, app, area);
    }
    if matches!(app.mode, Mode::ProjectPicker | Mode::GroupPicker) {
        draw_picker_dropdown(f, app, area);
    }
    if matches!(app.mode, Mode::ConfirmOverwrite) {
        draw_confirm_overwrite(f, app, area);
    }
}

fn draw_copy_overlay(f: &mut Frame, app: &App, content_area: Rect) {
    let buf = f.buffer_mut();
    let screen_rows = app.term_rows;
    let screen_cols = app.term_cols;

    // If selecting, highlight the selection range
    if app.copy_selecting {
        let cur_abs = app.copy_scroll_offset + (screen_rows - 1 - app.copy_cursor_row) as usize;
        let sel_start = app.copy_sel_start;
        let sel_end = (cur_abs, app.copy_cursor_col);

        // Normalize: from = higher abs (older), to = lower abs (newer)
        let (from, to) = if sel_start.0 > sel_end.0 || (sel_start.0 == sel_end.0 && sel_start.1 <= sel_end.1) {
            (sel_start, sel_end)
        } else {
            (sel_end, sel_start)
        };

        for row in 0..screen_rows {
            let row_abs = app.copy_scroll_offset + (screen_rows - 1 - row) as usize;
            if row_abs > from.0 || row_abs < to.0 {
                continue;
            }
            let col_start = if row_abs == from.0 { from.1 } else { 0 };
            let col_end = if row_abs == to.0 { to.1 } else { screen_cols - 1 };

            for col in col_start..=col_end {
                let x = content_area.x + col;
                let y = content_area.y + row;
                if x < content_area.right() && y < content_area.bottom() {
                    let cell = &mut buf[(x, y)];
                    cell.set_style(Style::default().bg(Color::DarkGray).fg(Color::White));
                }
            }
        }
    }

    // Draw cursor (inverted)
    let cx = content_area.x + app.copy_cursor_col;
    let cy = content_area.y + app.copy_cursor_row;
    if cx < content_area.right() && cy < content_area.bottom() {
        let cell = &mut buf[(cx, cy)];
        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let help_text = "\
 Ctrl+B  Enter nav mode    Ctrl+Q  Quit

 Nav Mode:
 h/l     Prev/next tab     j/k     Change level
 1-9     Select tab         Esc     Exit nav mode
 c       New window         x       Close window
 g       Window → new group p       Window → new project
 r       Rename             ?       This help
 a       AI nav mode
 s       Set group dir      S       Set project dir
 W       Save preset        L       Load preset
 w       New worktree group X       Close group
 R       Rebase onto main   M       Merge into main
 t       Toggle tiled       v       Vertical split
 -       Horizontal split   T       Swap split direction
 m       Close pane         n/N     Cycle pane (group)
 o/O     Cycle pane (all)
 f       Session tree       d       Detach

 Tree Nav: j/k move, h fold, l expand, Enter select
 Tree Nav: H/L collapse/expand one level, J/K same-level
 Tree Nav: r rename, x close, g/G top/bottom
 AI Nav: h/l to cycle, Esc to exit
 Tiled: Ctrl+h/j/k/l to move focus
 Tiled: Shift+arrows to resize pane

 Press any key to close";

    let lines: Vec<&str> = help_text.lines().collect();
    let height = (lines.len() as u16 + 2).min(area.height);
    let width = 54u16.min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Keybindings ");
    let para = Paragraph::new(help_text)
        .block(block)
        .style(Style::default().fg(Color::White));
    f.render_widget(para, popup);
}

fn draw_branch_picker(f: &mut Frame, app: &App, area: Rect) {
    let filtered = app.filtered_branches();
    if filtered.is_empty() {
        return;
    }

    let max_visible = 10usize;
    let visible_count = filtered.len().min(max_visible);
    let height = (visible_count as u16 + 2).min(area.height); // +2 for borders
    let width = filtered.iter().map(|b| b.len()).max().unwrap_or(10).max(20) as u16 + 4; // padding
    let width = width.min(area.width);

    // Position below the tab bar
    let x = area.x + 2;
    let y = area.y + 2; // below border + tab bar
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Branches ");

    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Scroll offset to keep selection visible
    let scroll_offset = if let Some(sel) = app.branch_selected {
        if sel >= max_visible {
            sel - max_visible + 1
        } else {
            0
        }
    } else {
        0
    };

    let lines: Vec<Line> = filtered
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .enumerate()
        .map(|(i, name)| {
            let actual_idx = i + scroll_offset;
            let is_selected = app.branch_selected == Some(actual_idx);
            let style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(format!(" {} ", name), style))
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

fn draw_preset_picker(f: &mut Frame, app: &App, area: Rect) {
    let filtered = app.filtered_presets();
    if filtered.is_empty() {
        return;
    }

    let max_visible = 10usize;
    let visible_count = filtered.len().min(max_visible);
    let height = (visible_count as u16 + 2).min(area.height);
    let width = filtered.iter().map(|p| p.len()).max().unwrap_or(10).max(20) as u16 + 4;
    let width = width.min(area.width);

    let x = area.x + 2;
    let y = area.y + 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Presets ");

    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let scroll_offset = if let Some(sel) = app.preset_selected {
        if sel >= max_visible {
            sel - max_visible + 1
        } else {
            0
        }
    } else {
        0
    };

    let lines: Vec<Line> = filtered
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .enumerate()
        .map(|(i, name)| {
            let actual_idx = i + scroll_offset;
            let is_selected = app.preset_selected == Some(actual_idx);
            let style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(format!(" {} ", name), style))
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

fn draw_confirm_overwrite(f: &mut Frame, app: &App, area: Rect) {
    let name = match &app.overwrite_preset_name {
        Some(n) => n.as_str(),
        None => return,
    };
    let prompt = format!(" Overwrite preset '{}'? (y/n) ", name);
    let width = (prompt.len() as u16 + 4).min(area.width);
    let height = 3u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let text = Paragraph::new(Line::from(Span::styled(
        prompt,
        Style::default().fg(Color::Yellow).bold(),
    )));
    f.render_widget(text, inner);
}

fn draw_picker_dropdown(f: &mut Frame, app: &App, area: Rect) {
    let (items, color, title, active_id) = match app.mode {
        Mode::ProjectPicker => (&app.projects, Color::Cyan, " Projects ", app.active_project),
        Mode::GroupPicker => (&app.groups, Color::Green, " Groups ", app.active_group),
        _ => return,
    };
    if items.is_empty() {
        return;
    }

    let max_visible = 10usize;
    let visible_count = items.len().min(max_visible);
    let height = (visible_count as u16 + 2).min(area.height);
    let width = items.iter().map(|e| e.name.len()).max().unwrap_or(10).max(12) as u16 + 4;
    let width = width.min(area.width);

    let x = (app.dropdown_x + 1).min(area.width.saturating_sub(width));
    let y = area.y + 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let scroll_offset = if app.dropdown_selected >= max_visible {
        app.dropdown_selected - max_visible + 1
    } else {
        0
    };

    let lines: Vec<Line> = items
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .enumerate()
        .map(|(i, entry)| {
            let actual_idx = i + scroll_offset;
            let is_selected = app.dropdown_selected == actual_idx;
            let is_active = Some(entry.id) == active_id;
            let style = if is_selected {
                Style::default().fg(Color::Black).bg(color).bold()
            } else if is_active {
                Style::default().fg(color).bold()
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(format!(" {} ", entry.name), style))
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

/// Compute the dropdown rect for hit-testing mouse clicks.
pub fn picker_dropdown_rect(app: &App) -> Option<Rect> {
    let items = match app.mode {
        Mode::ProjectPicker => &app.projects,
        Mode::GroupPicker => &app.groups,
        _ => return None,
    };
    if items.is_empty() {
        return None;
    }
    let max_visible = 10usize;
    let visible_count = items.len().min(max_visible);
    let height = visible_count as u16 + 2;
    let width = items.iter().map(|e| e.name.len()).max().unwrap_or(10).max(12) as u16 + 4;
    let (cols, _rows) = app.last_size;
    let width = width.min(cols);
    let x = (app.dropdown_x + 1).min(cols.saturating_sub(width));
    Some(Rect::new(x, 2, width, height))
}

fn draw_tree_nav(f: &mut Frame, app: &App, area: Rect) {
    let items = app.tree_visible_items();
    if items.is_empty() {
        return;
    }

    // Full-screen overlay
    f.render_widget(Clear, area);

    // Split: tree list on left, preview on right
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    let tree_area = halves[0];
    let preview_area = halves[1];

    // Tree list
    let tree_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Session Tree ");
    let tree_inner = tree_block.inner(tree_area);
    f.render_widget(tree_block, tree_area);

    let visible_height = tree_inner.height as usize;
    // Scroll to keep cursor visible
    let scroll_offset = if app.tree_cursor >= visible_height {
        app.tree_cursor - visible_height + 1
    } else {
        0
    };

    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(i, item)| {
            let is_cursor = i == app.tree_cursor;
            match item {
                TreeItem::Project { id, name, expanded } => {
                    let arrow = if *expanded { "▾" } else { "▸" };
                    let is_active = app.tree_active_project == Some(*id);
                    let mut style = Style::default().fg(Color::Cyan);
                    if is_active {
                        style = style.bold();
                    }
                    if is_cursor {
                        style = style.fg(Color::Black).bg(Color::Cyan);
                    }
                    Line::from(Span::styled(format!("{} {}", arrow, name), style))
                }
                TreeItem::Group { id, name, expanded } => {
                    let arrow = if *expanded { "▾" } else { "▸" };
                    let is_active = app.tree_active_group == Some(*id);
                    let mut style = Style::default().fg(Color::Green);
                    if is_active {
                        style = style.bold();
                    }
                    if is_cursor {
                        style = style.fg(Color::Black).bg(Color::Green);
                    }
                    Line::from(Span::styled(format!("  {} {}", arrow, name), style))
                }
                TreeItem::Window { id, name, ai_status } => {
                    let is_active = app.tree_active_window == Some(*id);
                    let mut style = Style::default().fg(Color::White);
                    if is_active {
                        style = style.fg(Color::Yellow).bold();
                    }
                    if is_cursor {
                        style = style.fg(Color::Black).bg(Color::Yellow);
                    }
                    let ai_indicator = match ai_status {
                        Some(crate::ai_detect::AiStatus::Running { .. }) => " ●",
                        Some(crate::ai_detect::AiStatus::Idle { .. }) => " ◐",
                        Some(crate::ai_detect::AiStatus::Finished { .. }) => " ○",
                        None => "",
                    };
                    Line::from(Span::styled(format!("    {}{}", name, ai_indicator), style))
                }
            }
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, tree_inner);

    // Preview pane
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Preview ");
    let preview_inner = preview_block.inner(preview_area);
    f.render_widget(preview_block, preview_area);

    // Show preview if cursor is on a window
    if let Some(wid) = app.tree_cursor_window_id() {
        if let Some(parser) = app.tree_parsers.get(&wid) {
            let parser = parser.lock().unwrap();
            let pseudo_term = PseudoTerminal::new(parser.screen());
            f.render_widget(pseudo_term, preview_inner);
        }
    }
}

fn draw_tiled(f: &mut Frame, app: &App, area: Rect) {
    if let Some(ref tree) = app.split_tree {
        draw_split_node(f, app, tree, area);
    }
}

/// Recursively render a split tree node
fn draw_split_node(f: &mut Frame, app: &App, tree: &SplitTree, area: Rect) {
    match tree {
        SplitTree::Leaf { pane_id, window_id } => {
            let is_active = app.active_pane == Some(*pane_id);

            let name = app.windows.iter()
                .find(|e| e.id == *window_id)
                .map(|e| e.name.as_str())
                .unwrap_or("?");

            let border_style = if is_active {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(Span::styled(
                    format!(" {} ", name),
                    if is_active {
                        Style::default().fg(Color::Yellow).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ));

            let inner = block.inner(area);
            f.render_widget(block, area);

            if let Some(parser) = app.parser_for(*window_id) {
                let parser = parser.lock().unwrap();
                let pseudo_term = PseudoTerminal::new(parser.screen());
                f.render_widget(pseudo_term, inner);
            }
        }
        SplitTree::Split { direction, ratio, first, second } => {
            let ratio_u32 = (ratio * 1000.0).round() as u32;
            let remainder = 1000u32.saturating_sub(ratio_u32);

            let chunks = Layout::default()
                .direction(match direction {
                    SplitDir::Vertical => Direction::Horizontal,
                    SplitDir::Horizontal => Direction::Vertical,
                })
                .constraints([
                    Constraint::Ratio(ratio_u32, 1000),
                    Constraint::Ratio(remainder, 1000),
                ])
                .split(area);

            draw_split_node(f, app, first, chunks[0]);
            draw_split_node(f, app, second, chunks[1]);
        }
    }
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
    if matches!(app.mode, Mode::Search) {
        let line = Line::from(vec![
            Span::styled(" search: ", Style::default().fg(Color::Magenta).bold()),
            Span::styled(format!("{}_", app.rename_buf), Style::default().fg(Color::White)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if matches!(app.mode, Mode::BranchInput) {
        // Tab bar shows input; popup drawn separately
        let line = Line::from(vec![
            Span::styled(" branch: ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(format!("{}_", app.rename_buf), Style::default().fg(Color::White)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if matches!(app.mode, Mode::PresetInput) {
        let line = Line::from(vec![
            Span::styled(" preset: ", Style::default().fg(Color::Yellow).bold()),
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
    let proj_style = if matches!(app.mode, Mode::ProjectPicker) || (nav && app.tab_focus == TabLevel::Project) {
        Style::default().fg(Color::Black).bg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::Cyan)
    };
    spans.push(Span::styled(format!(" {}", proj_name), proj_style));
    spans.push(Span::styled(" > ", Style::default().fg(Color::DarkGray)));

    // Group
    let grp_name = app.groups.get(active_grp_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let grp_style = if matches!(app.mode, Mode::GroupPicker) || (nav && app.tab_focus == TabLevel::Group) {
        Style::default().fg(Color::Black).bg(Color::Green).bold()
    } else {
        Style::default().fg(Color::Green)
    };
    spans.push(Span::styled(grp_name.to_string(), grp_style));

    // Layout indicator
    match app.layout_mode {
        LayoutMode::Tiled => {
            spans.push(Span::styled(
                " [tiled]",
                Style::default().fg(Color::Magenta).bold(),
            ));
        }
        LayoutMode::Stacked => {}
    }

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
    if matches!(app.mode, Mode::TreeNav) {
        spans.push(Span::styled(" [TREE]", Style::default().fg(Color::Cyan).bold()));
    }
    if matches!(app.mode, Mode::Copy) {
        spans.push(Span::styled(
            format!(" [COPY{}{}]",
                if app.copy_selecting { " SEL" } else { "" },
                if app.copy_scroll_offset > 0 { format!(" +{}", app.copy_scroll_offset) } else { String::new() },
            ),
            Style::default().fg(Color::Magenta).bold(),
        ));
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

/// Map a column position on the tab bar to a clickable tab entry.
/// Click targets are forgiving: separators and empty space map to the nearest tab.
pub fn tab_click_at(app: &App, col: u16) -> Option<TabClick> {
    let active_proj_idx = app.active_project
        .and_then(|id| app.projects.iter().position(|e| e.id == id))
        .unwrap_or(0);
    let active_win_idx = app.active_window
        .and_then(|id| app.windows.iter().position(|e| e.id == id))
        .unwrap_or(0);

    let col = col as usize;
    let mut x: usize = 0;

    // Project: " {name}"
    let proj_name = app.projects.get(active_proj_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let proj_span = 1 + proj_name.len(); // " {name}"
    if col < x + proj_span {
        return Some(TabClick::Project);
    }
    x += proj_span;

    // " > " separator — attribute to project
    if col < x + 3 {
        return Some(TabClick::Project);
    }
    x += 3;

    // Group: "{name}"
    let active_grp_idx = app.active_group
        .and_then(|id| app.groups.iter().position(|e| e.id == id))
        .unwrap_or(0);
    let grp_name = app.groups.get(active_grp_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let grp_span = grp_name.len();
    if col < x + grp_span {
        return Some(TabClick::Group);
    }
    x += grp_span;

    // Layout indicator — attribute to group
    if app.layout_mode == LayoutMode::Tiled {
        let layout_span = 8; // " [tiled]"
        if col < x + layout_span {
            return Some(TabClick::Group);
        }
        x += layout_span;
    }

    // " > " separator — attribute to group
    if col < x + 3 {
        return Some(TabClick::Group);
    }
    x += 3;

    // Windows — replicate visible_tab_range logic
    let nav = matches!(app.mode, Mode::Nav);
    let prefix_width = x;
    let suffix_width = if nav { 6 } else { 0 };
    let avail_width = (app.last_size.0 as usize).saturating_sub(prefix_width + suffix_width + 2); // +2 for border

    let tab_widths: Vec<usize> = app.windows.iter().enumerate().map(|(i, entry)| {
        entry.name.len() + if i > 0 { 3 } else { 0 }
    }).collect();

    let (start, end) = visible_tab_range(&tab_widths, active_win_idx, avail_width);

    if app.windows.is_empty() {
        return None;
    }

    if start > 0 {
        x += 2; // "< "
    }

    let mut last_window_idx = start;
    for i in start..end {
        if i > start {
            // " | " separator — left half goes to previous tab, right half to next
            if col >= x && col < x + 3 {
                return if col < x + 2 {
                    Some(TabClick::Window(i - 1))
                } else {
                    Some(TabClick::Window(i))
                };
            }
            x += 3;
        }
        let name_len = app.windows[i].name.len();
        let ai_len = if app.windows[i].ai_status.is_some() { 1 } else { 0 };
        if col >= x && col < x + name_len + ai_len {
            return Some(TabClick::Window(i));
        }
        x += name_len + ai_len;
        last_window_idx = i;
    }

    // Empty space after all tabs — select the last visible window
    Some(TabClick::Window(last_window_idx))
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
