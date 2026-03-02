use crate::app::{App, Mode, TabLevel, TreeItem};
use crate::protocol::{LayoutMode, NodeId, TileLayout};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::HashMap;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

pub enum TabClick {
    Project(usize),
    Group(usize),
    Window(usize),
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

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
 t       Toggle tiled       T       Cycle tile layout
 m       Toggle window tile n/N     Cycle pane content
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
    let x = area.x + 1;
    let y = area.y + 1; // just below tab bar
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

    let x = area.x + 1;
    let y = area.y + 1;
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
    let windows = &app.tiled_windows;
    let n = windows.len();
    if n == 0 {
        return;
    }

    let rects = compute_tile_rects(app.tile_layout, windows, area, &app.pane_weights);

    for (i, &wid) in windows.iter().enumerate() {
        if i >= rects.len() {
            break;
        }
        let rect = rects[i];
        let is_active = app.active_window == Some(wid);

        // Find window name
        let name = app.windows.iter()
            .find(|e| e.id == wid)
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

        let inner = block.inner(rect);
        f.render_widget(block, rect);

        if let Some(parser) = app.parser_for(wid) {
            let parser = parser.lock().unwrap();
            let pseudo_term = PseudoTerminal::new(parser.screen());
            f.render_widget(pseudo_term, inner);
        }
    }
}

/// Compute rects for each tiled pane within the given area, using per-pane weights.
fn compute_tile_rects(layout: TileLayout, windows: &[NodeId], area: Rect, weights: &HashMap<NodeId, (f64, f64)>) -> Vec<Rect> {
    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![area];
    }

    // Convert float weights to integer ratios for Constraint::Ratio
    // Multiply by 100 and round to get integer proportions
    let w_ratios: Vec<u32> = windows.iter()
        .map(|&id| (weights.get(&id).map_or(1.0, |&(w, _)| w) * 100.0).round() as u32)
        .collect();
    let h_ratios: Vec<u32> = windows.iter()
        .map(|&id| (weights.get(&id).map_or(1.0, |&(_, h)| h) * 100.0).round() as u32)
        .collect();

    match layout {
        TileLayout::EqualColumns => {
            let total: u32 = w_ratios.iter().sum();
            let constraints: Vec<Constraint> = w_ratios.iter()
                .map(|&r| Constraint::Ratio(r, total))
                .collect();
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area)
                .to_vec()
        }
        TileLayout::EqualRows => {
            let total: u32 = h_ratios.iter().sum();
            let constraints: Vec<Constraint> = h_ratios.iter()
                .map(|&r| Constraint::Ratio(r, total))
                .collect();
            Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area)
                .to_vec()
        }
        TileLayout::MainLeft => {
            let main_w = w_ratios[0];
            // Average the side pane widths for the horizontal split
            let side_avg: u32 = if n > 1 { w_ratios[1..].iter().sum::<u32>() / (n - 1) as u32 } else { 100 };
            let total_w = main_w + side_avg;
            let horiz = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(main_w, total_w), Constraint::Ratio(side_avg, total_w)])
                .split(area);
            let mut rects = vec![horiz[0]];
            let side_count = n - 1;
            let side_h_ratios: Vec<u32> = h_ratios[1..].to_vec();
            let side_total: u32 = side_h_ratios.iter().sum();
            let constraints: Vec<Constraint> = side_h_ratios.iter()
                .map(|&r| Constraint::Ratio(r, side_total))
                .collect();
            let side_rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(horiz[1]);
            rects.extend(side_rects.iter().take(side_count));
            rects
        }
        TileLayout::Grid => {
            let cols = (n as f64).sqrt().ceil() as usize;
            let rows = (n + cols - 1) / cols;
            let row_rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![Constraint::Ratio(1, rows as u32); rows])
                .split(area);
            let mut rects = Vec::new();
            let mut idx = 0;
            for row_rect in row_rects.iter() {
                let in_this_row = cols.min(n - idx);
                let col_rects = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(vec![Constraint::Ratio(1, in_this_row as u32); in_this_row])
                    .split(*row_rect);
                rects.extend(col_rects.iter());
                idx += in_this_row;
            }
            rects
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

    // Layout indicator
    match app.layout_mode {
        LayoutMode::Tiled => {
            spans.push(Span::styled(
                format!(" [{}]", app.tile_layout.name()),
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
        let tile_prefix = if app.tiled_windows.contains(&entry.id) { 1 } else { 0 }; // "*"
        entry.name.len() + tile_prefix + if i > 0 { 3 } else { 0 } // " | " separator
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
        let is_tiled = app.tiled_windows.contains(&app.windows[i].id);
        let style = if i == active_win_idx {
            if nav && app.tab_focus == TabLevel::Window {
                Style::default().fg(Color::Black).bg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::Yellow).bold()
            }
        } else {
            Style::default().fg(Color::White)
        };
        if is_tiled {
            spans.push(Span::styled("*", Style::default().fg(Color::Magenta)));
        }
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
        return Some(TabClick::Project(active_proj_idx));
    }
    x += proj_span;

    // " > " separator — attribute to project
    if col < x + 3 {
        return Some(TabClick::Project(active_proj_idx));
    }
    x += 3;

    // Group: "{name}"
    let active_grp_idx = app.active_group
        .and_then(|id| app.groups.iter().position(|e| e.id == id))
        .unwrap_or(0);
    let grp_name = app.groups.get(active_grp_idx).map(|e| e.name.as_str()).unwrap_or("?");
    let grp_span = grp_name.len();
    if col < x + grp_span {
        return Some(TabClick::Group(active_grp_idx));
    }
    x += grp_span;

    // Layout indicator — attribute to group
    if app.layout_mode == LayoutMode::Tiled {
        let layout_span = 2 + app.tile_layout.name().len() + 1; // " [name]"
        if col < x + layout_span {
            return Some(TabClick::Group(active_grp_idx));
        }
        x += layout_span;
    }

    // " > " separator — attribute to group
    if col < x + 3 {
        return Some(TabClick::Group(active_grp_idx));
    }
    x += 3;

    // Windows — replicate visible_tab_range logic
    let nav = matches!(app.mode, Mode::Nav);
    let prefix_width = x;
    let suffix_width = if nav { 6 } else { 0 };
    let avail_width = (app.last_size.0 as usize).saturating_sub(prefix_width + suffix_width);

    let tab_widths: Vec<usize> = app.windows.iter().enumerate().map(|(i, entry)| {
        let tile_prefix = if app.tiled_windows.contains(&entry.id) { 1 } else { 0 };
        entry.name.len() + tile_prefix + if i > 0 { 3 } else { 0 }
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
        let tile_len = if app.tiled_windows.contains(&app.windows[i].id) { 1 } else { 0 };
        let name_len = app.windows[i].name.len();
        let ai_len = if app.windows[i].ai_status.is_some() { 1 } else { 0 };
        if col >= x && col < x + tile_len + name_len + ai_len {
            return Some(TabClick::Window(i));
        }
        x += tile_len + name_len + ai_len;
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
