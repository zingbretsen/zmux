mod ai_detect;
mod app;
mod client;
mod config;
mod protocol;
mod pty;
mod server;
mod ui;
mod worktree;

use anyhow::Result;
use app::{App, Mode, TabLevel};
use protocol::PaneDirection;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers, MouseButton, MouseEventKind, EnableMouseCapture, DisableMouseCapture},
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
        Mode::AiNav => handle_ai_nav_key(app, key).await,
        Mode::Rename => handle_rename_key(app, key).await,
        Mode::Copy => handle_copy_key(app, key).await,
        Mode::Search => handle_search_key(app, key).await,
        Mode::BranchInput => handle_branch_input_key(app, key).await,
        Mode::Help => {
            app.mode = Mode::Normal;
            Ok(())
        }
    }
}

async fn handle_normal_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('b') {
        app.mode = Mode::Nav;
        return Ok(());
    }

    // Pane focus navigation in tiled mode
    if app.is_tiled() && ctrl {
        let dir = match key.code {
            KeyCode::Char('h') => Some(PaneDirection::Left),
            KeyCode::Char('j') => Some(PaneDirection::Down),
            KeyCode::Char('k') => Some(PaneDirection::Up),
            KeyCode::Char('l') => Some(PaneDirection::Right),
            _ => None,
        };
        if let Some(direction) = dir {
            app.conn.focus_pane(direction).await?;
            return Ok(());
        }
    }

    if let Some(bytes) = key_to_bytes(key) {
        if app.is_tiled() {
            // In tiled mode, send input to the active (focused) window
            if let Some(wid) = app.active_window {
                app.conn.send_input_to_window(wid, bytes).await?;
            }
        } else {
            app.conn.send_input(bytes).await?;
        }
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

        KeyCode::Char('x') => {
            app.conn.close_window().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('c') => {
            app.conn.new_window(None).await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('g') => {
            app.conn.move_window_to_new_group().await?;
            app.mode = Mode::Normal;
        }
        KeyCode::Char('p') => {
            app.conn.move_window_to_new_project().await?;
            app.mode = Mode::Normal;
        }

        // Rename the focused tab
        KeyCode::Char('r') => {
            let target = match app.tab_focus {
                TabLevel::Project => app.active_project,
                TabLevel::Group => app.active_group,
                TabLevel::Window => app.active_window,
            };
            if let Some(id) = target {
                // Pre-fill with current name
                let current_name = match app.tab_focus {
                    TabLevel::Project => app.projects.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                    TabLevel::Group => app.groups.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                    TabLevel::Window => app.windows.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                };
                app.rename_buf = current_name.unwrap_or_default();
                app.rename_target = Some(id);
                app.mode = Mode::Rename;
            }
        }

        // Enter AI navigation mode
        KeyCode::Char('a') => {
            app.conn.next_ai_window().await?;
            app.mode = Mode::AiNav;
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

        // Worktree: new group from branch
        KeyCode::Char('w') => {
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.conn.list_branches().await?;
            app.mode = Mode::BranchInput;
        }

        // Rebase onto main
        KeyCode::Char('R') => {
            app.conn.rebase_main().await?;
            app.mode = Mode::Normal;
        }

        // Merge worktree branch into main
        KeyCode::Char('M') => {
            app.conn.merge_into_main().await?;
            app.mode = Mode::Normal;
        }

        // Search across windows
        KeyCode::Char('/') => {
            app.rename_buf.clear();
            app.mode = Mode::Search;
        }

        // Enter copy mode
        KeyCode::Char('[') => {
            app.copy_scroll_offset = 0;
            app.mode = Mode::Copy;
        }

        // Close group (with worktree cleanup)
        KeyCode::Char('X') => {
            app.conn.close_group(false).await?;
            app.mode = Mode::Normal;
        }

        // Help
        KeyCode::Char('?') => {
            app.mode = Mode::Help;
        }

        // Toggle layout mode (Stacked ↔ Tiled)
        KeyCode::Char('t') => {
            app.conn.toggle_layout().await?;
        }

        // Cycle tile layout algorithm
        KeyCode::Char('T') => {
            app.conn.cycle_layout().await?;
        }

        // Toggle current window in/out of tile set
        KeyCode::Char('m') => {
            if let Some(wid) = app.active_window {
                app.conn.toggle_tile(wid).await?;
            }
        }

        _ => {}
    }
    Ok(())
}

async fn handle_ai_nav_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Char('l') | KeyCode::Right => {
            app.conn.next_ai_window().await?;
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.conn.prev_ai_window().await?;
        }
        // Press 'a' again to go to next
        KeyCode::Char('a') => {
            app.conn.next_ai_window().await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_rename_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.rename_target = None;
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            if let Some(id) = app.rename_target.take() {
                if !app.rename_buf.is_empty() {
                    app.conn.rename(id, app.rename_buf.clone()).await?;
                }
            }
            app.rename_buf.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_branch_input_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            // Use selected branch if one is highlighted, otherwise use typed text
            let branch = if let Some(idx) = app.branch_selected {
                let filtered = app.filtered_branches();
                filtered.get(idx).map(|s| s.to_string())
            } else {
                None
            };
            let branch = branch.unwrap_or_else(|| app.rename_buf.clone());
            if !branch.is_empty() {
                app.conn.new_worktree_group(branch).await?;
            }
            app.rename_buf.clear();
            app.branch_candidates.clear();
            app.branch_selected = None;
            app.mode = Mode::Normal;
        }
        KeyCode::Down => {
            let count = app.filtered_branches().len();
            if count > 0 {
                app.branch_selected = Some(match app.branch_selected {
                    None => 0,
                    Some(i) => (i + 1).min(count - 1),
                });
            }
        }
        KeyCode::Up => {
            app.branch_selected = match app.branch_selected {
                None | Some(0) => None,
                Some(i) => Some(i - 1),
            };
        }
        KeyCode::Tab => {
            // Autocomplete: fill input with selected branch
            if let Some(idx) = app.branch_selected {
                let filtered = app.filtered_branches();
                if let Some(name) = filtered.get(idx) {
                    app.rename_buf = name.to_string();
                    app.branch_selected = None;
                }
            }
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
            app.branch_selected = None;
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
            app.branch_selected = None;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_search_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.rename_buf.clear();
            app.mode = Mode::Nav;
        }
        KeyCode::Enter => {
            if !app.rename_buf.is_empty() {
                app.conn.search_windows(app.rename_buf.clone()).await?;
            }
            app.rename_buf.clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.rename_buf.pop();
        }
        KeyCode::Char(c) => {
            app.rename_buf.push(c);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_copy_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let half_page = (app.last_size.1 / 2) as usize;

    let set_scrollback = |app: &mut App, offset: usize| {
        if let Some(wid) = app.active_window {
            if let Some(parser) = app.parser_for(wid) {
                parser.lock().unwrap().set_scrollback(offset);
            }
        }
    };

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.copy_scroll_offset = 0;
            set_scrollback(app, 0);
            app.mode = Mode::Normal;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(1).min(1000);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(1);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('u') if ctrl => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_add(half_page).min(1000);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('d') if ctrl => {
            app.copy_scroll_offset = app.copy_scroll_offset.saturating_sub(half_page);
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('g') => {
            app.copy_scroll_offset = 1000;
            set_scrollback(app, app.copy_scroll_offset);
        }
        KeyCode::Char('G') => {
            app.copy_scroll_offset = 0;
            set_scrollback(app, 0);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> Result<()> {
    // Tab bar clicks work in any mode
    if mouse.row == 0 {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(click) = ui::tab_click_at(app, mouse.column) {
                match click {
                    ui::TabClick::Project(idx) => {
                        if let Some(entry) = app.projects.get(idx) {
                            app.conn.select_project(entry.id).await?;
                        }
                    }
                    ui::TabClick::Group(idx) => {
                        if let Some(entry) = app.groups.get(idx) {
                            app.conn.select_group(entry.id).await?;
                        }
                    }
                    ui::TabClick::Window(idx) => {
                        if let Some(entry) = app.windows.get(idx) {
                            app.conn.select_window(entry.id).await?;
                        }
                    }
                }
                app.mode = Mode::Normal;
            }
            return Ok(());
        }
    }

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
