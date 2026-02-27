use crate::protocol::{self, ClientMsg, NodeId, ServerMsg};
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

    pub async fn new_group(&self, name: Option<String>) -> Result<()> {
        self.send(ClientMsg::NewGroup { name }).await
    }

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

    pub async fn detach(&self) -> Result<()> {
        self.send(ClientMsg::Detach).await
    }
}
