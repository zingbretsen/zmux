use serde::{Deserialize, Serialize};

pub type NodeId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    Stacked,
    Tiled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileLayout {
    EqualColumns,
    EqualRows,
    MainLeft,
    Grid,
}

impl TileLayout {
    pub fn next(self) -> Self {
        match self {
            TileLayout::EqualColumns => TileLayout::EqualRows,
            TileLayout::EqualRows => TileLayout::MainLeft,
            TileLayout::MainLeft => TileLayout::Grid,
            TileLayout::Grid => TileLayout::EqualColumns,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TileLayout::EqualColumns => "columns",
            TileLayout::EqualRows => "rows",
            TileLayout::MainLeft => "main-left",
            TileLayout::Grid => "grid",
        }
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
    NewProject { name: Option<String> },
    /// Request current state (sent on connect)
    Subscribe,
    /// Load a preset
    LoadPreset { name: String },
    /// Save active window's cwd as the project's default directory
    SetProjectDir,
    /// Save active window's cwd as the group's default directory
    SetGroupDir,
    /// Save current session tree as a preset
    SavePreset { name: Option<String> },
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
    /// Cycle tile layout algorithm
    CycleLayout,
    /// Toggle a window in/out of the tile set
    ToggleTile { id: NodeId },
    /// Move focus between panes in tiled mode
    FocusPane { direction: PaneDirection },
    /// Send input to a specific window (used in tiled mode)
    InputToWindow { window_id: NodeId, data: Vec<u8> },
    /// Shut down the server
    Shutdown,
}

// Server → Client
#[derive(Debug, Serialize, Deserialize)]
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
        tile_layout: TileLayout,
        tiled_windows: Vec<NodeId>,
    },
    /// Error
    Error { message: String },
    /// Informational message (confirmations, etc.)
    Info { message: String },
    /// Window was created
    WindowCreated { id: NodeId, name: String, group_id: NodeId },
    /// List of git branches for branch picker
    BranchList { branches: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabEntry {
    pub id: NodeId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_status: Option<crate::ai_detect::AiStatus>,
}

/// Socket path
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = std::path::PathBuf::from(runtime_dir).join("zmux");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("server.sock")
    } else {
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

