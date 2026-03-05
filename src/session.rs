use crate::ai_detect::{self, AiStatus};
use crate::config;
use crate::protocol::{LayoutMode, NodeId, PaneDirection, ServerMsg, SplitDir, SplitTree, TabEntry, TreeGroup, TreeProject, TreeWindow};
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
    pub(crate) env_profile: Option<String>,
    pub(crate) on_project_stop: Option<String>,
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
    /// Binary split tree for tiled layout (None when in Stacked mode or not yet split)
    pub(crate) split_tree: Option<SplitTree>,
    /// Next pane ID to allocate
    pub(crate) next_pane_id: u32,
    /// Which pane is currently focused
    pub(crate) active_pane: Option<u32>,
    pub(crate) env_profile: Option<String>,
    pub(crate) layout_preset: Option<config::LayoutPreset>,
}

pub(crate) struct WindowNode {
    pub(crate) name: String,
    pub(crate) parent: NodeId,
    pub(crate) pty: PtyHandle,
    pub(crate) ai_status: Option<AiStatus>,
    pub(crate) last_cpu_time: u64,
    pub(crate) env_profile: Option<String>,
    pub(crate) on_pane_close: Option<String>,
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

    /// Find an existing project by name, returning its NodeId if found.
    pub(crate) fn find_project_by_name(&self, name: &str) -> Option<NodeId> {
        self.root_children.iter().find(|&&id| {
            matches!(self.nodes.get(&id), Some(Node::Project(p)) if p.name == name)
        }).copied()
    }

    /// Find an existing group by name under a project, returning its NodeId if found.
    pub(crate) fn find_group_by_name(&self, project_id: NodeId, name: &str) -> Option<NodeId> {
        if let Some(Node::Project(p)) = self.nodes.get(&project_id) {
            p.children.iter().find(|&&id| {
                matches!(self.nodes.get(&id), Some(Node::Group(g)) if g.name == name)
            }).copied()
        } else {
            None
        }
    }

    /// Find an existing window by name under a group, returning its NodeId if found.
    pub(crate) fn find_window_by_name(&self, group_id: NodeId, name: &str) -> Option<NodeId> {
        if let Some(Node::Group(g)) = self.nodes.get(&group_id) {
            g.children.iter().find(|&&id| {
                matches!(self.nodes.get(&id), Some(Node::Window(w)) if w.name == name)
            }).copied()
        } else {
            None
        }
    }

    pub(crate) fn add_project(&mut self, name: String, working_dir: PathBuf) -> NodeId {
        let id = self.alloc_id();
        self.nodes.insert(id, Node::Project(ProjectNode {
            name, working_dir, children: Vec::new(), env_profile: None, on_project_stop: None,
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
            layout_mode: LayoutMode::Stacked,
            split_tree: None,
            next_pane_id: 0,
            active_pane: None,
            env_profile: None,
            layout_preset: None,
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
        command: Option<String>,
        path_override: Option<PathBuf>,
        env_profile: Option<String>,
        pre_command: Option<String>,
        on_pane_close: Option<String>,
    ) -> Result<NodeId> {
        let id = self.alloc_id();
        let working_dir = path_override.unwrap_or_else(|| self.window_working_dir(parent));

        // Collect .env vars: directory .env first, then named env profiles overlay
        let mut env = HashMap::new();
        // Mark child shells as running inside zmux so nested `zmux` invocations
        // create a new project instead of launching a recursive client.
        env.insert("ZMUX".to_string(), "1".to_string());
        if let Some(Node::Group(g)) = self.nodes.get(&parent) {
            if let Some(Node::Project(p)) = self.nodes.get(&g.parent) {
                env.extend(config::parse_dotenv(&p.working_dir));
                // Layer project env profile
                if let Some(ref profile) = p.env_profile {
                    if let Ok(profile_env) = config::load_env_profile(profile) {
                        env.extend(profile_env);
                    }
                }
            }
            if let Some(ref wd) = g.working_dir {
                env.extend(config::parse_dotenv(wd));
            }
            // Layer group env profile
            if let Some(ref profile) = g.env_profile {
                if let Ok(profile_env) = config::load_env_profile(profile) {
                    env.extend(profile_env);
                }
            }
        }
        // Layer window-level env profile (explicit or inherited)
        let effective_profile = env_profile.or_else(|| self.resolve_env_profile(parent));
        if let Some(ref profile) = effective_profile {
            if let Ok(profile_env) = config::load_env_profile(profile) {
                env.extend(profile_env);
            }
        }

        let (mut pty, mut pty_rx) = PtyHandle::spawn_in(rows, cols, &working_dir, &env, self.shell.as_deref())?;

        // Write pre_command (e.g., virtualenv activation) before the startup command
        if let Some(ref pre) = pre_command {
            let _ = pty.write(format!("{}\n", pre).as_bytes());
        }
        // If a startup command was specified, write it to the PTY
        if let Some(ref cmd) = command {
            let _ = pty.write(format!("{}\n", cmd).as_bytes());
        }

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

        self.nodes.insert(id, Node::Window(WindowNode { name, parent, pty, ai_status: None, last_cpu_time: 0, env_profile: effective_profile, on_pane_close }));
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

        // Remove from parent group's children and split tree
        if let Some(pid) = parent_id {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&pid) {
                g.children.retain(|id| *id != window_id);
                // Remove from split tree if present
                if let Some(tree) = g.split_tree.take() {
                    g.split_tree = tree.remove_window(window_id);
                    // If tree is gone, reset pane state
                    if g.split_tree.is_none() {
                        g.active_pane = None;
                        g.layout_mode = LayoutMode::Stacked;
                    } else if let Some(ap) = g.active_pane {
                        // If active pane was removed, pick the first remaining leaf
                        if g.split_tree.as_ref().unwrap().window_for_pane(ap).is_none() {
                            g.active_pane = g.split_tree.as_ref().unwrap().leaves().first().map(|(pid, _)| *pid);
                        }
                    }
                }
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

        let (layout_mode, split_tree, active_pane) = if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                (g.layout_mode, g.split_tree.clone(), g.active_pane)
            } else {
                (LayoutMode::Stacked, None, None)
            }
        } else {
            (LayoutMode::Stacked, None, None)
        };

        ServerMsg::TabState {
            projects, groups, windows,
            active_project: self.active_project,
            active_group: self.active_group,
            active_window: self.active_window,
            layout_mode, split_tree, active_pane,
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
                                let parser = w.pty.parser.lock().unwrap_or_else(|e| e.into_inner());
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
                        // Create a single-leaf tree if none exists
                        if g.split_tree.is_none() {
                            if let Some(wid) = self.active_window {
                                if g.children.contains(&wid) {
                                    let pane_id = g.next_pane_id;
                                    g.next_pane_id += 1;
                                    g.split_tree = Some(SplitTree::Leaf { pane_id, window_id: wid });
                                    g.active_pane = Some(pane_id);
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

    /// Split the active pane in the given direction
    pub(crate) fn split_pane(&mut self, direction: SplitDir) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        let active_pane = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled => {
                match g.active_pane {
                    Some(ap) => ap,
                    None => return,
                }
            }
            _ => return,
        };

        // Get the current window for this pane
        let current_window = match self.nodes.get(&gid) {
            Some(Node::Group(g)) => {
                g.split_tree.as_ref().and_then(|t| t.window_for_pane(active_pane))
            }
            _ => None,
        };
        let current_window = match current_window {
            Some(w) => w,
            None => return,
        };

        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            let new_pane_id = g.next_pane_id;
            g.next_pane_id += 1;

            // Replace the active leaf with a split: original on first, new on second
            fn split_at(tree: SplitTree, target: u32, direction: SplitDir, new_pane_id: u32, window_id: NodeId) -> SplitTree {
                match tree {
                    SplitTree::Leaf { pane_id, window_id: wid } if pane_id == target => {
                        SplitTree::Split {
                            direction,
                            ratio: 0.5,
                            first: Box::new(SplitTree::Leaf { pane_id, window_id: wid }),
                            second: Box::new(SplitTree::Leaf { pane_id: new_pane_id, window_id }),
                        }
                    }
                    SplitTree::Split { direction: d, ratio, first, second } => {
                        SplitTree::Split {
                            direction: d,
                            ratio,
                            first: Box::new(split_at(*first, target, direction, new_pane_id, window_id)),
                            second: Box::new(split_at(*second, target, direction, new_pane_id, window_id)),
                        }
                    }
                    other => other,
                }
            }

            if let Some(tree) = g.split_tree.take() {
                g.split_tree = Some(split_at(tree, active_pane, direction, new_pane_id, current_window));
                g.active_pane = Some(new_pane_id);
            }
        }

        // Update active_window to match the new pane's window
        self.active_window = Some(current_window);
    }

    /// Close the active pane (unsplit). Sibling takes parent's place.
    pub(crate) fn close_split(&mut self) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        let active_pane = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled => g.active_pane,
            _ => return,
        };
        let active_pane = match active_pane {
            Some(ap) => ap,
            None => return,
        };

        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if let Some(tree) = g.split_tree.take() {
                match tree.remove_leaf(active_pane) {
                    Some(new_tree) => {
                        // Pick the first remaining leaf as the new active pane
                        let new_active = new_tree.leaves().first().map(|(pid, _)| *pid);
                        g.split_tree = Some(new_tree);
                        g.active_pane = new_active;
                        // Update active_window
                        if let (Some(ap), Some(ref t)) = (g.active_pane, &g.split_tree) {
                            self.active_window = t.window_for_pane(ap);
                        }
                    }
                    None => {
                        // Last pane removed, switch back to stacked
                        g.split_tree = None;
                        g.active_pane = None;
                        g.layout_mode = LayoutMode::Stacked;
                    }
                }
            }
        }
    }

    /// Swap split direction (H↔V) at the active pane's parent
    pub(crate) fn swap_split_direction(&mut self) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if g.layout_mode != LayoutMode::Tiled {
                return;
            }
            if let (Some(ap), Some(ref mut tree)) = (g.active_pane, &mut g.split_tree) {
                tree.swap_direction_at(ap);
            }
        }
    }

    /// Cycle pane content: replace the active pane's window with next/prev window in group.
    pub(crate) fn cycle_pane_content(&mut self, forward: bool) -> bool {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return false,
        };

        let (active_pane, children, tree_windows) = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled && g.split_tree.is_some() => {
                let tw: Vec<NodeId> = g.split_tree.as_ref().unwrap().window_ids();
                (g.active_pane, g.children.clone(), tw)
            }
            _ => return false,
        };
        let active_pane = match active_pane {
            Some(ap) => ap,
            None => return false,
        };

        let current_window = match self.nodes.get(&gid) {
            Some(Node::Group(g)) => g.split_tree.as_ref().and_then(|t| t.window_for_pane(active_pane)),
            _ => None,
        };
        let current_window = match current_window {
            Some(w) => w,
            None => return false,
        };

        // Find the current window's position in children
        let child_idx = match children.iter().position(|&id| id == current_window) {
            Some(i) => i,
            None => return false,
        };

        // Find next/prev window not already shown in a pane
        let n = children.len();
        let mut replacement = None;
        for step in 1..n {
            let idx = if forward { (child_idx + step) % n } else { (child_idx + n - step) % n };
            let candidate = children[idx];
            if !tree_windows.contains(&candidate) || candidate == current_window {
                if !tree_windows.contains(&candidate) {
                    replacement = Some(candidate);
                    break;
                }
            }
        }

        let replacement = match replacement {
            Some(id) => id,
            None => return false,
        };

        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if let Some(ref mut tree) = g.split_tree {
                tree.set_pane_window(active_pane, replacement);
            }
        }
        self.active_window = Some(replacement);
        true
    }

    /// Cycle pane content globally: across all groups and projects.
    pub(crate) fn cycle_pane_content_global(&mut self, forward: bool) -> bool {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return false,
        };

        let active_pane = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled && g.split_tree.is_some() => g.active_pane,
            _ => return false,
        };
        let active_pane = match active_pane {
            Some(ap) => ap,
            None => return false,
        };

        let current_window = match self.nodes.get(&gid) {
            Some(Node::Group(g)) => g.split_tree.as_ref().and_then(|t| t.window_for_pane(active_pane)),
            _ => None,
        };
        let current_window = match current_window {
            Some(w) => w,
            None => return false,
        };

        // Build a flat list of all window IDs across all projects/groups
        let mut all_windows = Vec::new();
        for &pid in &self.root_children {
            if let Some(Node::Project(p)) = self.nodes.get(&pid) {
                for &gid2 in &p.children {
                    if let Some(Node::Group(g2)) = self.nodes.get(&gid2) {
                        for &wid in &g2.children {
                            if matches!(self.nodes.get(&wid), Some(Node::Window(_))) {
                                all_windows.push(wid);
                            }
                        }
                    }
                }
            }
        }

        if all_windows.is_empty() {
            return false;
        }

        // Get windows currently shown in the split tree
        let tree_windows: Vec<NodeId> = match self.nodes.get(&gid) {
            Some(Node::Group(g)) => g.split_tree.as_ref().map(|t| t.window_ids()).unwrap_or_default(),
            _ => Vec::new(),
        };

        let current_idx = all_windows.iter().position(|&id| id == current_window).unwrap_or(0);
        let n = all_windows.len();
        let mut replacement = None;
        for step in 1..n {
            let idx = if forward { (current_idx + step) % n } else { (current_idx + n - step) % n };
            let candidate = all_windows[idx];
            if !tree_windows.contains(&candidate) {
                replacement = Some(candidate);
                break;
            }
        }

        let replacement = match replacement {
            Some(id) => id,
            None => return false,
        };

        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if let Some(ref mut tree) = g.split_tree {
                tree.set_pane_window(active_pane, replacement);
            }
        }
        self.active_window = Some(replacement);
        true
    }

    /// Spatial focus navigation: find the nearest pane in the given direction
    pub(crate) fn focus_pane(&mut self, direction: PaneDirection) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        let active_pane = match self.nodes.get(&gid) {
            Some(Node::Group(g)) if g.layout_mode == LayoutMode::Tiled && g.split_tree.is_some() => g.active_pane,
            _ => return,
        };
        let active_pane = match active_pane {
            Some(ap) => ap,
            None => return,
        };

        // Compute spatial rects for all leaves using a dummy area
        let tree = match self.nodes.get(&gid) {
            Some(Node::Group(g)) => g.split_tree.clone(),
            _ => None,
        };
        let tree = match tree {
            Some(t) => t,
            None => return,
        };

        // Use a large dummy area; we only care about relative positions
        let rects = split_tree_rects(&tree, 0, 0, 1000, 1000);

        let current_rect = match rects.iter().find(|(pid, _, _, _, _)| *pid == active_pane) {
            Some(r) => *r,
            None => return,
        };
        let (_, cx, cy, cw, ch) = current_rect;
        let center_x = cx + cw / 2;
        let center_y = cy + ch / 2;

        let mut best: Option<(u32, i32)> = None; // (pane_id, distance)
        for &(pid, rx, ry, rw, rh) in &rects {
            if pid == active_pane {
                continue;
            }
            let rcx = rx + rw / 2;
            let rcy = ry + rh / 2;

            let valid = match direction {
                PaneDirection::Left => rcx < center_x,
                PaneDirection::Right => rcx > center_x,
                PaneDirection::Up => rcy < center_y,
                PaneDirection::Down => rcy > center_y,
            };
            if !valid {
                continue;
            }

            let dist = (rcx - center_x).abs() + (rcy - center_y).abs();
            if best.is_none() || dist < best.unwrap().1 {
                best = Some((pid, dist));
            }
        }

        if let Some((new_pane, _)) = best {
            if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
                g.active_pane = Some(new_pane);
                if let Some(ref tree) = g.split_tree {
                    self.active_window = tree.window_for_pane(new_pane);
                }
            }
        }
    }

    /// Resize the active pane by adjusting its parent split's ratio
    pub(crate) fn resize_pane(&mut self, direction: PaneDirection) {
        let gid = match self.active_group {
            Some(gid) => gid,
            None => return,
        };
        if let Some(Node::Group(g)) = self.nodes.get_mut(&gid) {
            if g.layout_mode != LayoutMode::Tiled {
                return;
            }
            if let (Some(ap), Some(ref mut tree)) = (g.active_pane, &mut g.split_tree) {
                let delta = match direction {
                    PaneDirection::Right | PaneDirection::Down => 0.05,
                    PaneDirection::Left | PaneDirection::Up => -0.05,
                };
                tree.adjust_ratio_at(ap, delta);
            }
        }
    }

    /// Returns true if the active group is in tiled mode and a window is in the split tree
    pub(crate) fn is_tiled_window(&self, window_id: NodeId) -> bool {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled {
                    if let Some(ref tree) = g.split_tree {
                        return tree.window_ids().contains(&window_id);
                    }
                }
            }
        }
        false
    }

    pub(crate) fn active_tiled_windows(&self) -> &[NodeId] {
        if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled {
                    if let Some(ref tree) = g.split_tree {
                        return tree.window_ids();
                    }
                }
            }
        }
        &[]
    }

    pub(crate) fn resize_all(&mut self, rows: u16, cols: u16) -> Result<()> {
        let tiled_sizes: Option<Vec<(u32, NodeId, u16, u16)>> = if let Some(gid) = self.active_group {
            if let Some(Node::Group(g)) = self.nodes.get(&gid) {
                if g.layout_mode == LayoutMode::Tiled {
                    g.split_tree.as_ref().map(|tree| {
                        split_tree_pane_sizes(tree, rows, cols)
                    })
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
                    if let Some((_, _, r, c)) = sizes.iter().find(|(_, wid, _, _)| wid == id) {
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

    /// Set env profile on a node
    pub(crate) fn set_env_profile(&mut self, node_id: NodeId, profile: Option<String>) {
        match self.nodes.get_mut(&node_id) {
            Some(Node::Project(p)) => p.env_profile = profile,
            Some(Node::Group(g)) => g.env_profile = profile,
            Some(Node::Window(w)) => w.env_profile = profile,
            None => {}
        }
    }

    /// Resolve env profile by walking up the tree to find the nearest one
    pub(crate) fn resolve_env_profile(&self, node_id: NodeId) -> Option<String> {
        match self.nodes.get(&node_id) {
            Some(Node::Window(w)) => {
                w.env_profile.clone().or_else(|| self.resolve_env_profile(w.parent))
            }
            Some(Node::Group(g)) => {
                g.env_profile.clone().or_else(|| self.resolve_env_profile(g.parent))
            }
            Some(Node::Project(p)) => p.env_profile.clone(),
            None => None,
        }
    }

    /// Collect all window IDs under a node (project or group)
    #[allow(dead_code)]
    pub(crate) fn windows_in_scope(&self, node_id: NodeId) -> Vec<NodeId> {
        match self.nodes.get(&node_id) {
            Some(Node::Window(_)) => vec![node_id],
            Some(Node::Group(g)) => g.children.clone(),
            Some(Node::Project(p)) => {
                p.children.iter().flat_map(|gid| {
                    match self.nodes.get(gid) {
                        Some(Node::Group(g)) => g.children.clone(),
                        _ => Vec::new(),
                    }
                }).collect()
            }
            None => Vec::new(),
        }
    }

    /// Source an env profile file into a running PTY
    pub(crate) fn source_env_profile(&mut self, window_id: NodeId, profile: &str) -> Result<()> {
        let path = config::env_profile_path(profile);
        if !path.exists() {
            anyhow::bail!("Env profile '{}' not found", profile);
        }
        if let Some(Node::Window(w)) = self.nodes.get_mut(&window_id) {
            let cmd = format!("set -a; source '{}'; set +a\n", path.display());
            w.pty.write(cmd.as_bytes())?;
        }
        Ok(())
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
                let group_dir = self.window_working_dir(*gid);
                let windows = g.children.iter().filter_map(|wid| {
                    match self.nodes.get(wid) {
                        Some(Node::Window(w)) => {
                            let win_cwd = w.pty.cwd();
                            let path = win_cwd.and_then(|cwd| {
                                if cwd != group_dir {
                                    Some(cwd.to_string_lossy().to_string())
                                } else {
                                    None
                                }
                            });
                            Some(config::WindowPreset {
                                name: w.name.clone(),
                                path,
                                command: None,
                                env_profile: w.env_profile.clone(),
                                on_pane_open: None,
                                on_pane_close: w.on_pane_close.clone(),
                            })
                        },
                        _ => None,
                    }
                }).collect();
                Some(config::GroupPreset {
                    name: g.name.clone(),
                    path: g.working_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                    worktree_branch: g.worktree_path.as_ref().and_then(|wt| {
                        wt.file_name().map(|n| n.to_string_lossy().to_string())
                    }),
                    env_profile: g.env_profile.clone(),
                    layout: g.layout_preset.clone(),
                    windows,
                })
            }).collect();
            Some(config::ProjectPreset {
                name: p.name.clone(),
                path: p.working_dir.to_string_lossy().to_string(),
                env_profile: p.env_profile.clone(),
                startup_group: None,
                startup_window: None,
                pre_window: None,
                on_project_start: None,
                on_project_first_start: None,
                on_project_restart: None,
                on_project_stop: p.on_project_stop.clone(),
                groups,
            })
        }).collect();
        config::Preset { projects }
    }

    /// Get a screen dump for attach/reconnect: convert vt100 screen to ANSI bytes
    pub(crate) fn screen_dump(&self, window_id: NodeId) -> Option<Vec<u8>> {
        match self.nodes.get(&window_id) {
            Some(Node::Window(w)) => {
                let parser = w.pty.parser.lock().unwrap_or_else(|e| e.into_inner());
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

/// Build a split tree from a list of (pane_id, window_id) pairs and a named layout preset.
pub(crate) fn build_layout_tree(
    windows: &[(u32, NodeId)],
    layout: &config::LayoutPreset,
) -> Option<SplitTree> {
    if windows.is_empty() {
        return None;
    }
    if windows.len() == 1 {
        return Some(SplitTree::Leaf {
            pane_id: windows[0].0,
            window_id: windows[0].1,
        });
    }
    Some(match layout {
        config::LayoutPreset::EvenHorizontal => build_even_split(windows, SplitDir::Vertical),
        config::LayoutPreset::EvenVertical => build_even_split(windows, SplitDir::Horizontal),
        config::LayoutPreset::MainHorizontal => build_main_split(windows, SplitDir::Horizontal),
        config::LayoutPreset::MainVertical => build_main_split(windows, SplitDir::Vertical),
        config::LayoutPreset::Tiled => build_tiled(windows),
    })
}

fn build_even_split(windows: &[(u32, NodeId)], dir: SplitDir) -> SplitTree {
    if windows.len() == 1 {
        return SplitTree::Leaf { pane_id: windows[0].0, window_id: windows[0].1 };
    }
    let n = windows.len();
    SplitTree::Split {
        direction: dir,
        ratio: 1.0 / n as f64,
        first: Box::new(SplitTree::Leaf { pane_id: windows[0].0, window_id: windows[0].1 }),
        second: Box::new(build_even_split(&windows[1..], dir)),
    }
}

fn build_main_split(windows: &[(u32, NodeId)], dir: SplitDir) -> SplitTree {
    let secondary_dir = match dir {
        SplitDir::Horizontal => SplitDir::Vertical,
        SplitDir::Vertical => SplitDir::Horizontal,
    };
    SplitTree::Split {
        direction: dir,
        ratio: 0.65,
        first: Box::new(SplitTree::Leaf { pane_id: windows[0].0, window_id: windows[0].1 }),
        second: Box::new(build_even_split(&windows[1..], secondary_dir)),
    }
}

fn build_tiled(windows: &[(u32, NodeId)]) -> SplitTree {
    let n = windows.len();
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = (n + cols - 1) / cols;

    let row_trees: Vec<SplitTree> = (0..rows).map(|r| {
        let start = r * cols;
        let end = (start + cols).min(n);
        build_even_split(&windows[start..end], SplitDir::Vertical)
    }).collect();

    build_even_split_from_trees(&row_trees, SplitDir::Horizontal)
}

fn build_even_split_from_trees(trees: &[SplitTree], dir: SplitDir) -> SplitTree {
    if trees.len() == 1 {
        return trees[0].clone();
    }
    let n = trees.len();
    SplitTree::Split {
        direction: dir,
        ratio: 1.0 / n as f64,
        first: Box::new(trees[0].clone()),
        second: Box::new(build_even_split_from_trees(&trees[1..], dir)),
    }
}

/// Compute pane sizes for a split tree. Returns (pane_id, window_id, rows, cols) for each leaf.
pub(crate) fn split_tree_pane_sizes(tree: &SplitTree, total_rows: u16, total_cols: u16) -> Vec<(u32, NodeId, u16, u16)> {
    let mut result = Vec::new();
    fn walk(tree: &SplitTree, rows: u16, cols: u16, result: &mut Vec<(u32, NodeId, u16, u16)>) {
        match tree {
            SplitTree::Leaf { pane_id, window_id } => {
                result.push((*pane_id, *window_id, rows, cols));
            }
            SplitTree::Split { direction, ratio, first, second } => {
                match direction {
                    SplitDir::Vertical => {
                        // Split columns: first gets ratio*cols, second gets rest (minus 1 for border)
                        let usable = cols.saturating_sub(1); // 1 col for border
                        let first_cols = ((usable as f64) * ratio).round() as u16;
                        let second_cols = usable.saturating_sub(first_cols);
                        walk(first, rows, first_cols, result);
                        walk(second, rows, second_cols, result);
                    }
                    SplitDir::Horizontal => {
                        // Split rows: first gets ratio*rows, second gets rest (minus 1 for border)
                        let usable = rows.saturating_sub(1);
                        let first_rows = ((usable as f64) * ratio).round() as u16;
                        let second_rows = usable.saturating_sub(first_rows);
                        walk(first, first_rows, cols, result);
                        walk(second, second_rows, cols, result);
                    }
                }
            }
        }
    }
    walk(tree, total_rows, total_cols, &mut result);
    result
}

/// Compute spatial rects (for focus navigation). Returns (pane_id, x, y, w, h) using i32.
pub(crate) fn split_tree_rects(tree: &SplitTree, x: i32, y: i32, w: i32, h: i32) -> Vec<(u32, i32, i32, i32, i32)> {
    let mut result = Vec::new();
    fn walk(tree: &SplitTree, x: i32, y: i32, w: i32, h: i32, result: &mut Vec<(u32, i32, i32, i32, i32)>) {
        match tree {
            SplitTree::Leaf { pane_id, .. } => {
                result.push((*pane_id, x, y, w, h));
            }
            SplitTree::Split { direction, ratio, first, second } => {
                match direction {
                    SplitDir::Vertical => {
                        let first_w = ((w as f64) * ratio).round() as i32;
                        let second_w = w - first_w;
                        walk(first, x, y, first_w, h, result);
                        walk(second, x + first_w, y, second_w, h, result);
                    }
                    SplitDir::Horizontal => {
                        let first_h = ((h as f64) * ratio).round() as i32;
                        let second_h = h - first_h;
                        walk(first, x, y, w, first_h, result);
                        walk(second, x, y + first_h, w, second_h, result);
                    }
                }
            }
        }
    }
    walk(tree, x, y, w, h, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tree_pane_sizes_single() {
        let tree = SplitTree::Leaf { pane_id: 0, window_id: 1 };
        let result = split_tree_pane_sizes(&tree, 24, 80);
        assert_eq!(result, vec![(0, 1, 24, 80)]);
    }

    #[test]
    fn split_tree_pane_sizes_vertical() {
        let tree = SplitTree::Split {
            direction: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::Leaf { pane_id: 0, window_id: 1 }),
            second: Box::new(SplitTree::Leaf { pane_id: 1, window_id: 2 }),
        };
        let result = split_tree_pane_sizes(&tree, 24, 81);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].2, 24); // full rows
        assert_eq!(result[1].2, 24);
        // 81 - 1 border = 80, 50% = 40 each
        assert_eq!(result[0].3 + result[1].3, 80);
    }

    #[test]
    fn split_tree_pane_sizes_horizontal() {
        let tree = SplitTree::Split {
            direction: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(SplitTree::Leaf { pane_id: 0, window_id: 1 }),
            second: Box::new(SplitTree::Leaf { pane_id: 1, window_id: 2 }),
        };
        let result = split_tree_pane_sizes(&tree, 25, 80);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].3, 80); // full cols
        assert_eq!(result[1].3, 80);
        assert_eq!(result[0].2 + result[1].2, 24); // 25 - 1 border
    }
}

/// Convert a vt100 screen to ANSI escape sequences that reproduce it.
fn screen_to_ansi(screen: &vt100::Screen) -> Vec<u8> {
    let mut out = Vec::with_capacity(8192);
    // Clear screen and reset attributes before writing formatted contents
    out.extend_from_slice(b"\x1b[H\x1b[2J\x1b[0m");
    // Use the vt100 library's built-in method which preserves colors and attributes
    out.extend_from_slice(&screen.contents_formatted());
    let cursor = screen.cursor_position();
    out.extend_from_slice(format!("\x1b[{};{}H", cursor.0 + 1, cursor.1 + 1).as_bytes());
    out
}
