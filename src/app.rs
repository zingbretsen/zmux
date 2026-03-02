use crate::client::ClientConnection;
use crate::protocol::{LayoutMode, NodeId, ServerMsg, TabEntry, TileLayout, TreeProject};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(PartialEq)]
pub enum Mode {
    Normal,
    Nav,
    Copy,
    Search,
    AiNav,
    Rename,
    BranchInput,
    PresetInput,
    Help,
    TreeNav,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TabLevel {
    Project,
    Group,
    Window,
}

/// Flattened item in the tree nav view
pub enum TreeItem {
    Project { id: NodeId, name: String, expanded: bool },
    Group { id: NodeId, name: String, expanded: bool },
    Window { id: NodeId, name: String, ai_status: Option<crate::ai_detect::AiStatus> },
}

pub struct App {
    pub conn: ClientConnection,
    pub should_quit: bool,
    pub should_detach: bool,
    pub should_reconnect: bool,
    pub last_size: (u16, u16),
    pub mode: Mode,
    pub tab_focus: TabLevel,

    // Cached state from server
    pub projects: Vec<TabEntry>,
    pub groups: Vec<TabEntry>,
    pub windows: Vec<TabEntry>,
    pub active_project: Option<NodeId>,
    pub active_group: Option<NodeId>,
    pub active_window: Option<NodeId>,

    // Layout state
    pub layout_mode: LayoutMode,
    pub tile_layout: TileLayout,
    pub tiled_windows: Vec<NodeId>,
    pub pane_weights: HashMap<NodeId, (f64, f64)>,

    // Client-side vt100 parsers keyed by window ID
    pub parsers: HashMap<NodeId, Arc<Mutex<vt100::Parser>>>,
    /// Default terminal size for creating new parsers
    pub term_rows: u16,
    pub term_cols: u16,

    // Rename input buffer
    pub rename_buf: String,
    pub rename_target: Option<NodeId>,

    // Copy mode state
    pub copy_scroll_offset: usize,
    pub copy_cursor_row: u16,
    pub copy_cursor_col: u16,
    pub copy_selecting: bool,
    pub copy_sel_start: (usize, u16), // (absolute line, col)
    pub paste_buffer: String,

    // Status message shown briefly in the tab bar
    pub status_message: Option<(String, Instant)>,

    // Branch picker state
    pub branch_candidates: Vec<String>,
    pub branch_selected: Option<usize>,

    // Preset picker state
    pub preset_candidates: Vec<String>,
    pub preset_selected: Option<usize>,

    // Tree nav state
    pub tree_data: Vec<TreeProject>,
    pub tree_cursor: usize,
    pub tree_collapsed_projects: HashSet<NodeId>,
    pub tree_collapsed_groups: HashSet<NodeId>,
    pub tree_active_project: Option<NodeId>,
    pub tree_active_group: Option<NodeId>,
    pub tree_active_window: Option<NodeId>,
    /// Parsers for tree nav preview (separate from main parsers)
    pub tree_parsers: HashMap<NodeId, Arc<Mutex<vt100::Parser>>>,
}

impl App {
    pub async fn new(conn: ClientConnection, rows: u16, cols: u16) -> Result<Self> {
        conn.send_resize(cols, rows).await?;
        let term_rows = rows.saturating_sub(1);
        Ok(App {
            conn,
            should_quit: false,
            should_detach: false,
            should_reconnect: false,
            last_size: (cols, rows),
            mode: Mode::Normal,
            tab_focus: TabLevel::Window,
            projects: Vec::new(),
            groups: Vec::new(),
            windows: Vec::new(),
            active_project: None,
            active_group: None,
            active_window: None,
            layout_mode: LayoutMode::Stacked,
            tile_layout: TileLayout::EqualColumns,
            tiled_windows: Vec::new(),
            pane_weights: HashMap::new(),
            parsers: HashMap::new(),
            term_rows,
            term_cols: cols,
            rename_buf: String::new(),
            rename_target: None,
            copy_scroll_offset: 0,
            copy_cursor_row: 0,
            copy_cursor_col: 0,
            copy_selecting: false,
            copy_sel_start: (0, 0),
            paste_buffer: String::new(),
            status_message: None,
            branch_candidates: Vec::new(),
            branch_selected: None,
            preset_candidates: Vec::new(),
            preset_selected: None,
            tree_data: Vec::new(),
            tree_cursor: 0,
            tree_collapsed_projects: HashSet::new(),
            tree_collapsed_groups: HashSet::new(),
            tree_active_project: None,
            tree_active_group: None,
            tree_active_window: None,
            tree_parsers: HashMap::new(),
        })
    }

    /// Get or create a parser for a window
    pub fn get_parser(&mut self, window_id: NodeId) -> Arc<Mutex<vt100::Parser>> {
        self.parsers.entry(window_id).or_insert_with(|| {
            Arc::new(Mutex::new(vt100::Parser::new(self.term_rows, self.term_cols, 1000)))
        }).clone()
    }

    /// Get parser without creating (for rendering)
    pub fn parser_for(&self, window_id: NodeId) -> Option<Arc<Mutex<vt100::Parser>>> {
        self.parsers.get(&window_id).cloned()
    }

    pub fn apply_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::TabState { projects, groups, windows, active_project, active_group, active_window, layout_mode, tile_layout, tiled_windows, pane_weights } => {
                self.projects = projects;
                self.groups = groups;
                self.windows = windows;
                self.active_project = active_project;
                self.active_group = active_group;
                self.active_window = active_window;
                self.layout_mode = layout_mode;
                self.tile_layout = tile_layout;
                self.tiled_windows = tiled_windows;
                self.pane_weights = pane_weights.into_iter().map(|(id, w, h)| (id, (w, h))).collect();

                // Clean up parsers for windows that no longer exist
                let window_ids: Vec<NodeId> = self.windows.iter().map(|e| e.id).collect();
                self.parsers.retain(|id, _| window_ids.contains(id));
            }
            ServerMsg::ScreenDump { window_id, data } => {
                let parser = self.get_parser(window_id);
                let mut parser = parser.lock().unwrap();
                parser.process(b"\x1b[2J\x1b[H");
                parser.process(&data);
            }
            ServerMsg::PtyOutput { window_id, data } => {
                let parser = self.get_parser(window_id);
                parser.lock().unwrap().process(&data);
            }
            ServerMsg::Info { message } => {
                self.status_message = Some((message, Instant::now()));
            }
            ServerMsg::BranchList { branches } => {
                self.branch_candidates = branches;
                self.branch_selected = None;
            }
            ServerMsg::PresetList { presets } => {
                self.preset_candidates = presets;
                self.preset_selected = None;
            }
            ServerMsg::WindowCreated { .. } => {}
            ServerMsg::Error { message } => {
                self.status_message = Some((format!("Error: {}", message), Instant::now()));
            }
            ServerMsg::Reloading => {
                self.status_message = Some(("Server reloading...".to_string(), Instant::now()));
                self.should_reconnect = true;
            }
            ServerMsg::FullTree { projects, active_project, active_group, active_window } => {
                // Build preview parsers from screen data
                self.tree_parsers.clear();
                for proj in &projects {
                    for grp in &proj.groups {
                        for win in &grp.windows {
                            if !win.screen_data.is_empty() {
                                let parser = Arc::new(Mutex::new(
                                    vt100::Parser::new(self.term_rows, self.term_cols, 0),
                                ));
                                parser.lock().unwrap().process(&win.screen_data);
                                self.tree_parsers.insert(win.id, parser);
                            }
                        }
                    }
                }
                self.tree_data = projects;
                self.tree_active_project = active_project;
                self.tree_active_group = active_group;
                self.tree_active_window = active_window;
                // Position cursor on the currently active item
                self.tree_cursor = self.tree_find_active_index();
            }
        }
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if self.last_size != (cols, rows) {
            self.last_size = (cols, rows);
            self.term_rows = rows.saturating_sub(1);
            self.term_cols = cols;
            // Resize all existing parsers
            for parser in self.parsers.values() {
                parser.lock().unwrap().set_size(self.term_rows, cols);
            }
            self.conn.send_resize(cols, rows).await?;
        }
        Ok(())
    }

    pub fn is_tiled(&self) -> bool {
        self.layout_mode == LayoutMode::Tiled && !self.tiled_windows.is_empty()
    }

    // Tab navigation helpers

    pub async fn next_tab(&mut self) -> Result<()> {
        match self.tab_focus {
            TabLevel::Project => {
                if let Some(active) = self.active_project {
                    if let Some(idx) = self.projects.iter().position(|e| e.id == active) {
                        let new_idx = (idx + 1) % self.projects.len();
                        self.conn.select_project(self.projects[new_idx].id).await?;
                    }
                }
            }
            TabLevel::Group => {
                if let Some(active) = self.active_group {
                    if let Some(idx) = self.groups.iter().position(|e| e.id == active) {
                        let new_idx = (idx + 1) % self.groups.len();
                        self.conn.select_group(self.groups[new_idx].id).await?;
                    }
                }
            }
            TabLevel::Window => {
                if let Some(active) = self.active_window {
                    if let Some(idx) = self.windows.iter().position(|e| e.id == active) {
                        let new_idx = (idx + 1) % self.windows.len();
                        self.conn.select_window(self.windows[new_idx].id).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn prev_tab(&mut self) -> Result<()> {
        match self.tab_focus {
            TabLevel::Project => {
                if let Some(active) = self.active_project {
                    if let Some(idx) = self.projects.iter().position(|e| e.id == active) {
                        let len = self.projects.len();
                        let new_idx = (idx + len - 1) % len;
                        self.conn.select_project(self.projects[new_idx].id).await?;
                    }
                }
            }
            TabLevel::Group => {
                if let Some(active) = self.active_group {
                    if let Some(idx) = self.groups.iter().position(|e| e.id == active) {
                        let len = self.groups.len();
                        let new_idx = (idx + len - 1) % len;
                        self.conn.select_group(self.groups[new_idx].id).await?;
                    }
                }
            }
            TabLevel::Window => {
                if let Some(active) = self.active_window {
                    if let Some(idx) = self.windows.iter().position(|e| e.id == active) {
                        let len = self.windows.len();
                        let new_idx = (idx + len - 1) % len;
                        self.conn.select_window(self.windows[new_idx].id).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Get branch candidates filtered by current input.
    pub fn filtered_branches(&self) -> Vec<&str> {
        let query = self.rename_buf.to_lowercase();
        self.branch_candidates
            .iter()
            .filter(|b| query.is_empty() || b.to_lowercase().contains(&query))
            .map(|b| b.as_str())
            .collect()
    }

    /// Get preset candidates filtered by current input.
    pub fn filtered_presets(&self) -> Vec<&str> {
        let query = self.rename_buf.to_lowercase();
        self.preset_candidates
            .iter()
            .filter(|p| query.is_empty() || p.to_lowercase().contains(&query))
            .map(|p| p.as_str())
            .collect()
    }

    pub async fn select_tab_by_index(&mut self, index: usize) -> Result<()> {
        match self.tab_focus {
            TabLevel::Project => {
                if let Some(entry) = self.projects.get(index) {
                    self.conn.select_project(entry.id).await?;
                }
            }
            TabLevel::Group => {
                if let Some(entry) = self.groups.get(index) {
                    self.conn.select_group(entry.id).await?;
                }
            }
            TabLevel::Window => {
                if let Some(entry) = self.windows.get(index) {
                    self.conn.select_window(entry.id).await?;
                }
            }
        }
        Ok(())
    }

    /// Build a flattened list of visible tree items, respecting collapsed state
    pub fn tree_visible_items(&self) -> Vec<TreeItem> {
        let mut items = Vec::new();
        for proj in &self.tree_data {
            let proj_expanded = !self.tree_collapsed_projects.contains(&proj.id);
            items.push(TreeItem::Project {
                id: proj.id,
                name: proj.name.clone(),
                expanded: proj_expanded,
            });
            if proj_expanded {
                for grp in &proj.groups {
                    let grp_expanded = !self.tree_collapsed_groups.contains(&grp.id);
                    items.push(TreeItem::Group {
                        id: grp.id,
                        name: grp.name.clone(),
                        expanded: grp_expanded,
                    });
                    if grp_expanded {
                        for win in &grp.windows {
                            items.push(TreeItem::Window {
                                id: win.id,
                                name: win.name.clone(),
                                ai_status: win.ai_status.clone(),
                            });
                        }
                    }
                }
            }
        }
        items
    }

    /// Find the index in the visible items list that corresponds to the active window
    fn tree_find_active_index(&self) -> usize {
        let items = self.tree_visible_items();
        // Try to find active window first, then group, then project
        if let Some(wid) = self.tree_active_window {
            if let Some(idx) = items.iter().position(|item| matches!(item, TreeItem::Window { id, .. } if *id == wid)) {
                return idx;
            }
        }
        if let Some(gid) = self.tree_active_group {
            if let Some(idx) = items.iter().position(|item| matches!(item, TreeItem::Group { id, .. } if *id == gid)) {
                return idx;
            }
        }
        if let Some(pid) = self.tree_active_project {
            if let Some(idx) = items.iter().position(|item| matches!(item, TreeItem::Project { id, .. } if *id == pid)) {
                return idx;
            }
        }
        0
    }

    /// Get the window ID under the tree cursor, if any
    pub fn tree_cursor_window_id(&self) -> Option<NodeId> {
        let items = self.tree_visible_items();
        match items.get(self.tree_cursor) {
            Some(TreeItem::Window { id, .. }) => Some(*id),
            _ => None,
        }
    }

    /// Find the parent group ID for a window in tree_data
    pub fn tree_parent_group(&self, window_id: NodeId) -> Option<NodeId> {
        for proj in &self.tree_data {
            for grp in &proj.groups {
                for win in &grp.windows {
                    if win.id == window_id {
                        return Some(grp.id);
                    }
                }
            }
        }
        None
    }

    /// Find the parent project ID for a group in tree_data
    pub fn tree_parent_project(&self, group_id: NodeId) -> Option<NodeId> {
        for proj in &self.tree_data {
            for grp in &proj.groups {
                if grp.id == group_id {
                    return Some(proj.id);
                }
            }
        }
        None
    }

    /// Get the name of a node in tree_data
    pub fn tree_node_name(&self, id: NodeId) -> Option<String> {
        for proj in &self.tree_data {
            if proj.id == id { return Some(proj.name.clone()); }
            for grp in &proj.groups {
                if grp.id == id { return Some(grp.name.clone()); }
                for win in &grp.windows {
                    if win.id == id { return Some(win.name.clone()); }
                }
            }
        }
        None
    }
}
