use crate::protocol::{self, ClientMsg, NodeId, PaneDirection, ServerMsg};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};

pub struct ClientConnection {
    writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    pub msg_rx: mpsc::UnboundedReceiver<ServerMsg>,
}

impl ClientConnection {
    pub async fn connect() -> Result<Self> {
        let sock_path = protocol::socket_path();
        let stream = UnixStream::connect(&sock_path).await?;
        let (reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(writer));

        let (msg_tx, msg_rx) = mpsc::unbounded_channel();

        // Reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            loop {
                match protocol::read_msg::<_, ServerMsg>(&mut reader).await {
                    Ok(Some(msg)) => {
                        if msg_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break, // Server disconnected
                    Err(_) => break,
                }
            }
        });

        let conn = ClientConnection { writer, msg_rx };
        conn.send(ClientMsg::Subscribe).await?;
        Ok(conn)
    }

    pub async fn send(&self, msg: ClientMsg) -> Result<()> {
        let mut w = self.writer.lock().await;
        protocol::write_msg(&mut *w, &msg).await
    }

    pub async fn send_input(&self, data: Vec<u8>) -> Result<()> {
        self.send(ClientMsg::Input { data }).await
    }

    pub async fn send_resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.send(ClientMsg::Resize { cols, rows }).await
    }

    pub async fn select_project(&self, id: NodeId) -> Result<()> {
        self.send(ClientMsg::SelectProject { id }).await
    }

    pub async fn select_group(&self, id: NodeId) -> Result<()> {
        self.send(ClientMsg::SelectGroup { id }).await
    }

    pub async fn select_window(&self, id: NodeId) -> Result<()> {
        self.send(ClientMsg::SelectWindow { id }).await
    }

    pub async fn new_window(&self, name: Option<String>) -> Result<()> {
        self.send(ClientMsg::NewWindow { name }).await
    }

    #[allow(dead_code)]
    pub async fn new_group(&self, name: Option<String>) -> Result<()> {
        self.send(ClientMsg::NewGroup { name }).await
    }

    #[allow(dead_code)]
    pub async fn new_project(&self, name: Option<String>) -> Result<()> {
        self.send(ClientMsg::NewProject { name }).await
    }

    pub async fn set_project_dir(&self) -> Result<()> {
        self.send(ClientMsg::SetProjectDir).await
    }

    pub async fn set_group_dir(&self) -> Result<()> {
        self.send(ClientMsg::SetGroupDir).await
    }

    pub async fn save_preset(&self, name: Option<String>) -> Result<()> {
        self.send(ClientMsg::SavePreset { name }).await
    }

    pub async fn next_ai_window(&self) -> Result<()> {
        self.send(ClientMsg::NextAiWindow).await
    }

    pub async fn prev_ai_window(&self) -> Result<()> {
        self.send(ClientMsg::PrevAiWindow).await
    }

    pub async fn move_window_to_new_project(&self) -> Result<()> {
        self.send(ClientMsg::MoveWindowToNewProject).await
    }

    pub async fn move_window_to_new_group(&self) -> Result<()> {
        self.send(ClientMsg::MoveWindowToNewGroup).await
    }

    pub async fn rename(&self, id: NodeId, name: String) -> Result<()> {
        self.send(ClientMsg::Rename { id, name }).await
    }

    pub async fn close_window(&self) -> Result<()> {
        self.send(ClientMsg::CloseWindow).await
    }

    pub async fn rebase_main(&self) -> Result<()> {
        self.send(ClientMsg::RebaseMain).await
    }

    pub async fn merge_into_main(&self) -> Result<()> {
        self.send(ClientMsg::MergeIntoMain).await
    }

    pub async fn new_worktree_group(&self, branch: String) -> Result<()> {
        self.send(ClientMsg::NewWorktreeGroup { branch }).await
    }

    pub async fn list_branches(&self) -> Result<()> {
        self.send(ClientMsg::ListBranches).await
    }

    pub async fn list_presets(&self) -> Result<()> {
        self.send(ClientMsg::ListPresets).await
    }

    pub async fn load_preset(&self, name: String) -> Result<()> {
        self.send(ClientMsg::LoadPreset { name }).await
    }

    pub async fn close_group(&self, force: bool) -> Result<()> {
        self.send(ClientMsg::CloseGroup { force }).await
    }

    pub async fn search_windows(&self, query: String) -> Result<()> {
        self.send(ClientMsg::SearchWindows { query }).await
    }

    pub async fn detach(&self) -> Result<()> {
        self.send(ClientMsg::Detach).await
    }

    pub async fn toggle_layout(&self) -> Result<()> {
        self.send(ClientMsg::ToggleLayout).await
    }

    pub async fn cycle_layout(&self) -> Result<()> {
        self.send(ClientMsg::CycleLayout).await
    }

    pub async fn toggle_tile(&self, id: NodeId) -> Result<()> {
        self.send(ClientMsg::ToggleTile { id }).await
    }

    pub async fn focus_pane(&self, direction: PaneDirection) -> Result<()> {
        self.send(ClientMsg::FocusPane { direction }).await
    }

    pub async fn resize_pane(&self, direction: PaneDirection) -> Result<()> {
        self.send(ClientMsg::ResizePane { direction }).await
    }

    pub async fn cycle_pane_content(&self, forward: bool) -> Result<()> {
        self.send(ClientMsg::CyclePaneContent { forward }).await
    }

    pub async fn send_input_to_window(&self, window_id: NodeId, data: Vec<u8>) -> Result<()> {
        self.send(ClientMsg::InputToWindow { window_id, data }).await
    }

    pub async fn reload(&self) -> Result<()> {
        self.send(ClientMsg::Reload).await
    }

    pub async fn close_node(&self, id: NodeId) -> Result<()> {
        self.send(ClientMsg::CloseNode { id }).await
    }

    pub async fn request_tree(&self) -> Result<()> {
        self.send(ClientMsg::RequestTree).await
    }
}
