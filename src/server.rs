use crate::ai_detect::{self, AiStatus};
use crate::config;
use crate::protocol::{self, ClientMsg, LayoutMode, NodeId, PaneDirection, ServerMsg, TabEntry, TileLayout};
use crate::pty::PtyHandle;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

// ── Server-side session tree ──

struct SessionTree {
    nodes: HashMap<NodeId, Node>,
    root_children: Vec<NodeId>,
    next_id: NodeId,
    active_project: Option<NodeId>,
    active_group: Option<NodeId>,
    active_window: Option<NodeId>,
    shell: Option<String>,
}

struct ProjectNode {
    name: String,
    working_dir: PathBuf,
    children: Vec<NodeId>,
}

struct GroupNode {
    name: String,
    parent: NodeId,
    children: Vec<NodeId>,
    working_dir: Option<PathBuf>,
    /// If this group was created from a git worktree, track it for cleanup
    worktree_path: Option<PathBuf>,
    /// Layout mode: stacked (one visible) or tiled (multiple visible)
    layout_mode: LayoutMode,
    /// Current tile layout algorithm
    tile_layout: TileLayout,
    /// Windows marked for tiling (subset of children)
    tiled_windows: Vec<NodeId>,
    /// Per-window size weights for tiled layout (width_weight, height_weight), default (1.0, 1.0)
    pane_weights: HashMap<NodeId, (f64, f64)>,
}

struct WindowNode {
    name: String,
    #[allow(dead_code)]
    parent: NodeId,
    pty: PtyHandle,
    ai_status: Option<AiStatus>,
    last_cpu_time: u64,
}

enum Node {
    Project(ProjectNode),
    Group(GroupNode),
    Window(WindowNode),
}

impl SessionTree {
    fn new() -> Self {
        SessionTree {
            nodes: HashMap::new(),
            root_children: Vec::new(),
            next_id: 1,
            active_project: None,
            active_group: None,
            active_window: None,
            shell: None,
        }
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn add_project(&mut self, name: String, working_dir: PathBuf) -> NodeId {
        let id = self.alloc_id();
        self.nodes.insert(id, Node::Project(ProjectNode {
            name, working_dir, children: Vec::new(),
        }));
        self.root_children.push(id);
        if self.active_project.is_none() {
            self.active_project = Some(id);
        }
        id
    }

    fn add_group(&mut self, parent: NodeId, name: String, working_dir: Option<PathBuf>, worktree_path: Option<PathBuf>) -> NodeId {
        let id = self.alloc_id();
        self.nodes.insert(id, Node::Group(GroupNode {
            name, parent, children: Vec::new(), working_dir, worktree_path,
            layout_mode: LayoutMode::Stacked, tile_layout: TileLayout::EqualColumns,
            tiled_windows: Vec::new(),
            pane_weights: HashMap::new(),
        }));
        if let Some(Node::Project(p)) = self.nodes.get_mut(&parent) {
            p.children.push(id);
        }
        if self.active_group.is_none() {
            self.active_group = Some(id);
        }
        id
    }

    fn add_window(
        &mut self,
        parent: NodeId,
        name: String,
        rows: u16,
        cols: u16,
        pty_output_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
    ) -> Result<NodeId> {
        let id = self.alloc_id();
        let working_dir = self.window_working_dir(parent);

        // Collect .env vars: project dir first, then group dir overlays
        let mut env = HashMap::new();
        if let Some(Node::Group(g)) = self.nodes.get(&parent) {
            if let Some(Node::Project(p)) = self.nodes.get(&g.parent) {
                env.extend(config::parse_dotenv(&p.working_dir));
            }
            if let Some(ref wd) = g.working_dir {
                env.extend(config::parse_dotenv(wd));
            }
        }

        let (pty, mut pty_rx) = PtyHandle::spawn_in(rows, cols, &working_dir, &env, self.shell.as_deref())?;

        // Forward raw PTY bytes with window ID
        let win_id = id;
        tokio::spawn(async move {
            while let Some(bytes) = pty_rx.recv().await {
                if pty_output_tx.send((win_id, bytes)).is_err() {
                    break;
                }
            }
            // PTY exited — send empty sentinel to trigger window removal
            let _ = pty_output_tx.send((win_id, Vec::new()));
        });

        self.nodes.insert(id, Node::Window(WindowNode { name, parent, pty, ai_status: None, last_cpu_time: 0 }));
        if let Some(Node::Group(g)) = self.nodes.get_mut(&parent) {
            g.children.push(id);
        }
        if self.active_window.is_none() {
            self.active_window = Some(id);
        }
        Ok(id)
    }

    fn remove_window(&mut self, window_id: NodeId) {
        // Get parent group before removing the node
        let parent_id = if let Some(Node::Window(w)) = self.nodes.get(&window_id) {
            Some(w.parent)
        } else {
            None
        };

        self.nodes.remove(&window_id);

        // Remove from parent group's children and tiled set
        if let Some(pid) = parent_id {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&pid) {
                g.children.retain(|id| *id != window_id);
                g.tiled_windows.retain(|id| *id != window_id);
            }
        }

        // If this was the active window, select a sibling
        if self.active_window == Some(window_id) {
            self.active_window = parent_id.and_then(|pid| {
                if let Some(Node::Group(g)) = self.nodes.get(&pid) {
                    g.children.first().copied()
                } else {
                    None
                }
            });
        }
    }

    /// Remove a group and all its windows. Returns (project_dir, worktree_path) if worktree cleanup needed.
    fn remove_group(&mut self, group_id: NodeId) -> Option<(PathBuf, PathBuf)> {
        let (parent_id, window_ids, worktree_info) = match self.nodes.get(&group_id) {
            Some(Node::Group(g)) => {
                let wt_info = g.worktree_path.as_ref().and_then(|wt| {
                    if let Some(Node::Project(p)) = self.nodes.get(&g.parent) {
                        Some((p.working_dir.clone(), wt.clone()))
                    } else {
                        None
                    }
                });
                (g.parent, g.children.clone(), wt_info)
            }
            _ => return None,
        };

        // Remove all windows in the group
        for wid in &window_ids {
            self.nodes.remove(wid);
        }
        self.nodes.remove(&group_id);

        // Remove from parent project's children
        if let Some(Node::Project(p)) = self.nodes.get_mut(&parent_id) {
            p.children.retain(|id| *id != group_id);
        }

        // If the parent project is now empty, remove it and select a sibling project
        let project_empty = matches!(self.nodes.get(&parent_id), Some(Node::Project(p)) if p.children.is_empty());
        if project_empty {
            self.nodes.remove(&parent_id);
            self.root_children.retain(|id| *id != parent_id);

            if self.active_project == Some(parent_id) {
                self.active_project = self.root_children.first().copied();
                if let Some(pid) = self.active_project {
                    self.select_project(pid);
                } else {
                    self.active_group = None;
                    self.active_window = None;
                }
            }
        } else if self.active_group == Some(group_id) {
            // Parent project still has groups, select a sibling
            if let Some(Node::Project(p)) = self.nodes.get(&parent_id) {
                self.active_group = p.children.first().copied();
                self.active_window = self.active_group.and_then(|gid| {
                    if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                        g.children.first().copied()
                    } else {
                        None
                    }
                });
            } else {
                self.active_group = None;
                self.active_window = None;
            }
        } else if window_ids.contains(&self.active_window.unwrap_or(0)) {
            self.active_window = None;
        }

        worktree_info
    }

    fn move_window_to_group(&mut self, window_id: NodeId, new_group_id: NodeId) {
        // Remove from old parent
        if let Some(Node::Window(w)) = self.nodes.get(&window_id) {
            let old_parent = w.parent;
            if let Some(Node::Group(g)) = self.nodes.get_mut(&old_parent) {
                g.children.retain(|id| *id != window_id);
            }
        }
        // Update parent and add to new group
        if let Some(Node::Window(w)) = self.nodes.get_mut(&window_id) {
            w.parent = new_group_id;
        }
        if let Some(Node::Group(g)) = self.nodes.get_mut(&new_group_id) {
            g.children.push(window_id);
        }
    }

    fn window_cwd(&self, window_id: NodeId) -> Option<PathBuf> {
        if let Some(Node::Window(w)) = self.nodes.get(&window_id) {
            w.pty.cwd()
        } else {
            None
        }
    }

    fn window_working_dir(&self, group_id: NodeId) -> PathBuf {
        if let Some(Node::Group(g)) = self.nodes.get(&group_id) {
            if let Some(ref wd) = g.working_dir {
                return wd.clone();
            }
            if let Some(Node::Project(p)) = self.nodes.get(&g.parent) {
                return p.working_dir.clone();
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    }

    fn tab_state(&self) -> ServerMsg {
        let projects: Vec<TabEntry> = self.root_children.iter().filter_map(|id| {
            match self.nodes.get(id) {
                Some(Node::Project(p)) => Some(TabEntry { id: *id, name: p.name.clone(), ai_status: None }),
                _ => None,
            }
        }).collect();

        let groups: Vec<TabEntry> = if let Some(pid) = self.active_project {
            if let Some(Node::Project(p)) = self.nodes.get(&pid) {
                p.children.iter().filter_map(|id| match self.nodes.get(id) {
                    Some(Node::Group(g)) => Some(TabEntry { id: *id, name: g.name.clone(), ai_status: None }),
                    _ => None,
                }).collect()
            } else { Vec::new() }
        } else { Vec::new() };

        let windows: Vec<TabEntry> = if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                g.children.iter().filter_map(|id| match self.nodes.get(id) {
                    Some(Node::Window(w)) => Some(TabEntry {
                        id: *id,
                        name: w.name.clone(),
                        ai_status: w.ai_status.clone(),
                    }),
                    _ => None,
                }).collect()
            } else { Vec::new() }
        } else { Vec::new() };

        let (layout_mode, tile_layout, tiled_windows, pane_weights) = if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                let weights: Vec<(NodeId, f64, f64)> = g.pane_weights.iter().map(|(&id, &(w, h))| (id, w, h)).collect();
                (g.layout_mode, g.tile_layout, g.tiled_windows.clone(), weights)
            } else {
                (LayoutMode::Stacked, TileLayout::EqualColumns, Vec::new(), Vec::new())
            }
        } else {
            (LayoutMode::Stacked, TileLayout::EqualColumns, Vec::new(), Vec::new())
        };

        ServerMsg::TabState {
            projects, groups, windows,
            active_project: self.active_project,
            active_group: self.active_group,
            active_window: self.active_window,
            layout_mode, tile_layout, tiled_windows, pane_weights,
        }
    }

    fn select_project(&mut self, id: NodeId) {
        self.active_project = Some(id);
        if let Some(Node::Project(p)) = self.nodes.get(&id) {
            let first_group = p.children.first().copied();
            self.active_group = first_group;
            if let Some(gid) = first_group {
                if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                    self.active_window = g.children.first().copied();
                } else { self.active_window = None; }
            } else { self.active_window = None; }
        }
    }

    fn select_group(&mut self, id: NodeId) {
        self.active_group = Some(id);
        if let Some(Node::Group(g)) = self.nodes.get(&id) {
            self.active_window = g.children.first().copied();
        } else { self.active_window = None; }
    }

    fn select_window(&mut self, id: NodeId) {
        self.active_window = Some(id);
    }

    fn active_window_mut(&mut self) -> Option<&mut WindowNode> {
        let id = self.active_window?;
        match self.nodes.get_mut(&id) {
            Some(Node::Window(w)) => Some(w),
            _ => None,
        }
    }

    /// Poll all windows for AI tool processes. Returns true if any status changed.
    fn poll_ai_status(&mut self) -> bool {
        let window_ids: Vec<NodeId> = self.nodes.iter().filter_map(|(id, node)| {
            matches!(node, Node::Window(_)).then_some(*id)
        }).collect();

        let mut changed = false;
        for wid in window_ids {
            if let Some(Node::Window(w)) = self.nodes.get_mut(&wid) {
                let pid = match w.pty.child_pid {
                    Some(p) => p,
                    None => continue,
                };
                let (new_status, new_cpu_time) = ai_detect::detect(pid, w.ai_status.as_ref(), w.last_cpu_time);
                if new_status != w.ai_status {
                    w.ai_status = new_status;
                    changed = true;
                }
                w.last_cpu_time = new_cpu_time;
            }
        }
        changed
    }

    /// Get all window IDs that have an AI session, in a stable order (by project/group/window).
    fn ai_window_ids(&self) -> Vec<(NodeId, NodeId, NodeId)> {
        // Returns (project_id, group_id, window_id) tuples
        let mut result = Vec::new();
        for &pid in &self.root_children {
            if let Some(Node::Project(p)) = self.nodes.get(&pid) {
                for &gid in &p.children {
                    if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                        for &wid in &g.children {
                            if let Some(Node::Window(w)) = self.nodes.get(&wid) {
                                if w.ai_status.is_some() {
                                    result.push((pid, gid, wid));
                                }
                            }
                        }
                    }
                }
            }
        }
        result
    }

    /// Navigate to the next/prev AI window across all projects/groups.
    /// Returns true if navigation happened.
    fn cycle_ai_window(&mut self, forward: bool) -> bool {
        let ai_windows = self.ai_window_ids();
        if ai_windows.is_empty() {
            return false;
        }

        let current = self.active_window;
        let current_idx = current.and_then(|wid| {
            ai_windows.iter().position(|(_, _, w)| *w == wid)
        });

        let next_idx = match current_idx {
            Some(idx) => {
                if forward {
                    (idx + 1) % ai_windows.len()
                } else {
                    (idx + ai_windows.len() - 1) % ai_windows.len()
                }
            }
            None => 0, // Jump to first AI window
        };

        let (pid, gid, wid) = ai_windows[next_idx];
        self.active_project = Some(pid);
        self.active_group = Some(gid);
        self.active_window = Some(wid);
        true
    }

    /// Search all windows' screen content for a query string (case-insensitive).
    /// Returns (project_id, group_id, window_id, window_name) of first match.
    fn search_windows(&self, query: &str) -> Option<(NodeId, NodeId, NodeId, String)> {
        let query_lower = query.to_lowercase();
        for &pid in &self.root_children {
            if let Some(Node::Project(p)) = self.nodes.get(&pid) {
                for &gid in &p.children {
                    if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                        for &wid in &g.children {
                            if let Some(Node::Window(w)) = self.nodes.get(&wid) {
                                let parser = w.pty.parser.lock().unwrap();
                                let screen = parser.screen();
                                let (rows, cols) = screen.size();
                                let mut text = String::new();
                                for row in 0..rows {
                                    for col in 0..cols {
                                        if let Some(cell) = screen.cell(row, col) {
                                            let c = cell.contents();
                                            if c.is_empty() {
                                                text.push(' ');
                                            } else {
                                                text.push_str(&c);
                                            }
                                        }
                                    }
                                    text.push('\n');
                                }
                                if text.to_lowercase().contains(&query_lower) {
                                    return Some((pid, gid, wid, w.name.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn toggle_layout(&mut self) {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
                g.layout_mode = match g.layout_mode {
                    LayoutMode::Stacked => {
                        // Auto-tile active window if nothing is tiled
                        if g.tiled_windows.is_empty() {
                            if let Some(wid) = self.active_window {
                                if g.children.contains(&wid) {
                                    g.tiled_windows.push(wid);
                                }
                            }
                        }
                        LayoutMode::Tiled
                    }
                    LayoutMode::Tiled => LayoutMode::Stacked,
                };
            }
        }
    }

    fn cycle_layout(&mut self) {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
                g.tile_layout = g.tile_layout.next();
            }
        }
    }

    fn toggle_tile(&mut self, window_id: NodeId) {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
                if !g.children.contains(&window_id) {
                    return;
                }
                if let Some(pos) = g.tiled_windows.iter().position(|&id| id == window_id) {
                    g.tiled_windows.remove(pos);
                } else {
                    g.tiled_windows.push(window_id);
                }
            }
        }
    }

    fn focus_pane(&mut self, direction: PaneDirection) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        let (tiled, tile_layout) = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled && !g.tiled_windows.is_empty() => {
                (g.tiled_windows.clone(), g.tile_layout)
            }
            _ => return,
        };
        let active = match self.active_window {
            Some(wid) => wid,
            None => return,
        };
        let idx = match tiled.iter().position(|&id| id == active) {
            Some(i) => i,
            None => return,
        };
        let count = tiled.len();

        let new_idx = match tile_layout {
            TileLayout::EqualColumns => match direction {
                PaneDirection::Left => if idx > 0 { idx - 1 } else { idx },
                PaneDirection::Right => if idx + 1 < count { idx + 1 } else { idx },
                _ => idx,
            },
            TileLayout::EqualRows => match direction {
                PaneDirection::Up => if idx > 0 { idx - 1 } else { idx },
                PaneDirection::Down => if idx + 1 < count { idx + 1 } else { idx },
                _ => idx,
            },
            TileLayout::MainLeft => match direction {
                PaneDirection::Left => 0,
                PaneDirection::Right => if idx == 0 && count > 1 { 1 } else { idx },
                PaneDirection::Up => if idx > 1 { idx - 1 } else { idx },
                PaneDirection::Down => if idx >= 1 && idx + 1 < count { idx + 1 } else { idx },
            },
            TileLayout::Grid => {
                let cols_count = (count as f64).sqrt().ceil() as usize;
                let row = idx / cols_count;
                let col = idx % cols_count;
                match direction {
                    PaneDirection::Left => if col > 0 { idx - 1 } else { idx },
                    PaneDirection::Right => if col + 1 < cols_count && idx + 1 < count { idx + 1 } else { idx },
                    PaneDirection::Up => if row > 0 { idx - cols_count } else { idx },
                    PaneDirection::Down => if idx + cols_count < count { idx + cols_count } else { idx },
                }
            }
        };

        self.active_window = Some(tiled[new_idx]);
    }

    fn resize_pane(&mut self, direction: PaneDirection) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        let wid = match self.active_window {
            Some(wid) => wid,
            None => return,
        };
        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if g.layout_mode != LayoutMode::Tiled || !g.tiled_windows.contains(&wid) {
                return;
            }
            let (w, h) = g.pane_weights.entry(wid).or_insert((1.0, 1.0));
            match direction {
                PaneDirection::Left => *w = (*w - 0.1).max(0.2),
                PaneDirection::Right => *w = (*w + 0.1).min(5.0),
                PaneDirection::Up => *h = (*h + 0.1).min(5.0),
                PaneDirection::Down => *h = (*h - 0.1).max(0.2),
            }
        }
    }

    /// Returns true if the active group is in tiled mode and a window is tiled
    fn is_tiled_window(&self, window_id: NodeId) -> bool {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                return g.layout_mode == LayoutMode::Tiled && g.tiled_windows.contains(&window_id);
            }
        }
        false
    }

    fn active_tiled_windows(&self) -> Vec<NodeId> {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled {
                    return g.tiled_windows.clone();
                }
            }
        }
        Vec::new()
    }

    fn resize_all(&mut self, rows: u16, cols: u16) -> Result<()> {
        // Get tiled pane sizes if in tiled mode
        let tiled_sizes = if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled && !g.tiled_windows.is_empty() {
                    Some(pane_sizes(g.tile_layout, &g.tiled_windows, rows, cols, &g.pane_weights))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        for (id, node) in self.nodes.iter() {
            if let Node::Window(w) = node {
                if let Some(ref sizes) = tiled_sizes {
                    if let Some((_, r, c)) = sizes.iter().find(|(wid, _, _)| wid == id) {
                        w.pty.resize(*r, *c)?;
                        continue;
                    }
                }
                w.pty.resize(rows, cols)?;
            }
        }
        Ok(())
    }

    /// Get the cwd of the active window's shell process
    fn active_window_cwd(&self) -> Option<PathBuf> {
        let id = self.active_window?;
        match self.nodes.get(&id) {
            Some(Node::Window(w)) => w.pty.cwd(),
            _ => None,
        }
    }

    /// Set the active project's working directory to the active window's cwd
    fn set_project_dir(&mut self) -> Option<String> {
        let cwd = self.active_window_cwd()?;
        let pid = self.active_project?;
        if let Some(Node::Project(p)) = self.nodes.get_mut(&pid) {
            p.working_dir = cwd.clone();
            Some(format!("Project dir: {}", cwd.display()))
        } else {
            None
        }
    }

    /// Set the active group's working directory to the active window's cwd
    fn set_group_dir(&mut self) -> Option<String> {
        let cwd = self.active_window_cwd()?;
        let gid = self.active_group?;
        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            g.working_dir = Some(cwd.clone());
            Some(format!("Group dir: {}", cwd.display()))
        } else {
            None
        }
    }

    /// Convert current session tree to a Preset for saving
    fn to_preset(&self) -> config::Preset {
        let projects = self.root_children.iter().filter_map(|pid| {
            let p = match self.nodes.get(pid) {
                Some(Node::Project(p)) => p,
                _ => return None,
            };
            let groups = p.children.iter().filter_map(|gid| {
                let g = match self.nodes.get(gid) {
                    Some(Node::Group(g)) => g,
                    _ => return None,
                };
                let windows = g.children.iter().filter_map(|wid| {
                    match self.nodes.get(wid) {
                        Some(Node::Window(w)) => Some(config::WindowPreset {
                            name: w.name.clone(),
                            command: None,
                        }),
                        _ => None,
                    }
                }).collect();
                Some(config::GroupPreset {
                    name: g.name.clone(),
                    path: g.working_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                    worktree_branch: g.worktree_path.as_ref().and_then(|wt| {
                        wt.file_name().map(|n| n.to_string_lossy().to_string())
                    }),
                    windows,
                })
            }).collect();
            Some(config::ProjectPreset {
                name: p.name.clone(),
                path: p.working_dir.to_string_lossy().to_string(),
                groups,
            })
        }).collect();
        config::Preset { projects }
    }

    /// Get a screen dump for attach/reconnect: convert vt100 screen to ANSI bytes
    fn screen_dump(&self, window_id: NodeId) -> Option<Vec<u8>> {
        match self.nodes.get(&window_id) {
            Some(Node::Window(w)) => {
                let parser = w.pty.parser.lock().unwrap();
                Some(screen_to_ansi(parser.screen()))
            }
            _ => None,
        }
    }
}

/// Compute pane sizes for a tile layout. Returns (window_id, rows, cols) for each tiled window.
/// Accounts for 1-column borders between panes. Uses per-window weights for proportional sizing.
fn pane_sizes(layout: TileLayout, windows: &[NodeId], total_rows: u16, total_cols: u16, weights: &HashMap<NodeId, (f64, f64)>) -> Vec<(NodeId, u16, u16)> {
    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(windows[0], total_rows, total_cols)];
    }

    // Helper: distribute `usable` pixels among items proportional to their weights
    let distribute = |usable: u16, items: &[NodeId], get_weight: &dyn Fn(NodeId) -> f64| -> Vec<u16> {
        let total_weight: f64 = items.iter().map(|&id| get_weight(id)).sum();
        let mut sizes = Vec::with_capacity(items.len());
        let mut used: u16 = 0;
        for (i, &id) in items.iter().enumerate() {
            if i == items.len() - 1 {
                // Last item gets remainder to avoid rounding gaps
                sizes.push(usable.saturating_sub(used));
            } else {
                let s = ((usable as f64) * get_weight(id) / total_weight).round() as u16;
                sizes.push(s);
                used += s;
            }
        }
        sizes
    };

    let w_weight = |id: NodeId| -> f64 { weights.get(&id).map_or(1.0, |&(w, _)| w) };
    let h_weight = |id: NodeId| -> f64 { weights.get(&id).map_or(1.0, |&(_, h)| h) };

    match layout {
        TileLayout::EqualColumns => {
            let borders = (n - 1) as u16;
            let usable = total_cols.saturating_sub(borders);
            let widths = distribute(usable, windows, &w_weight);
            windows.iter().zip(widths).map(|(&wid, w)| (wid, total_rows, w)).collect()
        }
        TileLayout::EqualRows => {
            let borders = (n - 1) as u16;
            let usable = total_rows.saturating_sub(borders);
            let heights = distribute(usable, windows, &h_weight);
            windows.iter().zip(heights).map(|(&wid, h)| (wid, h, total_cols)).collect()
        }
        TileLayout::MainLeft => {
            // First window and side panes split width by weight
            let main_w = w_weight(windows[0]);
            let side_total_w: f64 = windows[1..].iter().map(|&id| w_weight(id)).sum::<f64>() / (n - 1) as f64;
            let total_w = main_w + side_total_w;
            let main_cols = ((total_cols.saturating_sub(1) as f64) * main_w / total_w).round() as u16;
            let side_cols = total_cols.saturating_sub(main_cols + 1);
            let mut result = vec![(windows[0], total_rows, main_cols)];
            let side_count = n - 1;
            let borders = if side_count > 1 { (side_count - 1) as u16 } else { 0 };
            let usable_h = total_rows.saturating_sub(borders);
            let side_windows = &windows[1..];
            let heights = distribute(usable_h, side_windows, &h_weight);
            for (&wid, h) in side_windows.iter().zip(heights) {
                result.push((wid, h, side_cols));
            }
            result
        }
        TileLayout::Grid => {
            let cols_count = (n as f64).sqrt().ceil() as usize;
            let rows_count = (n + cols_count - 1) / cols_count;
            let h_borders = if rows_count > 1 { (rows_count - 1) as u16 } else { 0 };
            let v_borders = if cols_count > 1 { (cols_count - 1) as u16 } else { 0 };
            let cell_h = total_rows.saturating_sub(h_borders) / rows_count as u16;
            let cell_w = total_cols.saturating_sub(v_borders) / cols_count as u16;
            let mut result = Vec::new();
            for &wid in windows {
                result.push((wid, cell_h, cell_w));
            }
            result
        }
    }
}

/// Convert a vt100 screen to ANSI escape sequences that reproduce it.
/// Used only for attach/reconnect, not for live streaming.
fn screen_to_ansi(screen: &vt100::Screen) -> Vec<u8> {
    let mut out = Vec::with_capacity(8192);
    out.extend_from_slice(b"\x1b[H\x1b[2J\x1b[0m");
    let (rows, cols) = (screen.size().0, screen.size().1);
    for row in 0..rows {
        // Position cursor at start of each row explicitly
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        for col in 0..cols {
            let cell = screen.cell(row, col).unwrap();
            let c = cell.contents();
            if c.is_empty() {
                out.push(b' ');
            } else {
                out.extend_from_slice(c.as_bytes());
            }
        }
    }
    let cursor = screen.cursor_position();
    out.extend_from_slice(format!("\x1b[{};{}H", cursor.0 + 1, cursor.1 + 1).as_bytes());
    out
}

// ── Server state ──

struct ServerState {
    session: SessionTree,
    client_tx: Option<mpsc::UnboundedSender<ServerMsg>>,
    term_size: (u16, u16),
    preset_name: Option<String>,
}

pub async fn run_server(preset_name: Option<&str>) -> Result<()> {
    let sock_path = protocol::socket_path();

    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)?;
    eprintln!("zmux server listening on {}", sock_path.display());

    let user_config = config::load_config();
    let mut session = SessionTree::new();
    session.shell = user_config.shell;
    let (pty_tx, mut pty_rx) = mpsc::unbounded_channel::<(NodeId, Vec<u8>)>();

    let default_rows: u16 = 24;
    let default_cols: u16 = 80;

    if let Some(name) = preset_name {
        let preset = config::load_preset(name)?;
        for proj_preset in &preset.projects {
            let project_id = session.add_project(proj_preset.name.clone(), PathBuf::from(&proj_preset.path));
            for grp_preset in &proj_preset.groups {
                let group_dir = grp_preset.path.as_ref().map(|p| PathBuf::from(p));
                let wt_path = grp_preset.worktree_branch.as_ref().and_then(|branch| {
                    let proj_dir = PathBuf::from(&proj_preset.path);
                    worktree::create(&proj_dir, branch).ok()
                });
                let working_dir = group_dir.or_else(|| wt_path.clone());
                let group_id = session.add_group(project_id, grp_preset.name.clone(), working_dir, wt_path);
                if grp_preset.windows.is_empty() {
                    session.add_window(group_id, "shell".to_string(), default_rows, default_cols, pty_tx.clone())?;
                } else {
                    for win_preset in &grp_preset.windows {
                        session.add_window(group_id, win_preset.name.clone(), default_rows, default_cols, pty_tx.clone())?;
                    }
                }
            }
            if proj_preset.groups.is_empty() {
                let group_id = session.add_group(project_id, "default".to_string(), None, None);
                session.add_window(group_id, "shell".to_string(), default_rows, default_cols, pty_tx.clone())?;
            }
        }
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let dir_name = cwd.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        let project_id = session.add_project(dir_name, cwd);
        let group_id = session.add_group(project_id, "default".to_string(), None, None);
        session.add_window(group_id, "shell".to_string(), default_rows, default_cols, pty_tx.clone())?;
    }

    let state = Arc::new(Mutex::new(ServerState {
        session,
        client_tx: None,
        term_size: (default_cols, default_rows),
        preset_name: preset_name.map(|s| s.to_string()),
    }));

    // PTY output forwarder: sends raw bytes as PtyOutput to connected client
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some((window_id, data)) = pty_rx.recv().await {
            if data.is_empty() {
                // Sentinel: PTY exited, remove the window
                let mut st = state_clone.lock().await;
                st.session.remove_window(window_id);
                if let Some(ref tx) = st.client_tx {
                    let tab = st.session.tab_state();
                    let _ = tx.send(tab);
                    // Send screen dump for the new active window
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                        }
                    }
                }
                continue;
            }
            let st = state_clone.lock().await;
            if let Some(ref tx) = st.client_tx {
                if st.session.active_window == Some(window_id) || st.session.is_tiled_window(window_id) {
                    let _ = tx.send(ServerMsg::PtyOutput { window_id, data });
                }
            }
        }
    });

    // AI status polling task — every 3 seconds
    let state_ai = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            interval.tick().await;
            let mut st = state_ai.lock().await;
            let changed = st.session.poll_ai_status();
            if changed {
                if let Some(ref tx) = st.client_tx {
                    let tab = st.session.tab_state();
                    let _ = tx.send(tab);
                }
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        let pty_tx = pty_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, state, pty_tx).await {
                eprintln!("Client error: {}", e);
            }
        });
    }
}

async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
    pty_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
) -> Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ServerMsg>();

    // Register as active client and send initial state
    {
        let mut st = state.lock().await;
        st.client_tx = Some(client_tx.clone());

        let tab_state = st.session.tab_state();
        let _ = client_tx.send(tab_state);

        // Send full screen dump for attach (client will feed it to its vt100 parser)
        if let Some(wid) = st.session.active_window {
            if let Some(data) = st.session.screen_dump(wid) {
                let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
            }
        }
    }

    // Writer task
    let writer_clone = Arc::clone(&writer);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            let mut w = writer_clone.lock().await;
            if protocol::write_msg(&mut *w, &msg).await.is_err() {
                break;
            }
        }
    });

    // Reader loop
    loop {
        let msg: Option<ClientMsg> = protocol::read_msg(&mut reader).await?;
        let msg = match msg {
            Some(m) => m,
            None => break,
        };

        let mut st = state.lock().await;
        match msg {
            ClientMsg::Input { data } => {
                if let Some(w) = st.session.active_window_mut() {
                    let _ = w.pty.write(&data);
                }
            }
            ClientMsg::Resize { cols, rows } => {
                st.term_size = (cols, rows);
                let term_rows = rows.saturating_sub(1);
                let _ = st.session.resize_all(term_rows, cols);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::SelectProject { id } => {
                st.session.select_project(id);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::SelectGroup { id } => {
                st.session.select_group(id);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::SelectWindow { id } => {
                st.session.select_window(id);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                if let Some(data) = st.session.screen_dump(id) {
                    let _ = client_tx.send(ServerMsg::ScreenDump { window_id: id, data });
                }
            }
            ClientMsg::NewWindow { name } => {
                if let Some(group_id) = st.session.active_group {
                    let (cols, rows) = st.term_size;
                    let term_rows = rows.saturating_sub(1);
                    let win_name = name.unwrap_or_else(|| {
                        let count = if let Some(Node::Group(g)) = st.session.nodes.get(&group_id) {
                            g.children.len()
                        } else { 0 };
                        format!("shell-{}", count + 1)
                    });
                    if let Ok(id) = st.session.add_window(group_id, win_name.clone(), term_rows, cols, pty_tx.clone()) {
                        st.session.select_window(id);
                        let tab = st.session.tab_state();
                        let _ = client_tx.send(tab);
                    }
                }
            }
            ClientMsg::NewGroup { name } => {
                if let Some(project_id) = st.session.active_project {
                    let grp_name = name.unwrap_or_else(|| {
                        if let Some(Node::Project(p)) = st.session.nodes.get(&project_id) {
                            format!("group-{}", p.children.len() + 1)
                        } else {
                            "group".to_string()
                        }
                    });
                    let group_id = st.session.add_group(project_id, grp_name, None, None);
                    let (cols, rows) = st.term_size;
                    let term_rows = rows.saturating_sub(1);
                    if let Ok(wid) = st.session.add_window(group_id, "shell".to_string(), term_rows, cols, pty_tx.clone()) {
                        st.session.select_group(group_id);
                        st.session.select_window(wid);
                        let tab = st.session.tab_state();
                        let _ = client_tx.send(tab);
                    }
                }
            }
            ClientMsg::NewProject { name } => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                let proj_name = name.unwrap_or_else(|| {
                    cwd.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "default".to_string())
                });
                let project_id = st.session.add_project(proj_name, cwd);
                let group_id = st.session.add_group(project_id, "default".to_string(), None, None);
                let (cols, rows) = st.term_size;
                let term_rows = rows.saturating_sub(1);
                if let Ok(wid) = st.session.add_window(group_id, "shell".to_string(), term_rows, cols, pty_tx.clone()) {
                    st.session.select_project(project_id);
                    st.session.select_window(wid);
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                }
            }
            ClientMsg::Subscribe => {
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::LoadPreset { name } => {
                match config::load_preset(&name) {
                    Ok(preset) => {
                        let (cols, rows) = st.term_size;
                        let term_rows = rows.saturating_sub(1);
                        for proj_preset in &preset.projects {
                            let project_id = st.session.add_project(
                                proj_preset.name.clone(), PathBuf::from(&proj_preset.path),
                            );
                            for grp_preset in &proj_preset.groups {
                                let group_dir = grp_preset.path.as_ref().map(|p| PathBuf::from(p));
                                let wt_path = grp_preset.worktree_branch.as_ref().and_then(|branch| {
                                    worktree::create(&PathBuf::from(&proj_preset.path), branch).ok()
                                });
                                let working_dir = group_dir.or_else(|| wt_path.clone());
                                let group_id = st.session.add_group(project_id, grp_preset.name.clone(), working_dir, wt_path);
                                if grp_preset.windows.is_empty() {
                                    let _ = st.session.add_window(group_id, "shell".to_string(), term_rows, cols, pty_tx.clone());
                                } else {
                                    for win_preset in &grp_preset.windows {
                                        let _ = st.session.add_window(group_id, win_preset.name.clone(), term_rows, cols, pty_tx.clone());
                                    }
                                }
                            }
                        }
                        let tab = st.session.tab_state();
                        let _ = client_tx.send(tab);
                    }
                    Err(e) => {
                        let _ = client_tx.send(ServerMsg::Error { message: format!("Failed to load preset: {}", e) });
                    }
                }
            }
            ClientMsg::SetProjectDir => {
                let msg = st.session.set_project_dir()
                    .unwrap_or_else(|| "No active window".to_string());
                let _ = client_tx.send(ServerMsg::Info { message: msg });
            }
            ClientMsg::SetGroupDir => {
                let msg = st.session.set_group_dir()
                    .unwrap_or_else(|| "No active window".to_string());
                let _ = client_tx.send(ServerMsg::Info { message: msg });
            }
            ClientMsg::SavePreset { name } => {
                let preset_name = name
                    .or_else(|| st.preset_name.clone())
                    .unwrap_or_else(|| {
                        // Derive from first project name
                        st.session.root_children.first()
                            .and_then(|id| match st.session.nodes.get(id) {
                                Some(Node::Project(p)) => Some(p.name.clone()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "default".to_string())
                    });
                let preset = st.session.to_preset();
                match config::save_preset(&preset_name, &preset) {
                    Ok(_) => {
                        st.preset_name = Some(preset_name.clone());
                        let _ = client_tx.send(ServerMsg::Info {
                            message: format!("Saved preset: {}", preset_name),
                        });
                    }
                    Err(e) => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Failed to save: {}", e),
                        });
                    }
                }
            }
            ClientMsg::NextAiWindow => {
                if st.session.cycle_ai_window(true) {
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info { message: "No AI sessions".to_string() });
                }
            }
            ClientMsg::PrevAiWindow => {
                if st.session.cycle_ai_window(false) {
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info { message: "No AI sessions".to_string() });
                }
            }
            ClientMsg::MoveWindowToNewProject => {
                if let Some(wid) = st.session.active_window {
                    let cwd = st.session.window_cwd(wid)
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
                    let proj_name = cwd.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "project".to_string());
                    let project_id = st.session.add_project(proj_name, cwd);
                    let group_id = st.session.add_group(project_id, "default".to_string(), None, None);
                    st.session.move_window_to_group(wid, group_id);
                    st.session.select_project(project_id);
                    st.session.active_group = Some(group_id);
                    st.session.active_window = Some(wid);
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                }
            }
            ClientMsg::MoveWindowToNewGroup => {
                if let (Some(wid), Some(project_id)) = (st.session.active_window, st.session.active_project) {
                    let cwd = st.session.window_cwd(wid)
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
                    let grp_name = cwd.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "group".to_string());
                    let group_id = st.session.add_group(project_id, grp_name, Some(cwd), None);
                    st.session.move_window_to_group(wid, group_id);
                    st.session.select_group(group_id);
                    st.session.active_window = Some(wid);
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                }
            }
            ClientMsg::Rename { id, name } => {
                match st.session.nodes.get_mut(&id) {
                    Some(Node::Project(p)) => p.name = name,
                    Some(Node::Group(g)) => g.name = name,
                    Some(Node::Window(w)) => w.name = name,
                    None => {}
                }
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
            }
            ClientMsg::CloseWindow => {
                if let Some(wid) = st.session.active_window {
                    st.session.remove_window(wid);
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                    if let Some(new_wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(new_wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump { window_id: new_wid, data });
                        }
                    }
                }
            }
            ClientMsg::RebaseMain => {
                if let Some(gid) = st.session.active_group {
                    let working_dir = st.session.window_working_dir(gid);
                    let output = std::process::Command::new("git")
                        .args(["rebase", "main"])
                        .current_dir(&working_dir)
                        .output();
                    match output {
                        Ok(o) if o.status.success() => {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            let msg = stdout.trim();
                            let _ = client_tx.send(ServerMsg::Info {
                                message: if msg.is_empty() { "Rebase complete".to_string() } else { msg.to_string() },
                            });
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Rebase failed: {}", stderr.trim()),
                            });
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Failed to run git: {}", e),
                            });
                        }
                    }
                }
            }
            ClientMsg::MergeIntoMain => {
                let (project_dir, branch) = if let Some(gid) = st.session.active_group {
                    match st.session.nodes.get(&gid) {
                        Some(Node::Group(g)) => {
                            let branch = g.worktree_path.as_ref()
                                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
                            let proj_dir = if let Some(Node::Project(p)) = st.session.nodes.get(&g.parent) {
                                Some(p.working_dir.clone())
                            } else { None };
                            (proj_dir, branch)
                        }
                        _ => (None, None),
                    }
                } else { (None, None) };

                match (project_dir, branch) {
                    (Some(dir), Some(branch)) => {
                        let output = std::process::Command::new("git")
                            .args(["merge", &branch])
                            .current_dir(&dir)
                            .output();
                        match output {
                            Ok(o) if o.status.success() => {
                                let _ = client_tx.send(ServerMsg::Info {
                                    message: format!("Merged {} into main", branch),
                                });
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Merge failed: {}", stderr.trim()),
                                });
                            }
                            Err(e) => {
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Failed to run git: {}", e),
                                });
                            }
                        }
                    }
                    _ => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: "Not a worktree group".to_string(),
                        });
                    }
                }
            }
            ClientMsg::NewWorktreeGroup { branch } => {
                if let Some(project_id) = st.session.active_project {
                    let project_dir = match st.session.nodes.get(&project_id) {
                        Some(Node::Project(p)) => p.working_dir.clone(),
                        _ => {
                            let _ = client_tx.send(ServerMsg::Error { message: "No active project".to_string() });
                            continue;
                        }
                    };
                    if !worktree::is_git_repo(&project_dir) {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Not a git repo: {}", project_dir.display()),
                        });
                        continue;
                    }
                    match worktree::create(&project_dir, &branch) {
                        Ok(wt_path) => {
                            let group_id = st.session.add_group(
                                project_id, branch.clone(), Some(wt_path.clone()), Some(wt_path),
                            );
                            let (cols, rows) = st.term_size;
                            let term_rows = rows.saturating_sub(1);
                            if let Ok(wid) = st.session.add_window(group_id, "shell".to_string(), term_rows, cols, pty_tx.clone()) {
                                st.session.select_group(group_id);
                                st.session.select_window(wid);
                                let tab = st.session.tab_state();
                                let _ = client_tx.send(tab);
                                let _ = client_tx.send(ServerMsg::Info {
                                    message: format!("Worktree: {}", branch),
                                });
                            }
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Worktree failed: {}", e),
                            });
                        }
                    }
                }
            }
            ClientMsg::ListBranches => {
                if let Some(project_id) = st.session.active_project {
                    let project_dir = match st.session.nodes.get(&project_id) {
                        Some(Node::Project(p)) => p.working_dir.clone(),
                        _ => continue,
                    };
                    let branches = worktree::list_branches(&project_dir);
                    let _ = client_tx.send(ServerMsg::BranchList { branches });
                }
            }
            ClientMsg::ListPresets => {
                match config::list_presets() {
                    Ok(presets) => {
                        let _ = client_tx.send(ServerMsg::PresetList { presets });
                    }
                    Err(e) => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Failed to list presets: {}", e),
                        });
                    }
                }
            }
            ClientMsg::CloseGroup { force } => {
                if let Some(group_id) = st.session.active_group {
                    // Check for dirty worktree
                    let is_wt = matches!(
                        st.session.nodes.get(&group_id),
                        Some(Node::Group(g)) if g.worktree_path.is_some()
                    );
                    if is_wt && !force {
                        let dirty = match st.session.nodes.get(&group_id) {
                            Some(Node::Group(g)) => g.worktree_path.as_ref()
                                .map(|p| worktree::is_dirty(p)).unwrap_or(false),
                            _ => false,
                        };
                        if dirty {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: "Worktree has uncommitted changes. Use force to remove.".to_string(),
                            });
                            continue;
                        }
                    }
                    if let Some((project_dir, wt_path)) = st.session.remove_group(group_id) {
                        if let Err(e) = worktree::remove(&project_dir, &wt_path, force) {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Worktree cleanup failed: {}", e),
                            });
                        }
                    }
                    let tab = st.session.tab_state();
                    let _ = client_tx.send(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                        }
                    }
                }
            }
            ClientMsg::SearchWindows { query } => {
                match st.session.search_windows(&query) {
                    Some((pid, gid, wid, name)) => {
                        st.session.active_project = Some(pid);
                        st.session.active_group = Some(gid);
                        st.session.active_window = Some(wid);
                        let tab = st.session.tab_state();
                        let _ = client_tx.send(tab);
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                        }
                        let _ = client_tx.send(ServerMsg::Info {
                            message: format!("Found in: {}", name),
                        });
                    }
                    None => {
                        let _ = client_tx.send(ServerMsg::Info {
                            message: "No match found".to_string(),
                        });
                    }
                }
            }
            ClientMsg::ToggleLayout => {
                st.session.toggle_layout();
                // Resize panes appropriately
                let (cols, rows) = st.term_size;
                let term_rows = rows.saturating_sub(1);
                let _ = st.session.resize_all(term_rows, cols);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                // Send screen dumps for all tiled windows
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
                // Also send for active window (in case stacked)
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::CycleLayout => {
                st.session.cycle_layout();
                let (cols, rows) = st.term_size;
                let term_rows = rows.saturating_sub(1);
                let _ = st.session.resize_all(term_rows, cols);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::ToggleTile { id } => {
                st.session.toggle_tile(id);
                let (cols, rows) = st.term_size;
                let term_rows = rows.saturating_sub(1);
                let _ = st.session.resize_all(term_rows, cols);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                    }
                }
            }
            ClientMsg::FocusPane { direction } => {
                st.session.focus_pane(direction);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
            }
            ClientMsg::ResizePane { direction } => {
                st.session.resize_pane(direction);
                let (cols, rows) = st.term_size;
                let term_rows = rows.saturating_sub(1);
                let _ = st.session.resize_all(term_rows, cols);
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                // Send screen dumps for all tiled windows
                if let Some(gid) = st.session.active_group {
                    if let Some(Node::Group(g)) = st.session.nodes.get(&gid) {
                        if g.layout_mode == LayoutMode::Tiled {
                            let tw = g.tiled_windows.clone();
                            for wid in tw {
                                if let Some(data) = st.session.screen_dump(wid) {
                                    let _ = client_tx.send(ServerMsg::ScreenDump { window_id: wid, data });
                                }
                            }
                        }
                    }
                }
            }
            ClientMsg::InputToWindow { window_id, data } => {
                if let Some(Node::Window(w)) = st.session.nodes.get_mut(&window_id) {
                    let _ = w.pty.write(&data);
                }
            }
            ClientMsg::Detach => break,
            ClientMsg::Shutdown => {
                std::process::exit(0);
            }
        }
    }

    {
        let mut st = state.lock().await;
        st.client_tx = None;
    }
    writer_task.abort();
    eprintln!("Client detached");
    Ok(())
}
