use serde::{Deserialize, Serialize};

pub type NodeId = u64;

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
    /// Remove the active group's worktree and delete the group
    CloseGroup { force: bool },
    /// Close the active window
    CloseWindow,
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
    },
    /// Error
    Error { message: String },
    /// Informational message (confirmations, etc.)
    Info { message: String },
    /// Window was created
    WindowCreated { id: NodeId, name: String, group_id: NodeId },
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

