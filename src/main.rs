mod ai_detect;
mod app;
mod client;
mod config;
mod input;
mod protocol;
mod pty;
mod server;
mod session;
mod ui;
mod worktree;

use anyhow::Result;
use app::App;
use crossterm::{
    event::{Event, EventStream, EnableMouseCapture, DisableMouseCapture},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures::StreamExt;
use ratatui::prelude::*;
use std::io::{self, stdout};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str());

    match cmd {
        Some("server") => {
            let preset = args.get(2).map(|s| s.as_str());
            server::run_server(preset).await
        }
        Some("kill") => {
            let sock_path = protocol::socket_path();
            match tokio::net::UnixStream::connect(&sock_path).await {
                Ok(stream) => {
                    let (reader, mut writer) = tokio::io::split(stream);
                    let _ = reader;
                    protocol::write_msg(&mut writer, &protocol::ClientMsg::Shutdown).await?;
                    println!("zmux server stopped");
                }
                Err(_) => {
                    println!("No zmux server running");
                }
            }
            Ok(())
        }
        Some("list") => {
            let presets = config::list_presets()?;
            if presets.is_empty() {
                println!("No presets found. Create one at ~/.config/zmux/presets/<name>.toml");
            } else {
                println!("Available presets:");
                for p in presets {
                    println!("  {}", p);
                }
            }
            Ok(())
        }
        _ => {
            let preset = cmd;
            ensure_server(preset).await?;
            run_client().await
        }
    }
}

async fn ensure_server(preset: Option<&str>) -> Result<()> {
    let sock_path = protocol::socket_path();

    if tokio::net::UnixStream::connect(&sock_path).await.is_ok() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("server");
    if let Some(name) = preset {
        cmd.arg(name);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    cmd.spawn()?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if sock_path.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(());
        }
    }
    anyhow::bail!("Server failed to start within 5 seconds")
}

fn restore_terminal() {
    let _ = stdout().execute(DisableMouseCapture);
    let _ = terminal::disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);
}

async fn run_client() -> Result<()> {
    let conn = client::ClientConnection::connect().await?;

    // Install panic hook to restore terminal on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    terminal::enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let size = terminal.size()?;
    let mut app = App::new(conn, size.height, size.width).await?;

    let result = run_loop(&mut terminal, &mut app).await;

    restore_terminal();

    result
}

async fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let mut event_stream = EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        tokio::select! {
            msg = app.conn.msg_rx.recv() => {
                match msg {
                    Some(m) => app.apply_server_msg(m),
                    None => break, // Server disconnected
                }
                while let Ok(m) = app.conn.msg_rx.try_recv() {
                    app.apply_server_msg(m);
                }
            }
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => input::handle_key(app, &key).await?,
                    Some(Ok(Event::Resize(cols, rows))) => app.resize(cols, rows).await?,
                    Some(Ok(Event::Mouse(mouse))) => input::handle_mouse(app, &mouse).await?,
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }

        if app.should_detach || app.should_quit {
            if app.should_detach {
                let _ = app.conn.detach().await;
            }
            break;
        }
    }
    Ok(())
}
