use crate::client::ClientConnection;
use crate::protocol::{LayoutMode, NodeId, ServerMsg, TabEntry, TileLayout};
use anyhow::Result;
use std::collections::HashMap;
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
    Help,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TabLevel {
    Project,
    Group,
    Window,
}

pub struct App {
    pub conn: ClientConnection,
    pub should_quit: bool,
    pub should_detach: bool,
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

    // Client-side vt100 parsers keyed by window ID
    pub parsers: HashMap<NodeId, Arc<Mutex<vt100::Parser>>>,
    /// Default terminal size for creating new parsers
    pub term_rows: u16,
    pub term_cols: u16,

    // Rename input buffer
    pub rename_buf: String,
    pub rename_target: Option<NodeId>,

    // Copy mode scroll offset
    pub copy_scroll_offset: usize,

    // Status message shown briefly in the tab bar
    pub status_message: Option<(String, Instant)>,

    // Branch picker state
    pub branch_candidates: Vec<String>,
    pub branch_selected: Option<usize>,
}

impl App {
    pub async fn new(conn: ClientConnection, rows: u16, cols: u16) -> Result<Self> {
        conn.send_resize(cols, rows).await?;
        let term_rows = rows.saturating_sub(1);
        Ok(App {
            conn,
            should_quit: false,
            should_detach: false,
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
            parsers: HashMap::new(),
            term_rows,
            term_cols: cols,
            rename_buf: String::new(),
            rename_target: None,
            copy_scroll_offset: 0,
            status_message: None,
            branch_candidates: Vec::new(),
            branch_selected: None,
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
            ServerMsg::TabState { projects, groups, windows, active_project, active_group, active_window, layout_mode, tile_layout, tiled_windows } => {
                self.projects = projects;
                self.groups = groups;
                self.windows = windows;
                self.active_project = active_project;
                self.active_group = active_group;
                self.active_window = active_window;
                self.layout_mode = layout_mode;
                self.tile_layout = tile_layout;
                self.tiled_windows = tiled_windows;

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
            ServerMsg::WindowCreated { .. } => {}
            ServerMsg::Error { message } => {
                self.status_message = Some((format!("Error: {}", message), Instant::now()));
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
}
