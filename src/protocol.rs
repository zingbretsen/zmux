use serde::{Deserialize, Serialize};

pub type NodeId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    Stacked,
    Tiled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDir {
    /// First on top, second on bottom
    Horizontal,
    /// First on left, second on right
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SplitTree {
    Leaf {
        pane_id: u32,
        window_id: NodeId,
    },
    Split {
        direction: SplitDir,
        /// Fraction (0.0–1.0) of space allocated to the first child
        ratio: f64,
        first: Box<SplitTree>,
        second: Box<SplitTree>,
    },
}

impl SplitTree {
    /// Collect all leaf (pane_id, window_id) pairs
    pub fn leaves(&self) -> Vec<(u32, NodeId)> {
        match self {
            SplitTree::Leaf { pane_id, window_id } => vec![(*pane_id, *window_id)],
            SplitTree::Split { first, second, .. } => {
                let mut v = first.leaves();
                v.extend(second.leaves());
                v
            }
        }
    }

    /// Collect all window IDs present in the tree
    pub fn window_ids(&self) -> Vec<NodeId> {
        self.leaves().into_iter().map(|(_, wid)| wid).collect()
    }

    /// Find the window_id for a given pane_id
    pub fn window_for_pane(&self, target: u32) -> Option<NodeId> {
        match self {
            SplitTree::Leaf { pane_id, window_id } => {
                if *pane_id == target { Some(*window_id) } else { None }
            }
            SplitTree::Split { first, second, .. } => {
                first.window_for_pane(target).or_else(|| second.window_for_pane(target))
            }
        }
    }

    /// Remove a leaf by pane_id. Returns the modified tree (or None if the whole tree was that leaf).
    pub fn remove_leaf(self, target: u32) -> Option<SplitTree> {
        match self {
            SplitTree::Leaf { pane_id, window_id } => {
                if pane_id == target { None } else { Some(SplitTree::Leaf { pane_id, window_id }) }
            }
            SplitTree::Split { direction, ratio, first, second } => {
                let first_has = first.leaves().iter().any(|(pid, _)| *pid == target);
                let second_has = second.leaves().iter().any(|(pid, _)| *pid == target);
                if first_has {
                    match first.remove_leaf(target) {
                        Some(new_first) => Some(SplitTree::Split { direction, ratio, first: Box::new(new_first), second }),
                        None => Some(*second), // first was the target leaf, sibling takes over
                    }
                } else if second_has {
                    match second.remove_leaf(target) {
                        Some(new_second) => Some(SplitTree::Split { direction, ratio, first, second: Box::new(new_second) }),
                        None => Some(*first), // second was the target leaf, sibling takes over
                    }
                } else {
                    Some(SplitTree::Split { direction, ratio, first, second })
                }
            }
        }
    }

    /// Set the window_id for a given pane_id
    pub fn set_pane_window(&mut self, target: u32, new_window: NodeId) {
        match self {
            SplitTree::Leaf { pane_id, window_id } => {
                if *pane_id == target {
                    *window_id = new_window;
                }
            }
            SplitTree::Split { first, second, .. } => {
                first.set_pane_window(target, new_window);
                second.set_pane_window(target, new_window);
            }
        }
    }

    /// Swap the split direction at the parent of the given pane_id.
    /// Returns true if a swap was made.
    pub fn swap_direction_at(&mut self, target: u32) -> bool {
        match self {
            SplitTree::Leaf { .. } => false,
            SplitTree::Split { direction, first, second, .. } => {
                let in_first = first.leaves().iter().any(|(pid, _)| *pid == target);
                let in_second = second.leaves().iter().any(|(pid, _)| *pid == target);
                if in_first || in_second {
                    // Check if target is a direct child
                    let direct_child = matches!(first.as_ref(), SplitTree::Leaf { pane_id, .. } if *pane_id == target)
                        || matches!(second.as_ref(), SplitTree::Leaf { pane_id, .. } if *pane_id == target);
                    if direct_child {
                        *direction = match *direction {
                            SplitDir::Horizontal => SplitDir::Vertical,
                            SplitDir::Vertical => SplitDir::Horizontal,
                        };
                        return true;
                    }
                    // Recurse into the child that contains the target
                    if in_first { first.swap_direction_at(target) }
                    else { second.swap_direction_at(target) }
                } else {
                    false
                }
            }
        }
    }

    /// Adjust the ratio at the parent split of the given pane_id.
    /// delta is added to ratio (positive = more space for first child).
    pub fn adjust_ratio_at(&mut self, target: u32, delta: f64) -> bool {
        match self {
            SplitTree::Leaf { .. } => false,
            SplitTree::Split { ratio, first, second, .. } => {
                let in_first = first.leaves().iter().any(|(pid, _)| *pid == target);
                let in_second = second.leaves().iter().any(|(pid, _)| *pid == target);
                if in_first || in_second {
                    let direct_child = matches!(first.as_ref(), SplitTree::Leaf { pane_id, .. } if *pane_id == target)
                        || matches!(second.as_ref(), SplitTree::Leaf { pane_id, .. } if *pane_id == target);
                    if direct_child {
                        // Determine sign: if target is in first child, delta as-is; if in second, negate
                        let effective_delta = if in_first { delta } else { -delta };
                        *ratio = (*ratio + effective_delta).clamp(0.1, 0.9);
                        return true;
                    }
                    if in_first { first.adjust_ratio_at(target, delta) }
                    else { second.adjust_ratio_at(target, delta) }
                } else {
                    false
                }
            }
        }
    }

    /// Remove all leaves that reference the given window_id.
    /// Returns None if the entire tree is removed.
    pub fn remove_window(self, window_id: NodeId) -> Option<SplitTree> {
        match self {
            SplitTree::Leaf { window_id: wid, .. } => {
                if wid == window_id { None } else { Some(self) }
            }
            SplitTree::Split { direction, ratio, first, second } => {
                let new_first = first.remove_window(window_id);
                let new_second = second.remove_window(window_id);
                match (new_first, new_second) {
                    (Some(f), Some(s)) => Some(SplitTree::Split { direction, ratio, first: Box::new(f), second: Box::new(s) }),
                    (Some(f), None) => Some(f),
                    (None, Some(s)) => Some(s),
                    (None, None) => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_roundtrip() {
        let msg = ClientMsg::Input { data: vec![1, 2, 3] };
        let json = serde_json::to_vec(&msg).unwrap();
        let decoded: ClientMsg = serde_json::from_slice(&json).unwrap();
        match decoded {
            ClientMsg::Input { data } => assert_eq!(data, vec![1, 2, 3]),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_msg_tab_state_roundtrip() {
        let tree = SplitTree::Split {
            direction: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::Leaf { pane_id: 0, window_id: 2 }),
            second: Box::new(SplitTree::Leaf { pane_id: 1, window_id: 3 }),
        };
        let msg = ServerMsg::TabState {
            projects: vec![TabEntry { id: 1, name: "proj".into(), ai_status: None }],
            groups: vec![],
            windows: vec![],
            active_project: Some(1),
            active_group: None,
            active_window: None,
            layout_mode: LayoutMode::Tiled,
            split_tree: Some(tree),
            active_pane: Some(0),
        };
        let json = serde_json::to_vec(&msg).unwrap();
        let decoded: ServerMsg = serde_json::from_slice(&json).unwrap();
        match decoded {
            ServerMsg::TabState { projects, active_project, layout_mode, split_tree, active_pane, .. } => {
                assert_eq!(projects.len(), 1);
                assert_eq!(projects[0].name, "proj");
                assert_eq!(active_project, Some(1));
                assert_eq!(layout_mode, LayoutMode::Tiled);
                assert!(split_tree.is_some());
                assert_eq!(split_tree.unwrap().leaves().len(), 2);
                assert_eq!(active_pane, Some(0));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn write_read_msg_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let msg = ClientMsg::Resize { cols: 80, rows: 24 };
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).await.unwrap();

            let mut cursor = std::io::Cursor::new(buf);
            let decoded: Option<ClientMsg> = read_msg(&mut cursor).await.unwrap();
            match decoded.unwrap() {
                ClientMsg::Resize { cols, rows } => {
                    assert_eq!(cols, 80);
                    assert_eq!(rows, 24);
                }
                _ => panic!("wrong variant"),
            }
        });
    }

    #[test]
    fn read_msg_rejects_oversized() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let len = (20 * 1024 * 1024u32).to_be_bytes();
            let mut buf = Vec::new();
            buf.extend_from_slice(&len);
            buf.extend_from_slice(&[0u8; 64]);

            let mut cursor = std::io::Cursor::new(buf);
            let result: anyhow::Result<Option<ClientMsg>> = read_msg(&mut cursor).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn split_tree_leaves() {
        let tree = SplitTree::Split {
            direction: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::Leaf { pane_id: 0, window_id: 10 }),
            second: Box::new(SplitTree::Leaf { pane_id: 1, window_id: 20 }),
        };
        let leaves = tree.leaves();
        assert_eq!(leaves, vec![(0, 10), (1, 20)]);
    }

    #[test]
    fn split_tree_remove_leaf() {
        let tree = SplitTree::Split {
            direction: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::Leaf { pane_id: 0, window_id: 10 }),
            second: Box::new(SplitTree::Leaf { pane_id: 1, window_id: 20 }),
        };
        let result = tree.remove_leaf(0);
        assert!(result.is_some());
        let remaining = result.unwrap();
        assert_eq!(remaining.leaves(), vec![(1, 20)]);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

// Client → Server
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Send keyboard input to the active window
    Input { data: Vec<u8> },
    /// Terminal resized
    Resize { cols: u16, rows: u16 },
    /// Navigation commands
    SelectProject { id: NodeId },
    SelectGroup { id: NodeId },
    SelectWindow { id: NodeId },
    /// Create a new window in the active group
    NewWindow { name: Option<String> },
    /// Create a new group in the active project
    NewGroup { name: Option<String> },
    /// Create a new project
    NewProject { name: Option<String>, path: Option<std::path::PathBuf> },
    /// Request current state (sent on connect)
    Subscribe,
    /// Load a preset
    LoadPreset { name: String },
    /// List available presets
    ListPresets,
    /// Save active window's cwd as the project's default directory
    SetProjectDir,
    /// Save active window's cwd as the group's default directory
    SetGroupDir,
    /// Save current session tree as a preset
    SavePreset { name: Option<String>, force: bool },
    /// Cycle to the next window with an AI session (across all projects/groups)
    NextAiWindow,
    /// Cycle to the previous window with an AI session
    PrevAiWindow,
    /// Detach (clean disconnect)
    Detach,
    /// Move active window to a new project (named after window's cwd)
    MoveWindowToNewProject,
    /// Move active window to a new group (named after window's cwd)
    MoveWindowToNewGroup,
    /// Create a new group with an associated git worktree
    NewWorktreeGroup { branch: String },
    /// Request list of git branches for the active project
    ListBranches,
    /// Remove the active group's worktree and delete the group
    CloseGroup { force: bool },
    /// Rename a node (project, group, or window)
    Rename { id: NodeId, name: String },
    /// Rebase active group's branch onto main
    RebaseMain,
    /// Merge active group's worktree branch into main
    MergeIntoMain,
    /// Close the active window
    CloseWindow,
    /// Search all windows for text content
    SearchWindows { query: String },
    /// Toggle group layout mode (Stacked ↔ Tiled)
    ToggleLayout,
    /// Split the active pane horizontally or vertically
    SplitPane { direction: SplitDir },
    /// Close the active pane (unsplit); sibling takes parent's place
    CloseSplit,
    /// Swap the split direction (H↔V) at the active pane's parent
    SwapSplitDirection,
    /// Move focus between panes in tiled mode
    FocusPane { direction: PaneDirection },
    /// Send input to a specific window (used in tiled mode)
    InputToWindow { window_id: NodeId, data: Vec<u8> },
    /// Resize the active pane in tiled mode
    ResizePane { direction: PaneDirection },
    /// Cycle the focused pane's content to the next/prev window in group
    CyclePaneContent { forward: bool },
    /// Cycle the focused pane's content to the next/prev window across all groups/projects
    CyclePaneContentGlobal { forward: bool },
    /// Close a specific node by ID (window, group, or project)
    CloseNode { id: NodeId },
    /// Request full session tree (for tree nav)
    RequestTree,
    /// Shut down the server
    Shutdown,
    /// Hot reload: server serializes state, exec()s new binary
    Reload,
}

// Server → Client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Raw PTY output bytes for a window (client feeds to its own vt100 parser)
    PtyOutput { window_id: NodeId, data: Vec<u8> },
    /// Full screen dump as ANSI bytes (sent on attach for each window with content)
    ScreenDump { window_id: NodeId, data: Vec<u8> },
    /// Tab state update
    TabState {
        projects: Vec<TabEntry>,
        groups: Vec<TabEntry>,
        windows: Vec<TabEntry>,
        active_project: Option<NodeId>,
        active_group: Option<NodeId>,
        active_window: Option<NodeId>,
        layout_mode: LayoutMode,
        split_tree: Option<SplitTree>,
        active_pane: Option<u32>,
    },
    /// Error
    Error { message: String },
    /// Informational message (confirmations, etc.)
    Info { message: String },
    /// Window was created
    WindowCreated { id: NodeId, name: String, group_id: NodeId },
    /// List of git branches for branch picker
    BranchList { branches: Vec<String> },
    /// List of available presets
    PresetList { presets: Vec<String> },
    /// Preset already exists; client should confirm overwrite
    ConfirmOverwrite { name: String },
    /// Server is about to exec() for hot reload; clients should reconnect
    Reloading,
    /// Full session tree (for tree nav)
    FullTree {
        projects: Vec<TreeProject>,
        active_project: Option<NodeId>,
        active_group: Option<NodeId>,
        active_window: Option<NodeId>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabEntry {
    pub id: NodeId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_status: Option<crate::ai_detect::AiStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeProject {
    pub id: NodeId,
    pub name: String,
    pub groups: Vec<TreeGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeGroup {
    pub id: NodeId,
    pub name: String,
    pub windows: Vec<TreeWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeWindow {
    pub id: NodeId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_status: Option<crate::ai_detect::AiStatus>,
    /// Screen dump for preview (ANSI bytes)
    pub screen_data: Vec<u8>,
}

/// Socket path
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = std::path::PathBuf::from(runtime_dir).join("zmux");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("server.sock")
    } else {
        // SAFETY: getuid() is always safe to call — no preconditions, no failure modes
        let uid = unsafe { libc::getuid() };
        let dir = std::path::PathBuf::from(format!("/tmp/zmux-{}", uid));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("server.sock")
    }
}

/// Write a length-prefixed JSON message to an async writer
pub async fn write_msg<W: tokio::io::AsyncWriteExt + Unpin, T: Serialize>(
    writer: &mut W,
    msg: &T,
) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&json).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON message from an async reader
pub async fn read_msg<R: tokio::io::AsyncReadExt + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> anyhow::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        anyhow::bail!("Message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let msg = serde_json::from_slice(&buf)?;
    Ok(Some(msg))
}

