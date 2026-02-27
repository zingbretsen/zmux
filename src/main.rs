mod app;
mod client;
mod config;
mod protocol;
mod pty;
mod server;
mod ui;

use anyhow::Result;
use app::{App, Mode, TabLevel};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture},
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

async fn run_client() -> Result<()> {
    let conn = client::ClientConnection::connect().await?;

    terminal::enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let size = terminal.size()?;
    let mut app = App::new(conn, size.height, size.width).await?;

    let result = run_loop(&mut terminal, &mut app).await;

    stdout().execute(DisableMouseCapture)?;
    terminal::disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

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
                    Some(Ok(Event::Key(key))) => handle_key(app, &key).await?,
                    Some(Ok(Event::Resize(cols, rows))) => app.resize(cols, rows).await?,
                    Some(Ok(Event::Mouse(mouse))) => handle_mouse(app, &mouse).await?,
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

async fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('q') {
        app.should_quit = true;
        return Ok(());
    }

    match app.mode {
        Mode::Normal => handle_normal_key(app, key).await,
        Mode::Nav => handle_nav_key(app, key).await,
    }
}

async fn handle_normal_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('b') {
        app.mode = Mode::Nav;
        return Ok(());
    }

    if let Some(bytes) = key_to_bytes(key) {
        app.conn.send_input(bytes).await?;
    }
    Ok(())
}

async fn handle_nav_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,

        KeyCode::Char('d') => {
            app.should_detach = true;
        }

        KeyCode::Char('k') | KeyCode::Up => {
            app.tab_focus = match app.tab_focus {
                TabLevel::Window => TabLevel::Group,
                TabLevel::Group => TabLevel::Project,
                TabLevel::Project => TabLevel::Project,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.tab_focus = match app.tab_focus {
                TabLevel::Project => TabLevel::Group,
                TabLevel::Group => TabLevel::Window,
                TabLevel::Window => TabLevel::Window,
            };
        }
        KeyCode::Char('h') | KeyCode::Left => app.prev_tab().await?,
        KeyCode::Char('l') | KeyCode::Right => app.next_tab().await?,

        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            app.select_tab_by_index(idx).await?;
        }

        KeyCode::Char('c') => {
            app.conn.new_window(None).await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('g') => {
            app.conn.new_group(None).await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('p') => {
            app.conn.new_project(None).await?;
            app.mode = Mode::Normal;
        }

        // Save current cwd as group dir (s) or project dir (S)
        KeyCode::Char('S') => {
            app.conn.set_project_dir().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('s') => {
            app.conn.set_group_dir().await?;
            app.mode = Mode::Normal;
        }

        // Save preset
        KeyCode::Char('W') => {
            app.conn.save_preset(None).await?;
            app.mode = Mode::Normal;
        }

        _ => {}
    }
    Ok(())
}

async fn handle_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> Result<()> {
    if app.mode != Mode::Normal {
        return Ok(());
    }
    let bytes = match mouse.kind {
        MouseEventKind::ScrollUp => b"\x1b[5~".to_vec(),
        MouseEventKind::ScrollDown => b"\x1b[6~".to_vec(),
        _ => return Ok(()),
    };
    app.conn.send_input(bytes).await?;
    Ok(())
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                Some(vec![ctrl_byte])
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                Some(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
