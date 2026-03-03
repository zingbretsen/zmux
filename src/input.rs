use crate::app::{App, Mode, TabLevel, TreeItem};
use crate::protocol::PaneDirection;
use crate::ui;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders};

pub async fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('q') {
        app.should_quit = true;
        return Ok(());
    }

    match app.mode {
        Mode::Normal => handle_normal_key(app, key).await,
        Mode::Nav => handle_nav_key(app, key).await,
        Mode::AiNav => handle_ai_nav_key(app, key).await,
        Mode::Rename => handle_rename_key(app, key).await,
        Mode::Copy => handle_copy_key(app, key).await,
        Mode::Search => handle_search_key(app, key).await,
        Mode::BranchInput => handle_branch_input_key(app, key).await,
        Mode::PresetInput => handle_preset_input_key(app, key).await,
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
        Mode::TreeNav => handle_tree_nav_key(app, key).await,
        Mode::ProjectPicker | Mode::GroupPicker => handle_picker_key(app, key).await,
    }
}

async fn handle_normal_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('b') {
        app.mode = Mode::Nav;
        return Ok(());
    }

    // Pane focus navigation in tiled mode
    if app.is_tiled() && ctrl {
        let dir = match key.code {
            KeyCode::Char('h') => Some(PaneDirection::Left),
            KeyCode::Char('j') => Some(PaneDirection::Down),
            KeyCode::Char('k') => Some(PaneDirection::Up),
            KeyCode::Char('l') => Some(PaneDirection::Right),
            _ => None,
        };
        if let Some(direction) = dir {
            app.conn.focus_pane(direction).await?;
            return Ok(());
        }
    }

    if let Some(bytes) = key_to_bytes(key) {
        if app.is_tiled() {
            // In tiled mode, send input to the active (focused) window
            if let Some(wid) = app.active_window {
                app.conn.send_input_to_window(wid, bytes).await?;
            }
        } else {
            app.conn.send_input(bytes).await?;
        }
    }
    Ok(())
}

async fn handle_nav_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,

        KeyCode::Char('d') => {
            app.should_detach = true;
        }

        // Resize active pane in tiled mode (Shift+Arrow)
        KeyCode::Left if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            app.conn.resize_pane(PaneDirection::Left).await?;
        }
        KeyCode::Right if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            app.conn.resize_pane(PaneDirection::Right).await?;
        }
        KeyCode::Up if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            app.conn.resize_pane(PaneDirection::Up).await?;
        }
        KeyCode::Down if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            app.conn.resize_pane(PaneDirection::Down).await?;
        }

        KeyCode::Char('k') | KeyCode::Up => {
            app.tab_focus = match app.tab_focus {
                TabLevel::Window => TabLevel::Group,
                TabLevel::Group => TabLevel::Project,
                TabLevel::Project => TabLevel::Project,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.tab_focus = match app.tab_focus {
                TabLevel::Project => TabLevel::Group,
                TabLevel::Group => TabLevel::Window,
                TabLevel::Window => TabLevel::Window,
            };
        }
        KeyCode::Char('h') | KeyCode::Left => app.prev_tab().await?,
        KeyCode::Char('l') | KeyCode::Right => app.next_tab().await?,

        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            app.select_tab_by_index(idx).await?;
        }

        KeyCode::Char('x') => {
            app.conn.close_window().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('c') => {
            app.conn.new_window(None).await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('g') => {
            app.conn.move_window_to_new_group().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('p') => {
            app.conn.move_window_to_new_project().await?;
            app.mode = Mode::Normal;
        }

        // Rename the focused tab
        KeyCode::Char('r') => {
            let target = match app.tab_focus {
                TabLevel::Project => app.active_project,
                TabLevel::Group => app.active_group,
                TabLevel::Window => app.active_window,
            };
            if let Some(id) = target {
                // Pre-fill with current name
                let current_name = match app.tab_focus {
                    TabLevel::Project => app.projects.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                    TabLevel::Group => app.groups.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                    TabLevel::Window => app.windows.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                };
                app.rename_buf = current_name.unwrap_or_default();
                app.rename_target = Some(id);
                app.mode = Mode::Rename;
            }
        }

        // Enter AI navigation mode
        KeyCode::Char('a') => {
            app.conn.next_ai_window().await?;
            app.mode = Mode::AiNav;
        }

        // Save current cwd as group dir (s) or project dir (S)
        KeyCode::Char('S') => {
            app.conn.set_project_dir().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('s') => {
            app.conn.set_group_dir().await?;
            app.mode = Mode::Normal;
        }

        // Save preset
        KeyCode::Char('W') => {
            app.conn.save_preset(None).await?;
            app.mode = Mode::Normal;
        }

        // Load preset
        KeyCode::Char('L') => {
            app.rename_buf.clear();
            app.preset_candidates.clear();
            app.preset_selected = None;
            app.conn.list_presets().await?;
            app.mode = Mode::PresetInput;
        }

        // Worktree: new group from branch
        KeyCode::Char('w') => {
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.conn.list_branches().await?;
            app.mode = Mode::BranchInput;
        }

        // Rebase onto main
        KeyCode::Char('R') => {
            app.conn.rebase_main().await?;
            app.mode = Mode::Normal;
        }

        // Merge worktree branch into main
        KeyCode::Char('M') => {
            app.conn.merge_into_main().await?;
            app.mode = Mode::Normal;
        }

        // Search across windows
        KeyCode::Char('/') => {
            app.rename_buf.clear();
            app.mode = Mode::Search;
        }

        // Enter copy mode
        KeyCode::Char('[') => {
            app.copy_scroll_offset = 0;
            app.copy_selecting = false;
            // Place cursor at the terminal's cursor position
            if let Some(wid) = app.active_window {
                if let Some(parser) = app.parser_for(wid) {
                    let p = parser.lock().unwrap();
                    let pos = p.screen().cursor_position();
                    app.copy_cursor_row = pos.0;
                    app.copy_cursor_col = pos.1;
                }
            }
            app.mode = Mode::Copy;
        }

        // Paste from copy buffer
        KeyCode::Char(']') => {
            if !app.paste_buffer.is_empty() {
                app.conn.send_input(app.paste_buffer.as_bytes().to_vec()).await?;
            }
            app.mode = Mode::Normal;
        }

        // Close group (with worktree cleanup)
        KeyCode::Char('X') => {
            app.conn.close_group(false).await?;
            app.mode = Mode::Normal;
        }

        // Tree nav (session overview)
        KeyCode::Char('f') => {
            app.conn.request_tree().await?;
            app.mode = Mode::TreeNav;
        }

        // Hot reload server binary
        KeyCode::Char('u') => {
            app.conn.reload().await?;
            app.mode = Mode::Normal;
        }

        // Help
        KeyCode::Char('?') => {
            app.mode = Mode::Help;
        }

        // Toggle layout mode (Stacked ↔ Tiled)
        KeyCode::Char('t') => {
            app.conn.toggle_layout().await?;
        }

        // Cycle tile layout algorithm
        KeyCode::Char('T') => {
            app.conn.cycle_layout().await?;
        }

        // Toggle current window in/out of tile set
        KeyCode::Char('m') => {
            if let Some(wid) = app.active_window {
                app.conn.toggle_tile(wid).await?;
            }
        }

        // Cycle pane content forward/backward in tiled mode
        KeyCode::Char('n') => {
            app.conn.cycle_pane_content(true).await?;
        }
        KeyCode::Char('N') => {
            app.conn.cycle_pane_content(false).await?;
        }

        _ => {}
    }
    Ok(())
}

async fn handle_ai_nav_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Char('l') | KeyCode::Right => {
            app.conn.next_ai_window().await?;
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.conn.prev_ai_window().await?;
        }
        // Press 'a' again to go to next
        KeyCode::Char('a') => {
            app.conn.next_ai_window().await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_rename_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let from_tree = !app.tree_data.is_empty();
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.rename_target = None;
            if from_tree {
                app.mode = Mode::TreeNav;
            } else {
                app.mode = Mode::Nav;
            }
        }
        KeyCode::Enter => {
            if let Some(id) = app.rename_target.take() {
                if !app.rename_buf.is_empty() {
                    app.conn.rename(id, app.rename_buf.clone()).await?;
                }
            }
            app.rename_buf.clear();
            if from_tree {
                // Refresh tree data and return to tree nav
                app.conn.request_tree().await?;
                app.mode = Mode::TreeNav;
            } else {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_branch_input_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            // Use selected branch if one is highlighted, otherwise use typed text
            let branch = if let Some(idx) = app.branch_selected {
                let filtered = app.filtered_branches();
                filtered.get(idx).map(|s| s.to_string())
            } else {
                None
            };
            let branch = branch.unwrap_or_else(|| app.rename_buf.clone());
            if !branch.is_empty() {
                app.conn.new_worktree_group(branch).await?;
            }
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.mode = Mode::Normal;
        }
        KeyCode::Down => {
            let count = app.filtered_branches().len();
            if count > 0 {
                app.branch_selected = Some(match app.branch_selected {
                    None => 0,
                    Some(i) => (i + 1).min(count - 1),
                });
            }
        }
        KeyCode::Up => {
            app.branch_selected = match app.branch_selected {
                None | Some(0) => None,
                Some(i) => Some(i - 1),
            };
        }
        KeyCode::Tab => {
            // Autocomplete: fill input with selected branch
            if let Some(idx) = app.branch_selected {
                let filtered = app.filtered_branches();
                if let Some(name) = filtered.get(idx) {
                    app.rename_buf = name.to_string();
                    app.branch_selected = None;
                }
            }
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
            app.branch_selected = None;
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
            app.branch_selected = None;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_preset_input_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.preset_candidates.clear();
            app.preset_selected = None;
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            let name = if let Some(idx) = app.preset_selected {
                let filtered = app.filtered_presets();
                filtered.get(idx).map(|s| s.to_string())
            } else {
                None
            };
            let name = name.unwrap_or_else(|| app.rename_buf.clone());
            if !name.is_empty() {
                app.conn.load_preset(name).await?;
            }
            app.rename_buf.clear();
            app.preset_candidates.clear();
            app.preset_selected = None;
            app.mode = Mode::Normal;
        }
        KeyCode::Down => {
            let count = app.filtered_presets().len();
            if count > 0 {
                app.preset_selected = Some(match app.preset_selected {
                    None => 0,
                    Some(i) => (i + 1).min(count - 1),
                });
            }
        }
        KeyCode::Up => {
            app.preset_selected = match app.preset_selected {
                None | Some(0) => None,
                Some(i) => Some(i - 1),
            };
        }
        KeyCode::Tab => {
            if let Some(idx) = app.preset_selected {
                let filtered = app.filtered_presets();
                if let Some(name) = filtered.get(idx) {
                    app.rename_buf = name.to_string();
                    app.preset_selected = None;
                }
            }
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
            app.preset_selected = None;
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
            app.preset_selected = None;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_search_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            if !app.rename_buf.is_empty() {
                app.conn.search_windows(app.rename_buf.clone()).await?;
            }
            app.rename_buf.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_tree_nav_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let items = app.tree_visible_items();
    let count = items.len();

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.tree_data.clear();
            app.tree_parsers.clear();
            app.mode = Mode::Normal;
        }

        KeyCode::Char('j') | KeyCode::Down => {
            if count > 0 {
                app.tree_cursor = (app.tree_cursor + 1).min(count - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.tree_cursor = app.tree_cursor.saturating_sub(1);
        }

        KeyCode::Char('g') => {
            app.tree_cursor = 0;
        }
        KeyCode::Char('G') => {
            if count > 0 {
                app.tree_cursor = count - 1;
            }
        }

        // Toggle collapse/expand
        KeyCode::Char(' ') => {
            if let Some(item) = items.get(app.tree_cursor) {
                match item {
                    TreeItem::Project { id, .. } => {
                        if app.tree_collapsed_projects.contains(id) {
                            app.tree_collapsed_projects.remove(id);
                        } else {
                            app.tree_collapsed_projects.insert(*id);
                        }
                    }
                    TreeItem::Group { id, .. } => {
                        if app.tree_collapsed_groups.contains(id) {
                            app.tree_collapsed_groups.remove(id);
                        } else {
                            app.tree_collapsed_groups.insert(*id);
                        }
                    }
                    TreeItem::Window { .. } => {}
                }
            }
        }

        // h = always fold / go up
        KeyCode::Char('h') => {
            if let Some(item) = items.get(app.tree_cursor) {
                match item {
                    TreeItem::Project { id, expanded, .. } => {
                        if *expanded {
                            app.tree_collapsed_projects.insert(*id);
                        }
                        // Already collapsed: no-op
                    }
                    TreeItem::Group { id, expanded, .. } => {
                        if *expanded {
                            // Collapse the group
                            app.tree_collapsed_groups.insert(*id);
                        } else {
                            // Already collapsed: collapse parent project and move cursor there
                            if let Some(pid) = app.tree_parent_project(*id) {
                                app.tree_collapsed_projects.insert(pid);
                                let new_items = app.tree_visible_items();
                                if let Some(idx) = new_items.iter().position(
                                    |it| matches!(it, TreeItem::Project { id: pid2, .. } if *pid2 == pid)
                                ) {
                                    app.tree_cursor = idx;
                                }
                            }
                        }
                    }
                    TreeItem::Window { id, .. } => {
                        // Collapse parent group and move cursor there
                        if let Some(gid) = app.tree_parent_group(*id) {
                            app.tree_collapsed_groups.insert(gid);
                            let new_items = app.tree_visible_items();
                            if let Some(idx) = new_items.iter().position(
                                |it| matches!(it, TreeItem::Group { id: gid2, .. } if *gid2 == gid)
                            ) {
                                app.tree_cursor = idx;
                            }
                        }
                    }
                }
            }
        }

        // l = always expand / go into first child
        KeyCode::Char('l') => {
            if let Some(item) = items.get(app.tree_cursor) {
                match item {
                    TreeItem::Project { id, expanded, .. } => {
                        if !*expanded {
                            app.tree_collapsed_projects.remove(id);
                        }
                        // Move to first child (next item after this project)
                        let new_items = app.tree_visible_items();
                        let new_count = new_items.len();
                        if app.tree_cursor + 1 < new_count {
                            app.tree_cursor += 1;
                        }
                    }
                    TreeItem::Group { id, expanded, .. } => {
                        if !*expanded {
                            app.tree_collapsed_groups.remove(id);
                        }
                        // Move to first child (next item after this group)
                        let new_items = app.tree_visible_items();
                        let new_count = new_items.len();
                        if app.tree_cursor + 1 < new_count {
                            app.tree_cursor += 1;
                        }
                    }
                    TreeItem::Window { .. } => {
                        // Windows have no children, no-op
                    }
                }
            }
        }

        // H = collapse one level at a time
        KeyCode::Char('H') => {
            let cursor_id = items.get(app.tree_cursor).map(|it| tree_item_id(it));
            // If any groups are expanded, collapse all groups first
            let any_group_expanded = app.tree_data.iter().any(|proj| {
                !app.tree_collapsed_projects.contains(&proj.id)
                    && proj.groups.iter().any(|grp| !app.tree_collapsed_groups.contains(&grp.id))
            });
            if any_group_expanded {
                for proj in &app.tree_data {
                    for grp in &proj.groups {
                        app.tree_collapsed_groups.insert(grp.id);
                    }
                }
            } else {
                // All groups collapsed (or hidden); collapse all projects
                for proj in &app.tree_data {
                    app.tree_collapsed_projects.insert(proj.id);
                }
            }
            // Try to keep cursor on the same item (or its nearest ancestor)
            if let Some(cid) = cursor_id {
                tree_follow_cursor(app, cid);
            }
        }

        // L = expand one level at a time
        KeyCode::Char('L') => {
            let cursor_id = items.get(app.tree_cursor).map(|it| tree_item_id(it));
            // If any projects are collapsed, expand all projects first
            let any_project_collapsed = app.tree_data.iter().any(|proj| {
                app.tree_collapsed_projects.contains(&proj.id)
            });
            if any_project_collapsed {
                app.tree_collapsed_projects.clear();
            } else {
                // All projects expanded; expand all groups
                app.tree_collapsed_groups.clear();
            }
            // Keep cursor on the same item
            if let Some(cid) = cursor_id {
                tree_follow_cursor(app, cid);
            }
        }

        // J = next item of same level
        KeyCode::Char('J') => {
            if let Some(item) = items.get(app.tree_cursor) {
                let target_level = tree_item_level(item);
                for i in (app.tree_cursor + 1)..count {
                    if tree_item_level(&items[i]) == target_level {
                        app.tree_cursor = i;
                        break;
                    }
                }
            }
        }

        // K = previous item of same level
        KeyCode::Char('K') => {
            if let Some(item) = items.get(app.tree_cursor) {
                let target_level = tree_item_level(item);
                for i in (0..app.tree_cursor).rev() {
                    if tree_item_level(&items[i]) == target_level {
                        app.tree_cursor = i;
                        break;
                    }
                }
            }
        }

        // r = rename item under cursor
        KeyCode::Char('r') => {
            if let Some(item) = items.get(app.tree_cursor) {
                let id = tree_item_id(item);
                let name = app.tree_node_name(id).unwrap_or_default();
                app.rename_buf = name;
                app.rename_target = Some(id);
                app.mode = Mode::Rename;
            }
        }

        // x = close/kill item under cursor
        KeyCode::Char('x') => {
            if let Some(item) = items.get(app.tree_cursor) {
                let id = tree_item_id(item);
                app.conn.close_node(id).await?;
                // Refresh tree after close
                app.conn.request_tree().await?;
            }
        }

        // c = send Ctrl-C (interrupt) to the window under cursor
        KeyCode::Char('c') => {
            if let Some(TreeItem::Window { id, .. }) = items.get(app.tree_cursor) {
                app.conn.send_input_to_window(*id, vec![0x03]).await?;
                // Refresh tree to update preview
                app.conn.request_tree().await?;
            }
        }

        // Select item and navigate
        KeyCode::Enter => {
            if let Some(item) = items.get(app.tree_cursor) {
                match item {
                    TreeItem::Project { id, .. } => {
                        app.conn.select_project(*id).await?;
                    }
                    TreeItem::Group { id, .. } => {
                        app.conn.select_group(*id).await?;
                    }
                    TreeItem::Window { id, .. } => {
                        app.conn.select_window(*id).await?;
                    }
                }
            }
            app.tree_data.clear();
            app.tree_parsers.clear();
            app.mode = Mode::Normal;
        }

        _ => {}
    }

    // Clamp cursor after potential collapse changes
    let new_count = app.tree_visible_items().len();
    if new_count > 0 && app.tree_cursor >= new_count {
        app.tree_cursor = new_count - 1;
    }

    Ok(())
}

fn tree_item_level(item: &TreeItem) -> u8 {
    match item {
        TreeItem::Project { .. } => 0,
        TreeItem::Group { .. } => 1,
        TreeItem::Window { .. } => 2,
    }
}

fn tree_item_id(item: &TreeItem) -> crate::protocol::NodeId {
    match item {
        TreeItem::Project { id, .. } => *id,
        TreeItem::Group { id, .. } => *id,
        TreeItem::Window { id, .. } => *id,
    }
}

/// After a collapse/expand operation, find the item by ID in the new visible list.
/// If the item is no longer visible (collapsed away), find its nearest visible ancestor.
fn tree_follow_cursor(app: &mut App, target_id: crate::protocol::NodeId) {
    let new_items = app.tree_visible_items();
    // Try exact match first
    if let Some(idx) = new_items.iter().position(|it| tree_item_id(it) == target_id) {
        app.tree_cursor = idx;
        return;
    }
    // Item was collapsed away — find parent group, then parent project
    if let Some(gid) = app.tree_parent_group(target_id) {
        if let Some(idx) = new_items.iter().position(|it| tree_item_id(it) == gid) {
            app.tree_cursor = idx;
            return;
        }
        // Group also collapsed, find project
        if let Some(pid) = app.tree_parent_project(gid) {
            if let Some(idx) = new_items.iter().position(|it| tree_item_id(it) == pid) {
                app.tree_cursor = idx;
                return;
            }
        }
    }
    if let Some(pid) = app.tree_parent_project(target_id) {
        if let Some(idx) = new_items.iter().position(|it| tree_item_id(it) == pid) {
            app.tree_cursor = idx;
            return;
        }
    }
}

async fn handle_copy_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let screen_rows = app.term_rows;
    let screen_cols = app.term_cols;
    let half_page = (screen_rows / 2) as usize;

    let set_scrollback = |app: &mut App, offset: usize| -> usize {
        if let Some(wid) = app.active_window {
            if let Some(parser) = app.parser_for(wid) {
                let mut p = parser.lock().unwrap();
                p.set_scrollback(offset);
                return p.screen().scrollback();
            }
        }
        0
    };

    let max_scrollback = |app: &mut App| -> usize {
        if let Some(wid) = app.active_window {
            if let Some(parser) = app.parser_for(wid) {
                let mut p = parser.lock().unwrap();
                let current = p.screen().scrollback();
                p.set_scrollback(usize::MAX);
                let max = p.screen().scrollback();
                p.set_scrollback(current);
                return max;
            }
        }
        0
    };

    // Helper: absolute line number for the cursor (scrollback_offset + inverted row)
    let abs_line = |app: &App| -> usize {
        // Row 0 is the top of viewport. With scrollback_offset, the top of viewport
        // is `scrollback_offset` lines above the bottom of live screen.
        // Absolute line: higher = further back in history
        app.copy_scroll_offset + (screen_rows - 1 - app.copy_cursor_row) as usize
    };

    // Helper: extract text between two absolute positions
    let extract_selection = |app: &mut App, start: (usize, u16), end: (usize, u16)| -> String {
        // Normalize so that `from` is earlier (higher abs line or same line lower col)
        let (from, to) = if start.0 > end.0 || (start.0 == end.0 && start.1 <= end.1) {
            (start, end)
        } else {
            (end, start)
        };

        let wid = match app.active_window {
            Some(w) => w,
            None => return String::new(),
        };
        let parser = match app.parser_for(wid) {
            Some(p) => p,
            None => return String::new(),
        };
        let mut p = parser.lock().unwrap();

        let mut result = String::new();
        // Iterate from `from` (higher abs = older) down to `to` (lower abs = newer)
        for abs in (to.0..=from.0).rev() {
            // Set scrollback so this absolute line is visible
            // abs line is at viewport row = screen_rows - 1 - (abs - scrollback_offset)
            // We need abs to be within [scrollback_offset, scrollback_offset + screen_rows - 1]
            // Simplest: set scrollback so this line is at row 0 (top)
            let needed_offset = abs.saturating_sub(screen_rows as usize - 1);
            p.set_scrollback(needed_offset);
            let row_in_viewport = (abs - needed_offset) as u16;
            let row_mapped = screen_rows - 1 - row_in_viewport;

            let col_start = if abs == from.0 { from.1 } else { 0 };
            let col_end = if abs == to.0 { to.1 } else { screen_cols - 1 };

            for col in col_start..=col_end {
                if let Some(cell) = p.screen().cell(row_mapped, col) {
                    let c = cell.contents();
                    if c.is_empty() {
                        result.push(' ');
                    } else {
                        result.push_str(&c);
                    }
                }
            }
            // Trim trailing spaces on each line, add newline between lines
            if abs != to.0 {
                let trimmed = result.trim_end_matches(' ');
                result.truncate(trimmed.len());
                result.push('\n');
            }
        }
        // Trim trailing spaces on last line
        let trimmed = result.trim_end_matches(' ');
        result.truncate(trimmed.len());

        // Restore scrollback
        p.set_scrollback(app.copy_scroll_offset);
        result
    };

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.copy_selecting {
                app.copy_selecting = false;
            } else {
                app.copy_scroll_offset = 0;
                set_scrollback(app, 0);
                app.mode = Mode::Normal;
            }
        }

        // Cursor movement
        KeyCode::Char('h') | KeyCode::Left => {
            app.copy_cursor_col = app.copy_cursor_col.saturating_sub(1);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.copy_cursor_col = (app.copy_cursor_col + 1).min(screen_cols - 1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.copy_cursor_row > 0 {
                app.copy_cursor_row -= 1;
            } else {
                // Scroll up
                app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(1);
                app.copy_scroll_offset = set_scrollback(app, app.copy_scroll_offset);
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if app.copy_cursor_row < screen_rows - 1 {
                app.copy_cursor_row += 1;
            } else {
                // Scroll down
                if app.copy_scroll_offset > 0 {
                    app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(1);
                    set_scrollback(app, app.copy_scroll_offset);
                }
            }
        }
        KeyCode::Char('0') => {
            app.copy_cursor_col = 0;
        }
        KeyCode::Char('$') => {
            app.copy_cursor_col = screen_cols - 1;
        }
        KeyCode::Char('w') => {
            // Jump to next word: skip non-space, then skip space, wrapping lines
            if let Some(wid) = app.active_window {
                if let Some(parser) = app.parser_for(wid) {
                    let p = parser.lock().unwrap();
                    let mut col = app.copy_cursor_col;
                    let mut row = app.copy_cursor_row;
                    let mut scroll = app.copy_scroll_offset;
                    // Skip current word (non-spaces)
                    loop {
                        if col >= screen_cols - 1 {
                            break;
                        }
                        if let Some(cell) = p.screen().cell(row, col) {
                            if cell.contents().trim().is_empty() { break; }
                        }
                        col += 1;
                    }
                    // Skip spaces, wrapping to next line if needed
                    let mut found = false;
                    loop {
                        while col < screen_cols {
                            if let Some(cell) = p.screen().cell(row, col) {
                                if !cell.contents().trim().is_empty() {
                                    found = true;
                                    break;
                                }
                            }
                            col += 1;
                        }
                        if found { break; }
                        // Wrap to next line
                        if row < screen_rows - 1 {
                            row += 1;
                            col = 0;
                        } else if scroll > 0 {
                            scroll = scroll.saturating_sub(1);
                            col = 0;
                            // row stays at bottom
                        } else {
                            // At the very bottom, can't go further
                            col = screen_cols - 1;
                            break;
                        }
                    }
                    drop(p);
                    if scroll != app.copy_scroll_offset {
                        set_scrollback(app, scroll);
                        app.copy_scroll_offset = scroll;
                    }
                    app.copy_cursor_row = row;
                    app.copy_cursor_col = col;
                }
            }
        }
        KeyCode::Char('b') => {
            // Jump to previous word, wrapping lines
            if let Some(wid) = app.active_window {
                if let Some(parser) = app.parser_for(wid) {
                    let p = parser.lock().unwrap();
                    let mut col = app.copy_cursor_col;
                    let mut row = app.copy_cursor_row;
                    let mut scroll = app.copy_scroll_offset;
                    // Skip spaces backward, wrapping to previous line if needed
                    let mut found_nonspace = false;
                    loop {
                        while col > 0 {
                            col -= 1;
                            if let Some(cell) = p.screen().cell(row, col) {
                                if !cell.contents().trim().is_empty() {
                                    found_nonspace = true;
                                    break;
                                }
                            }
                        }
                        if found_nonspace { break; }
                        // Check col 0
                        if col == 0 {
                            if let Some(cell) = p.screen().cell(row, col) {
                                if !cell.contents().trim().is_empty() {
                                    found_nonspace = true;
                                    break;
                                }
                            }
                        }
                        // Wrap to previous line
                        if row > 0 {
                            row -= 1;
                            col = screen_cols - 1;
                        } else {
                            let new_scroll = scroll.saturating_add(1);
                            let actual = max_scrollback(app);
                            if new_scroll <= actual {
                                scroll = new_scroll;
                                col = screen_cols - 1;
                                // row stays at top
                            } else {
                                // At the very top
                                break;
                            }
                        }
                    }
                    if found_nonspace {
                        // Skip word backward (find start of this word)
                        while col > 0 {
                            if let Some(cell) = p.screen().cell(row, col - 1) {
                                if cell.contents().trim().is_empty() { break; }
                            }
                            col -= 1;
                        }
                    }
                    drop(p);
                    if scroll != app.copy_scroll_offset {
                        set_scrollback(app, scroll);
                        app.copy_scroll_offset = scroll;
                    }
                    app.copy_cursor_row = row;
                    app.copy_cursor_col = col;
                }
            }
        }

        // Scrolling (cursor stays in viewport)
        KeyCode::Char('u') if ctrl => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(half_page);
            app.copy_scroll_offset = set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('d') if ctrl => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(half_page);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::PageUp => {
            let full_page = screen_rows as usize;
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(full_page);
            app.copy_scroll_offset = set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::PageDown => {
            let full_page = screen_rows as usize;
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(full_page);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('g') if !app.copy_selecting => {
            app.copy_scroll_offset = max_scrollback(app);
            app.copy_scroll_offset = set_scrollback(app, app.copy_scroll_offset);
            app.copy_cursor_row = 0;
        }
        KeyCode::Char('G') if !app.copy_selecting => {
            app.copy_scroll_offset = 0;
            set_scrollback(app, 0);
            app.copy_cursor_row = screen_rows - 1;
        }

        // Selection
        KeyCode::Char(' ') => {
            if app.copy_selecting {
                app.copy_selecting = false;
            } else {
                app.copy_selecting = true;
                app.copy_sel_start = (abs_line(app), app.copy_cursor_col);
            }
        }
        KeyCode::Enter => {
            if app.copy_selecting {
                let start = app.copy_sel_start;
                let end = (abs_line(app), app.copy_cursor_col);
                app.paste_buffer = extract_selection(app, start, end);
                app.copy_selecting = false;
                app.copy_scroll_offset = 0;
                set_scrollback(app, 0);
                app.mode = Mode::Normal;
            }
        }

        _ => {}
    }
    Ok(())
}

async fn handle_picker_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let items_len = if matches!(app.mode, Mode::ProjectPicker) {
        app.projects.len()
    } else {
        app.groups.len()
    };

    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
        }
        KeyCode::Enter => {
            if matches!(app.mode, Mode::ProjectPicker) {
                if let Some(entry) = app.projects.get(app.dropdown_selected) {
                    app.conn.select_project(entry.id).await?;
                }
            } else {
                if let Some(entry) = app.groups.get(app.dropdown_selected) {
                    app.conn.select_group(entry.id).await?;
                }
            }
            app.mode = Mode::Normal;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if items_len > 0 {
                app.dropdown_selected = (app.dropdown_selected + 1).min(items_len - 1);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.dropdown_selected = app.dropdown_selected.saturating_sub(1);
        }
        _ => {}
    }
    Ok(())
}

pub async fn handle_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> Result<()> {
    // Handle clicks when a picker dropdown is open
    if matches!(app.mode, Mode::ProjectPicker | Mode::GroupPicker) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(rect) = ui::picker_dropdown_rect(app) {
                // Click inside the dropdown — select the clicked item
                if mouse.row >= rect.y + 1
                    && mouse.row < rect.y + rect.height.saturating_sub(1)
                    && mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                {
                    let max_visible = 10usize;
                    let items = if matches!(app.mode, Mode::ProjectPicker) {
                        &app.projects
                    } else {
                        &app.groups
                    };
                    let scroll_offset = if app.dropdown_selected >= max_visible {
                        app.dropdown_selected - max_visible + 1
                    } else {
                        0
                    };
                    let item_idx = scroll_offset + (mouse.row - rect.y - 1) as usize;
                    if let Some(entry) = items.get(item_idx) {
                        if matches!(app.mode, Mode::ProjectPicker) {
                            app.conn.select_project(entry.id).await?;
                        } else {
                            app.conn.select_group(entry.id).await?;
                        }
                    }
                    app.mode = Mode::Normal;
                    return Ok(());
                }
            }
            // Click outside dropdown — dismiss and fall through to normal handling
            app.mode = Mode::Normal;
        }
        // Non-click events while picker is open — ignore
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Ok(());
        }
    }

    // Tab bar clicks work in any mode
    if mouse.row == 0 {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(click) = ui::tab_click_at(app, mouse.column) {
                match click {
                    ui::TabClick::Project(_) => {
                        app.dropdown_x = 1; // project starts at column 1
                        app.dropdown_selected = app.active_project
                            .and_then(|id| app.projects.iter().position(|e| e.id == id))
                            .unwrap_or(0);
                        app.mode = Mode::ProjectPicker;
                        return Ok(());
                    }
                    ui::TabClick::Group(_) => {
                        let proj_name = app.active_project
                            .and_then(|id| app.projects.iter().find(|e| e.id == id))
                            .map(|e| e.name.as_str())
                            .unwrap_or("?");
                        app.dropdown_x = (1 + proj_name.len() + 3) as u16;
                        app.dropdown_selected = app.active_group
                            .and_then(|id| app.groups.iter().position(|e| e.id == id))
                            .unwrap_or(0);
                        app.mode = Mode::GroupPicker;
                        return Ok(());
                    }
                    ui::TabClick::Window(idx) => {
                        if let Some(entry) = app.windows.get(idx) {
                            app.conn.select_window(entry.id).await?;
                        }
                    }
                }
                app.mode = Mode::Normal;
            }
            return Ok(());
        }
    }

    // Tree nav mouse handling (before normal mode handling)
    if matches!(app.mode, Mode::TreeNav) {
        let items = app.tree_visible_items();
        let count = items.len();
        if count == 0 {
            return Ok(());
        }

        // Recreate the tree area layout (must match draw_tree_nav in ui.rs)
        let (cols, rows) = app.last_size;
        let full_area = Rect::new(0, 0, cols, rows);
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(full_area);
        let tree_area = halves[0];
        let tree_block = Block::default().borders(Borders::ALL);
        let tree_inner = tree_block.inner(tree_area);
        let visible_height = tree_inner.height as usize;
        let scroll_offset = if app.tree_cursor >= visible_height {
            app.tree_cursor - visible_height + 1
        } else {
            0
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.column >= tree_inner.x
                    && mouse.column < tree_inner.x + tree_inner.width
                    && mouse.row >= tree_inner.y
                    && mouse.row < tree_inner.y + tree_inner.height
                {
                    let row_in_tree = (mouse.row - tree_inner.y) as usize;
                    let item_idx = scroll_offset + row_in_tree;
                    if let Some(item) = items.get(item_idx) {
                        match item {
                            TreeItem::Project { id, .. } => {
                                app.conn.select_project(*id).await?;
                            }
                            TreeItem::Group { id, .. } => {
                                app.conn.select_group(*id).await?;
                            }
                            TreeItem::Window { id, .. } => {
                                app.conn.select_window(*id).await?;
                            }
                        }
                        app.tree_data.clear();
                        app.tree_parsers.clear();
                        app.mode = Mode::Normal;
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                app.tree_cursor = app.tree_cursor.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                app.tree_cursor = (app.tree_cursor + 3).min(count - 1);
            }
            _ => {}
        }
        return Ok(());
    }

    match app.mode {
        Mode::Normal => {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    // Enter copy/scrollback mode and scroll up
                    app.copy_scroll_offset = 0;
                    app.copy_selecting = false;
                    if let Some(wid) = app.active_window {
                        if let Some(parser) = app.parser_for(wid) {
                            let p = parser.lock().unwrap();
                            let pos = p.screen().cursor_position();
                            app.copy_cursor_row = pos.0;
                            app.copy_cursor_col = pos.1;
                        }
                    }
                    app.mode = Mode::Copy;
                    // Scroll up by 3 lines
                    if let Some(wid) = app.active_window {
                        if let Some(parser) = app.parser_for(wid) {
                            let mut p = parser.lock().unwrap();
                            app.copy_scroll_offset = 3;
                            p.set_scrollback(app.copy_scroll_offset);
                            app.copy_scroll_offset = p.screen().scrollback();
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    // In normal mode, scroll down does nothing (already at bottom)
                }
                _ => {}
            }
        }
        Mode::Copy => {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(wid) = app.active_window {
                        if let Some(parser) = app.parser_for(wid) {
                            let mut p = parser.lock().unwrap();
                            app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(3);
                            p.set_scrollback(app.copy_scroll_offset);
                            app.copy_scroll_offset = p.screen().scrollback();
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(wid) = app.active_window {
                        if let Some(parser) = app.parser_for(wid) {
                            let mut p = parser.lock().unwrap();
                            app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(3);
                            p.set_scrollback(app.copy_scroll_offset);
                            app.copy_scroll_offset = p.screen().scrollback();
                            // Exit copy mode if scrolled back to bottom
                            if app.copy_scroll_offset == 0 {
                                app.copy_selecting = false;
                                app.mode = Mode::Normal;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                Some(vec![ctrl_byte])
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                Some(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn key_to_bytes_plain_char() {
        let key = make_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(key_to_bytes(&key), Some(vec![b'a']));
    }

    #[test]
    fn key_to_bytes_ctrl_char() {
        let key = make_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(&key), Some(vec![3])); // Ctrl-C = 0x03
    }

    #[test]
    fn key_to_bytes_enter() {
        let key = make_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(&key), Some(vec![b'\r']));
    }

    #[test]
    fn key_to_bytes_arrow_keys() {
        assert_eq!(key_to_bytes(&make_key(KeyCode::Up, KeyModifiers::NONE)), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Down, KeyModifiers::NONE)), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Right, KeyModifiers::NONE)), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Left, KeyModifiers::NONE)), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn key_to_bytes_function_keys() {
        assert_eq!(key_to_bytes(&make_key(KeyCode::F(1), KeyModifiers::NONE)), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_to_bytes(&make_key(KeyCode::F(12), KeyModifiers::NONE)), Some(b"\x1b[24~".to_vec()));
        assert_eq!(key_to_bytes(&make_key(KeyCode::F(13), KeyModifiers::NONE)), None);
    }

    #[test]
    fn key_to_bytes_special_keys() {
        assert_eq!(key_to_bytes(&make_key(KeyCode::Backspace, KeyModifiers::NONE)), Some(vec![0x7f]));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Tab, KeyModifiers::NONE)), Some(vec![b'\t']));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Esc, KeyModifiers::NONE)), Some(vec![0x1b]));
        assert_eq!(key_to_bytes(&make_key(KeyCode::Delete, KeyModifiers::NONE)), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn key_to_bytes_utf8() {
        let key = make_key(KeyCode::Char('é'), KeyModifiers::NONE);
        let bytes = key_to_bytes(&key).unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "é");
    }
}
