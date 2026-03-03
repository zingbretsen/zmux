use crate::ai_detect::{self, AiStatus};
use crate::config;
use crate::protocol::{LayoutMode, NodeId, PaneDirection, ServerMsg, TabEntry, TileLayout, TreeGroup, TreeProject, TreeWindow};
use crate::pty::PtyHandle;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub(crate) struct SessionTree {
    pub(crate) nodes: HashMap<NodeId, Node>,
    pub(crate) root_children: Vec<NodeId>,
    next_id: NodeId,
    pub(crate) active_project: Option<NodeId>,
    pub(crate) active_group: Option<NodeId>,
    pub(crate) active_window: Option<NodeId>,
    pub(crate) shell: Option<String>,
}

pub(crate) struct ProjectNode {
    pub(crate) name: String,
    pub(crate) working_dir: PathBuf,
    pub(crate) children: Vec<NodeId>,
}

pub(crate) struct GroupNode {
    pub(crate) name: String,
    pub(crate) parent: NodeId,
    pub(crate) children: Vec<NodeId>,
    pub(crate) working_dir: Option<PathBuf>,
    /// If this group was created from a git worktree, track it for cleanup
    pub(crate) worktree_path: Option<PathBuf>,
    /// Layout mode: stacked (one visible) or tiled (multiple visible)
    pub(crate) layout_mode: LayoutMode,
    /// Current tile layout algorithm
    pub(crate) tile_layout: TileLayout,
    /// Windows marked for tiling (subset of children)
    pub(crate) tiled_windows: Vec<NodeId>,
    /// Per-window size weights for tiled layout (width_weight, height_weight), default (1.0, 1.0)
    pub(crate) pane_weights: HashMap<NodeId, (f64, f64)>,
}

pub(crate) struct WindowNode {
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) parent: NodeId,
    pub(crate) pty: PtyHandle,
    pub(crate) ai_status: Option<AiStatus>,
    pub(crate) last_cpu_time: u64,
}

pub(crate) enum Node {
    Project(ProjectNode),
    Group(GroupNode),
    Window(WindowNode),
}

impl SessionTree {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn next_id(&self) -> NodeId {
        self.next_id
    }

    pub(crate) fn set_next_id(&mut self, id: NodeId) {
        self.next_id = id;
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub(crate) fn add_project(&mut self, name: String, working_dir: PathBuf) -> NodeId {
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

    pub(crate) fn add_group(&mut self, parent: NodeId, name: String, working_dir: Option<PathBuf>, worktree_path: Option<PathBuf>) -> NodeId {
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

    pub(crate) fn add_window(
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

    pub(crate) fn remove_window(&mut self, window_id: NodeId) {
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
    pub(crate) fn remove_group(&mut self, group_id: NodeId) -> Option<(PathBuf, PathBuf)> {
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

    /// Remove a project and all its groups/windows. Returns list of (project_dir, worktree_path) for cleanup.
    pub(crate) fn remove_project(&mut self, project_id: NodeId) -> Vec<(PathBuf, PathBuf)> {
        let group_ids = match self.nodes.get(&project_id) {
            Some(Node::Project(p)) => p.children.clone(),
            _ => return Vec::new(),
        };

        let mut worktree_infos = Vec::new();
        for gid in &group_ids {
            if let Some(info) = self.remove_group(*gid) {
                worktree_infos.push(info);
            }
        }

        // remove_group may have already cleaned up the project if it became empty,
        // but if not, clean up now
        if self.nodes.contains_key(&project_id) {
            self.nodes.remove(&project_id);
            self.root_children.retain(|id| *id != project_id);
        }

        if self.active_project == Some(project_id) {
            self.active_project = self.root_children.first().copied();
            if let Some(pid) = self.active_project {
                self.select_project(pid);
            } else {
                self.active_group = None;
                self.active_window = None;
            }
        }

        worktree_infos
    }

    pub(crate) fn move_window_to_group(&mut self, window_id: NodeId, new_group_id: NodeId) {
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

    pub(crate) fn window_cwd(&self, window_id: NodeId) -> Option<PathBuf> {
        if let Some(Node::Window(w)) = self.nodes.get(&window_id) {
            w.pty.cwd()
        } else {
            None
        }
    }

    pub(crate) fn window_working_dir(&self, group_id: NodeId) -> PathBuf {
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

    pub(crate) fn tab_state(&self) -> ServerMsg {
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

    pub(crate) fn select_project(&mut self, id: NodeId) {
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

    pub(crate) fn select_group(&mut self, id: NodeId) {
        self.active_group = Some(id);
        if let Some(Node::Group(g)) = self.nodes.get(&id) {
            self.active_project = Some(g.parent);
            self.active_window = g.children.first().copied();
        } else { self.active_window = None; }
    }

    pub(crate) fn select_window(&mut self, id: NodeId) {
        self.active_window = Some(id);
        if let Some(Node::Window(w)) = self.nodes.get(&id) {
            let group_id = w.parent;
            self.active_group = Some(group_id);
            if let Some(Node::Group(g)) = self.nodes.get(&group_id) {
                self.active_project = Some(g.parent);
            }
        }
    }

    pub(crate) fn active_window_mut(&mut self) -> Option<&mut WindowNode> {
        let id = self.active_window?;
        match self.nodes.get_mut(&id) {
            Some(Node::Window(w)) => Some(w),
            _ => None,
        }
    }

    /// Poll all windows for AI tool processes. Returns true if any status changed.
    pub(crate) fn poll_ai_status(&mut self) -> bool {
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
    pub(crate) fn cycle_ai_window(&mut self, forward: bool) -> bool {
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
            None => 0,
        };

        let (pid, gid, wid) = ai_windows[next_idx];
        self.active_project = Some(pid);
        self.active_group = Some(gid);
        self.active_window = Some(wid);
        true
    }

    /// Search all windows' screen content for a query string (case-insensitive).
    /// Returns (project_id, group_id, window_id, window_name) of first match.
    pub(crate) fn search_windows(&self, query: &str) -> Option<(NodeId, NodeId, NodeId, String)> {
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

    pub(crate) fn toggle_layout(&mut self) {
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

    pub(crate) fn cycle_layout(&mut self) {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
                g.tile_layout = g.tile_layout.next();
            }
        }
    }

    pub(crate) fn toggle_tile(&mut self, window_id: NodeId) {
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

    /// Replace the active window in the tiled set with the next/prev untiled window.
    /// Cycles relative to the current window's position in the group's children list.
    pub(crate) fn cycle_pane_content(&mut self, forward: bool) -> bool {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return false,
        };
        let active = match self.active_window {
            Some(wid) => wid,
            None => return false,
        };

        let (tiled, children) = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled => {
                (g.tiled_windows.clone(), g.children.clone())
            }
            _ => return false,
        };

        // Active window must be in the tiled set
        let tile_idx = match tiled.iter().position(|&id| id == active) {
            Some(i) => i,
            None => return false,
        };

        // Find the current window's position in the group's children list
        let child_idx = match children.iter().position(|&id| id == active) {
            Some(i) => i,
            None => return false,
        };

        // Find next/prev untiled window relative to the current window in children order
        let n = children.len();
        let mut replacement = None;
        for step in 1..n {
            let idx = if forward {
                (child_idx + step) % n
            } else {
                (child_idx + n - step) % n
            };
            let candidate = children[idx];
            if !tiled.contains(&candidate) {
                replacement = Some(candidate);
                break;
            }
        }

        let replacement = match replacement {
            Some(id) => id,
            None => return false, // all windows are tiled, nothing to swap
        };

        // Swap: replace active window at its tile position with the replacement
        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            g.tiled_windows[tile_idx] = replacement;

            // Transfer pane weight from old window to new
            if let Some(weight) = g.pane_weights.remove(&active) {
                g.pane_weights.insert(replacement, weight);
            }
        }

        self.active_window = Some(replacement);
        true
    }

    pub(crate) fn focus_pane(&mut self, direction: PaneDirection) {
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

    pub(crate) fn resize_pane(&mut self, direction: PaneDirection) {
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
    pub(crate) fn is_tiled_window(&self, window_id: NodeId) -> bool {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                return g.layout_mode == LayoutMode::Tiled && g.tiled_windows.contains(&window_id);
            }
        }
        false
    }

    pub(crate) fn active_tiled_windows(&self) -> Vec<NodeId> {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled {
                    return g.tiled_windows.clone();
                }
            }
        }
        Vec::new()
    }

    pub(crate) fn resize_all(&mut self, rows: u16, cols: u16) -> Result<()> {
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
    pub(crate) fn active_window_cwd(&self) -> Option<PathBuf> {
        let id = self.active_window?;
        match self.nodes.get(&id) {
            Some(Node::Window(w)) => w.pty.cwd(),
            _ => None,
        }
    }

    /// Set the active project's working directory to the active window's cwd
    pub(crate) fn set_project_dir(&mut self) -> Option<String> {
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
    pub(crate) fn set_group_dir(&mut self) -> Option<String> {
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
    pub(crate) fn to_preset(&self) -> config::Preset {
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
    pub(crate) fn screen_dump(&self, window_id: NodeId) -> Option<Vec<u8>> {
        match self.nodes.get(&window_id) {
            Some(Node::Window(w)) => {
                let parser = w.pty.parser.lock().unwrap();
                Some(screen_to_ansi(parser.screen()))
            }
            _ => None,
        }
    }

    /// Build the full session tree for tree nav mode
    pub(crate) fn full_tree(&self) -> Vec<TreeProject> {
        self.root_children.iter().filter_map(|&pid| {
            match self.nodes.get(&pid) {
                Some(Node::Project(p)) => {
                    let groups = p.children.iter().filter_map(|&gid| {
                        match self.nodes.get(&gid) {
                            Some(Node::Group(g)) => {
                                let windows = g.children.iter().filter_map(|&wid| {
                                    match self.nodes.get(&wid) {
                                        Some(Node::Window(w)) => {
                                            let screen_data = self.screen_dump(wid).unwrap_or_default();
                                            Some(TreeWindow {
                                                id: wid,
                                                name: w.name.clone(),
                                                ai_status: w.ai_status.clone(),
                                                screen_data,
                                            })
                                        }
                                        _ => None,
                                    }
                                }).collect();
                                Some(TreeGroup { id: gid, name: g.name.clone(), windows })
                            }
                            _ => None,
                        }
                    }).collect();
                    Some(TreeProject { id: pid, name: p.name.clone(), groups })
                }
                _ => None,
            }
        }).collect()
    }
}

/// Compute pane sizes for a tile layout. Returns (window_id, rows, cols) for each tiled window.
pub(crate) fn pane_sizes(layout: TileLayout, windows: &[NodeId], total_rows: u16, total_cols: u16, weights: &HashMap<NodeId, (f64, f64)>) -> Vec<(NodeId, u16, u16)> {
    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(windows[0], total_rows, total_cols)];
    }

    let distribute = |usable: u16, items: &[NodeId], get_weight: &dyn Fn(NodeId) -> f64| -> Vec<u16> {
        let total_weight: f64 = items.iter().map(|&id| get_weight(id)).sum();
        let mut sizes = Vec::with_capacity(items.len());
        let mut used: u16 = 0;
        for (i, &id) in items.iter().enumerate() {
            if i == items.len() - 1 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_sizes_single_window() {
        let windows = vec![1];
        let weights = HashMap::new();
        let result = pane_sizes(TileLayout::EqualColumns, &windows, 24, 80, &weights);
        assert_eq!(result, vec![(1, 24, 80)]);
    }

    #[test]
    fn pane_sizes_two_columns() {
        let windows = vec![1, 2];
        let weights = HashMap::new();
        let result = pane_sizes(TileLayout::EqualColumns, &windows, 24, 81, &weights);
        // 81 cols - 1 border = 80 usable, split 40/40
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 1);
        assert_eq!(result[1].0, 2);
        assert_eq!(result[0].1, 24); // full rows
        assert_eq!(result[1].1, 24);
        assert_eq!(result[0].2 + result[1].2, 80); // total usable cols
    }

    #[test]
    fn pane_sizes_two_rows() {
        let windows = vec![1, 2];
        let weights = HashMap::new();
        let result = pane_sizes(TileLayout::EqualRows, &windows, 25, 80, &weights);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].2, 80); // full cols
        assert_eq!(result[1].2, 80);
        assert_eq!(result[0].1 + result[1].1, 24); // 25 - 1 border
    }

    #[test]
    fn pane_sizes_empty() {
        let result = pane_sizes(TileLayout::EqualColumns, &[], 24, 80, &HashMap::new());
        assert!(result.is_empty());
    }

    #[test]
    fn pane_sizes_weighted() {
        let windows = vec![1, 2];
        let mut weights = HashMap::new();
        weights.insert(1, (2.0, 1.0)); // window 1 is 2x wider
        let result = pane_sizes(TileLayout::EqualColumns, &windows, 24, 81, &weights);
        // 80 usable, weights 2:1 => ~53:27
        assert!(result[0].2 > result[1].2);
    }

    #[test]
    fn pane_sizes_main_left() {
        let windows = vec![1, 2, 3];
        let weights = HashMap::new();
        let result = pane_sizes(TileLayout::MainLeft, &windows, 24, 80, &weights);
        assert_eq!(result.len(), 3);
        // Main pane should be wider than side panes
        assert!(result[0].2 > result[1].2);
        // Side panes share height
        assert_eq!(result[1].2, result[2].2);
    }

    #[test]
    fn pane_sizes_grid() {
        let windows = vec![1, 2, 3, 4];
        let weights = HashMap::new();
        let result = pane_sizes(TileLayout::Grid, &windows, 24, 80, &weights);
        assert_eq!(result.len(), 4);
        // All cells same size in grid
        assert_eq!(result[0].1, result[1].1);
        assert_eq!(result[0].2, result[1].2);
    }
}

/// Convert a vt100 screen to ANSI escape sequences that reproduce it.
fn screen_to_ansi(screen: &vt100::Screen) -> Vec<u8> {
    let mut out = Vec::with_capacity(8192);
    // Clear screen and reset attributes before writing formatted contents
    out.extend_from_slice(b"\x1b[H\x1b[2J\x1b[0m");
    // Use the vt100 library's built-in method which preserves colors and attributes
    out.extend_from_slice(&screen.contents_formatted());
    // Restore cursor position
    let cursor = screen.cursor_position();
    out.extend_from_slice(format!("\x1b[{};{}H", cursor.0 + 1, cursor.1 + 1).as_bytes());
    out
}
