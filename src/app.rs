use crate::client::ClientConnection;
use crate::protocol::{NodeId, ServerMsg, TabEntry};
use anyhow::Result;
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

    // Client-side vt100 parser for the active window
    pub parser: Arc<Mutex<vt100::Parser>>,

    // Rename input buffer
    pub rename_buf: String,
    pub rename_target: Option<NodeId>,

    // Copy mode scroll offset
    pub copy_scroll_offset: usize,

    // Status message shown briefly in the tab bar
    pub status_message: Option<(String, Instant)>,
}

impl App {
    pub async fn new(conn: ClientConnection, rows: u16, cols: u16) -> Result<Self> {
        conn.send_resize(cols, rows).await?;
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
            parser: Arc::new(Mutex::new(vt100::Parser::new(rows.saturating_sub(1), cols, 1000))),
            rename_buf: String::new(),
            rename_target: None,
            copy_scroll_offset: 0,
            status_message: None,
        })
    }

    pub fn apply_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::TabState { projects, groups, windows, active_project, active_group, active_window } => {
                self.projects = projects;
                self.groups = groups;
                self.windows = windows;
                self.active_project = active_project;
                self.active_group = active_group;
                self.active_window = active_window;
            }
            ServerMsg::ScreenDump { window_id: _, data } => {
                let mut parser = self.parser.lock().unwrap();
                // Clear and process the screen dump
                parser.process(b"\x1b[2J\x1b[H");
                parser.process(&data);
            }
            ServerMsg::PtyOutput { window_id: _, data } => {
                self.parser.lock().unwrap().process(&data);
            }
            ServerMsg::Info { message } => {
                self.status_message = Some((message, Instant::now()));
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
            self.parser.lock().unwrap().set_size(rows.saturating_sub(1), cols);
            self.conn.send_resize(cols, rows).await?;
        }
        Ok(())
    }

    // Tab navigation helpers (client-side, send commands to server)

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
