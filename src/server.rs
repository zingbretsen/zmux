use crate::config;
use crate::protocol::{self, ClientMsg, LayoutMode, NodeId, ServerMsg};
use crate::pty::PtyHandle;
use crate::session::{build_layout_tree, GroupNode, Node, ProjectNode, SessionTree, WindowNode};
use crate::worktree;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

// ── Lifecycle hooks ───────────────────────────────────────────────────

async fn run_hook(command: &str, working_dir: &std::path::Path) {
    use tokio::process::Command;
    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) if !status.success() => {
            warn!("Hook failed (exit {}): {}", status.code().unwrap_or(-1), command);
        }
        Err(e) => {
            warn!("Hook failed to execute: {} — {}", command, e);
        }
        _ => {}
    }
}

// ── Serializable reload state ─────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct ReloadState {
    listener_fd: RawFd,
    session: SerializedSession,
    next_client_id: u64,
    preset_name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SerializedSession {
    nodes: Vec<(NodeId, SerializedNode)>,
    root_children: Vec<NodeId>,
    next_id: NodeId,
    active_project: Option<NodeId>,
    active_group: Option<NodeId>,
    active_window: Option<NodeId>,
    shell: Option<String>,
}

#[derive(Serialize, Deserialize)]
enum SerializedNode {
    Project {
        name: String,
        working_dir: String,
        children: Vec<NodeId>,
        #[serde(default)]
        env_profile: Option<String>,
        #[serde(default)]
        on_project_stop: Option<String>,
    },
    Group {
        name: String,
        parent: NodeId,
        children: Vec<NodeId>,
        working_dir: Option<String>,
        worktree_path: Option<String>,
        layout_mode: LayoutMode,
        split_tree: Option<protocol::SplitTree>,
        next_pane_id: u32,
        active_pane: Option<u32>,
        #[serde(default)]
        env_profile: Option<String>,
        #[serde(default)]
        layout_preset: Option<config::LayoutPreset>,
    },
    Window {
        name: String,
        parent: NodeId,
        master_fd: RawFd,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
        screen_dump: Vec<u8>,
        #[serde(default)]
        env_profile: Option<String>,
        #[serde(default)]
        on_pane_close: Option<String>,
    },
}

// ── Server state ──────────────────────────────────────────────────────

struct ServerState {
    session: SessionTree,
    clients: HashMap<u64, mpsc::UnboundedSender<ServerMsg>>,
    next_client_id: u64,
    client_sizes: HashMap<u64, (u16, u16)>,
    preset_name: Option<String>,
}

impl ServerState {
    fn broadcast(&self, msg: ServerMsg) {
        for tx in self.clients.values() {
            let _ = tx.send(msg.clone());
        }
    }

    fn effective_size(&self) -> (u16, u16) {
        if self.client_sizes.is_empty() {
            return (80, 24);
        }
        let cols = self.client_sizes.values().map(|s| s.0).min().unwrap_or(80);
        let rows = self.client_sizes.values().map(|s| s.1).min().unwrap_or(24);
        (cols, rows)
    }
}

// ── Shared helpers ────────────────────────────────────────────────────

fn spawn_pty_forwarder(
    state: Arc<Mutex<ServerState>>,
    mut pty_rx: mpsc::UnboundedReceiver<(NodeId, Vec<u8>)>,
) {
    tokio::spawn(async move {
        while let Some((window_id, data)) = pty_rx.recv().await {
            if data.is_empty() {
                let mut st = state.lock().await;
                // Extract on_pane_close hook before removing
                let hook_info = if let Some(Node::Window(w)) = st.session.nodes.get(&window_id) {
                    w.on_pane_close.as_ref().map(|cmd| {
                        let dir = st.session.window_working_dir(w.parent);
                        (cmd.clone(), dir)
                    })
                } else {
                    None
                };
                st.session.remove_window(window_id);
                let tab = st.session.tab_state();
                st.broadcast(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                // Fire on_pane_close hook (fire-and-forget)
                if let Some((cmd, dir)) = hook_info {
                    drop(st);
                    tokio::spawn(async move { run_hook(&cmd, &dir).await });
                }
                continue;
            }
            let st = state.lock().await;
            if st.session.active_window == Some(window_id)
                || st.session.is_tiled_window(window_id)
            {
                st.broadcast(ServerMsg::PtyOutput { window_id, data });
            }
        }
    });
}

fn spawn_ai_poller(state: Arc<Mutex<ServerState>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            interval.tick().await;
            let mut st = state.lock().await;
            let changed = st.session.poll_ai_status();
            if changed {
                let tab = st.session.tab_state();
                st.broadcast(tab);
            }
        }
    });
}

/// Global reload channel. Client handlers send () on this to trigger reload.
static RELOAD_TX: std::sync::Mutex<Option<mpsc::UnboundedSender<()>>> =
    std::sync::Mutex::new(None);

fn trigger_reload() {
    if let Some(tx) = RELOAD_TX.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

async fn run_accept_loop(
    listener: UnixListener,
    state: Arc<Mutex<ServerState>>,
    pty_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
    mut reload_rx: mpsc::UnboundedReceiver<()>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = Arc::clone(&state);
                let pty_tx = pty_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state, pty_tx).await {
                        error!("Client error: {}", e);
                    }
                });
            }
            _ = reload_rx.recv() => {
                info!("Reload requested, performing exec()");
                if let Err(e) = perform_reload(state, &listener).await {
                    error!("Reload failed: {}", e);
                    // Server continues running on failure
                    return Err(e);
                }
                unreachable!("exec should have replaced the process");
            }
        }
    }
}

// ── Setup logging ─────────────────────────────────────────────────────

fn setup_logging() {
    let log_dir = protocol::socket_path().parent().unwrap().to_path_buf();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "zmux.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // _guard is intentionally leaked so the writer stays alive for the process lifetime
    std::mem::forget(_guard);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

// ── Normal server start ───────────────────────────────────────────────

pub async fn run_server(preset_name: Option<&str>) -> Result<()> {
    setup_logging();

    let sock_path = protocol::socket_path();

    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)?;
    info!("zmux server listening on {}", sock_path.display());

    let user_config = config::load_config();
    let mut session = SessionTree::new();
    session.shell = user_config.shell;
    let (pty_tx, pty_rx) = mpsc::unbounded_channel::<(NodeId, Vec<u8>)>();

    let default_rows: u16 = 24;
    let default_cols: u16 = 80;

    if let Some(name) = preset_name {
        let preset = config::load_preset(name)?;
        for proj_preset in &preset.projects {
            let proj_dir = PathBuf::from(&proj_preset.path);
            // Run lifecycle hooks before creating windows
            if let Some(ref cmd) = proj_preset.on_project_start {
                run_hook(cmd, &proj_dir).await;
            }
            if config::is_first_start(&proj_preset.name) {
                if let Some(ref cmd) = proj_preset.on_project_first_start {
                    run_hook(cmd, &proj_dir).await;
                }
            } else if let Some(ref cmd) = proj_preset.on_project_restart {
                run_hook(cmd, &proj_dir).await;
            }

            let project_id = session.find_project_by_name(&proj_preset.name)
                .unwrap_or_else(|| {
                    session.add_project(proj_preset.name.clone(), proj_dir.clone())
                });
            if proj_preset.env_profile.is_some() {
                session.set_env_profile(project_id, proj_preset.env_profile.clone());
            }
            for grp_preset in &proj_preset.groups {
                let group_id = if let Some(existing) = session.find_group_by_name(project_id, &grp_preset.name) {
                    existing
                } else {
                    let group_dir = grp_preset.path.as_ref().map(|p| PathBuf::from(p));
                    let wt_path = grp_preset.worktree_branch.as_ref().and_then(|branch| {
                        let proj_dir = PathBuf::from(&proj_preset.path);
                        worktree::create(&proj_dir, branch).ok()
                    });
                    let working_dir = group_dir.or_else(|| wt_path.clone());
                    session.add_group(
                        project_id,
                        grp_preset.name.clone(),
                        working_dir,
                        wt_path,
                    )
                };
                if grp_preset.env_profile.is_some() {
                    session.set_env_profile(group_id, grp_preset.env_profile.clone());
                }
                let mut group_window_ids = Vec::new();
                if grp_preset.windows.is_empty() {
                    if session.find_window_by_name(group_id, "shell").is_none() {
                        let wid = session.add_window(
                            group_id,
                            "shell".to_string(),
                            default_rows,
                            default_cols,
                            pty_tx.clone(),
                            None,
                            None,
                            None,
                            proj_preset.pre_window.clone(),
                            None,
                        )?;
                        group_window_ids.push(wid);
                    }
                } else {
                    for win_preset in &grp_preset.windows {
                        if session.find_window_by_name(group_id, &win_preset.name).is_none() {
                            let wid = session.add_window(
                                group_id,
                                win_preset.name.clone(),
                                default_rows,
                                default_cols,
                                pty_tx.clone(),
                                win_preset.command.clone(),
                                win_preset.path.as_ref().map(PathBuf::from),
                                win_preset.env_profile.clone(),
                                proj_preset.pre_window.clone(),
                                win_preset.on_pane_close.clone(),
                            )?;
                            // Write on_pane_open after command
                            if let Some(ref hook) = win_preset.on_pane_open {
                                if let Some(Node::Window(w)) = session.nodes.get_mut(&wid) {
                                    let _ = w.pty.write(format!("{}\n", hook).as_bytes());
                                }
                            }
                            group_window_ids.push(wid);
                        }
                    }
                }
                // Apply layout preset if specified
                if let Some(ref layout) = grp_preset.layout {
                    if let Some(Node::Group(g)) = session.nodes.get_mut(&group_id) {
                        let window_ids: Vec<NodeId> = g.children.clone();
                        if window_ids.len() > 1 {
                            let panes: Vec<(u32, NodeId)> = window_ids.iter().enumerate()
                                .map(|(i, &wid)| (i as u32, wid)).collect();
                            g.layout_mode = LayoutMode::Tiled;
                            g.split_tree = build_layout_tree(&panes, layout);
                            g.next_pane_id = panes.len() as u32;
                            g.active_pane = Some(0);
                            g.layout_preset = Some(layout.clone());
                        } else if window_ids.len() == 1 {
                            g.layout_mode = LayoutMode::Tiled;
                            g.split_tree = Some(protocol::SplitTree::Leaf { pane_id: 0, window_id: window_ids[0] });
                            g.next_pane_id = 1;
                            g.active_pane = Some(0);
                            g.layout_preset = Some(layout.clone());
                        }
                    }
                }
            }
            // Apply startup focus
            if let Some(ref startup_group) = proj_preset.startup_group {
                if let Some(gid) = session.find_group_by_name(project_id, startup_group) {
                    session.select_group(gid);
                    if let Some(ref startup_window) = proj_preset.startup_window {
                        if let Some(wid) = session.find_window_by_name(gid, startup_window) {
                            session.select_window(wid);
                        }
                    }
                }
            } else if let Some(ref startup_window) = proj_preset.startup_window {
                // Search all groups for the named window
                if let Some(Node::Project(p)) = session.nodes.get(&project_id) {
                    let groups = p.children.clone();
                    for gid in groups {
                        if let Some(wid) = session.find_window_by_name(gid, startup_window) {
                            session.select_group(gid);
                            session.select_window(wid);
                            break;
                        }
                    }
                }
            }
            // Store on_project_stop hook on the project node
            if proj_preset.on_project_stop.is_some() {
                if let Some(Node::Project(p)) = session.nodes.get_mut(&project_id) {
                    p.on_project_stop = proj_preset.on_project_stop.clone();
                }
            }
            if proj_preset.groups.is_empty() {
                if session.find_group_by_name(project_id, "default").is_none() {
                    let group_id =
                        session.add_group(project_id, "default".to_string(), None, None);
                    session.add_window(
                        group_id,
                        "shell".to_string(),
                        default_rows,
                        default_cols,
                        pty_tx.clone(),
                        None,
                        None,
                        None,
                        proj_preset.pre_window.clone(),
                        None,
                    )?;
                }
            }
        }
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let dir_name = cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        let project_id = session.add_project(dir_name, cwd);
        let group_id = session.add_group(project_id, "default".to_string(), None, None);
        session.add_window(
            group_id,
            "shell".to_string(),
            default_rows,
            default_cols,
            pty_tx.clone(),
            None,
            None,
            None,
            None,
            None,
        )?;
    }

    let state = Arc::new(Mutex::new(ServerState {
        session,
        clients: HashMap::new(),
        next_client_id: 0,
        client_sizes: HashMap::new(),
        preset_name: preset_name.map(|s| s.to_string()),
    }));

    let (reload_tx, reload_rx) = mpsc::unbounded_channel::<()>();
    RELOAD_TX.lock().unwrap().replace(reload_tx);

    spawn_pty_forwarder(Arc::clone(&state), pty_rx);
    spawn_ai_poller(Arc::clone(&state));

    run_accept_loop(listener, state, pty_tx, reload_rx).await
}

// ── Restore server after exec() ───────────────────────────────────────

pub async fn run_server_restore(state_path: &str) -> Result<()> {
    setup_logging();
    info!("zmux server restoring from {}", state_path);

    let json = std::fs::read(state_path)?;
    let reload_state: ReloadState = serde_json::from_slice(&json)?;
    let _ = std::fs::remove_file(state_path);

    // Reconstruct UnixListener from preserved fd
    let std_listener =
        unsafe { std::os::unix::net::UnixListener::from_raw_fd(reload_state.listener_fd) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;
    set_cloexec(reload_state.listener_fd)?;

    // Reconstruct session tree
    let (pty_tx, pty_rx) = mpsc::unbounded_channel::<(NodeId, Vec<u8>)>();
    let session = restore_session(reload_state.session, pty_tx.clone())?;

    let state = Arc::new(Mutex::new(ServerState {
        session,
        clients: HashMap::new(),
        next_client_id: reload_state.next_client_id,
        client_sizes: HashMap::new(),
        preset_name: reload_state.preset_name,
    }));

    let (reload_tx, reload_rx) = mpsc::unbounded_channel::<()>();
    RELOAD_TX.lock().unwrap().replace(reload_tx);

    spawn_pty_forwarder(Arc::clone(&state), pty_rx);
    spawn_ai_poller(Arc::clone(&state));

    info!("zmux server restored successfully");

    run_accept_loop(listener, state, pty_tx, reload_rx).await
}

// ── Reload: serialize + exec() ────────────────────────────────────────

fn serialize_state(
    st: &mut ServerState,
    listener: &UnixListener,
) -> Result<ReloadState> {
    let listener_fd = listener.as_raw_fd();

    let mut nodes = Vec::new();
    let window_ids: Vec<NodeId> = st
        .session
        .nodes
        .iter()
        .filter_map(|(&id, node)| matches!(node, Node::Window(_)).then_some(id))
        .collect();

    // Serialize non-window nodes
    for (&id, node) in &st.session.nodes {
        let serialized = match node {
            Node::Project(p) => SerializedNode::Project {
                name: p.name.clone(),
                working_dir: p.working_dir.to_string_lossy().to_string(),
                children: p.children.clone(),
                env_profile: p.env_profile.clone(),
                on_project_stop: p.on_project_stop.clone(),
            },
            Node::Group(g) => SerializedNode::Group {
                name: g.name.clone(),
                parent: g.parent,
                children: g.children.clone(),
                working_dir: g.working_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                worktree_path: g
                    .worktree_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                layout_mode: g.layout_mode,
                split_tree: g.split_tree.clone(),
                next_pane_id: g.next_pane_id,
                active_pane: g.active_pane,
                env_profile: g.env_profile.clone(),
                layout_preset: g.layout_preset.clone(),
            },
            Node::Window(_) => continue,
        };
        nodes.push((id, serialized));
    }

    // Serialize window nodes, taking ownership of the fds
    for wid in window_ids {
        if let Some(Node::Window(w)) = st.session.nodes.get_mut(&wid) {
            let parser = w.pty.parser.lock().unwrap();
            let screen_dump = parser.screen().contents_formatted();
            let rows = parser.screen().size().0;
            let cols = parser.screen().size().1;
            drop(parser);

            let master_fd = w.pty.take_master_fd();

            nodes.push((
                wid,
                SerializedNode::Window {
                    name: w.name.clone(),
                    parent: w.parent,
                    master_fd,
                    child_pid: w.pty.child_pid,
                    rows,
                    cols,
                    screen_dump,
                    env_profile: w.env_profile.clone(),
                    on_pane_close: w.on_pane_close.clone(),
                },
            ));
        }
    }

    Ok(ReloadState {
        listener_fd,
        session: SerializedSession {
            nodes,
            root_children: st.session.root_children.clone(),
            next_id: st.session.next_id(),
            active_project: st.session.active_project,
            active_group: st.session.active_group,
            active_window: st.session.active_window,
            shell: st.session.shell.clone(),
        },
        next_client_id: st.next_client_id,
        preset_name: st.preset_name.clone(),
    })
}

fn clear_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        anyhow::bail!("fcntl F_GETFD failed: {}", std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if result == -1 {
        anyhow::bail!("fcntl F_SETFD failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        anyhow::bail!("fcntl F_GETFD failed: {}", std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        anyhow::bail!("fcntl F_SETFD failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

async fn perform_reload(state: Arc<Mutex<ServerState>>, listener: &UnixListener) -> Result<()> {
    let mut st = state.lock().await;

    // Broadcast Reloading to all clients
    st.broadcast(ServerMsg::Reloading);
    st.clients.clear();

    // Serialize state
    let reload_state = serialize_state(&mut st, listener)?;

    // Write to temp file
    let state_path = protocol::socket_path()
        .parent()
        .unwrap()
        .join("reload_state.json");
    let json = serde_json::to_vec(&reload_state)?;
    std::fs::write(&state_path, &json)?;

    // Clear FD_CLOEXEC on all preserved fds
    clear_cloexec(reload_state.listener_fd)?;
    for (_, node) in &reload_state.session.nodes {
        if let SerializedNode::Window { master_fd, .. } = node {
            clear_cloexec(*master_fd)?;
        }
    }

    // exec() the new binary
    // current_exe() reads /proc/self/exe which may return "…/zmux (deleted)"
    // after cargo rebuilds the binary in-place. Strip the suffix so we exec
    // the newly-written file at the original path.
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();
    let exe = if exe_str.ends_with(" (deleted)") {
        std::path::PathBuf::from(exe_str.trim_end_matches(" (deleted)"))
    } else {
        exe
    };
    info!("exec()ing new binary: {}", exe.display());

    let state_path_str = state_path.to_string_lossy().to_string();
    let c_exe = std::ffi::CString::new(exe.to_string_lossy().as_bytes())?;
    let c_arg_server = std::ffi::CString::new("server")?;
    let c_arg_reload = std::ffi::CString::new("--reload")?;
    let c_arg_path = std::ffi::CString::new(state_path_str.as_bytes())?;

    let c_args: Vec<*const libc::c_char> = vec![
        c_exe.as_ptr(),
        c_arg_server.as_ptr(),
        c_arg_reload.as_ptr(),
        c_arg_path.as_ptr(),
        std::ptr::null(),
    ];

    unsafe {
        libc::execv(c_exe.as_ptr(), c_args.as_ptr());
    }

    // If we reach here, exec failed
    let err = std::io::Error::last_os_error();
    error!("exec() failed: {}", err);
    let _ = std::fs::remove_file(&state_path);
    anyhow::bail!("exec() failed: {}", err)
}

// ── Restore session tree from serialized state ────────────────────────

fn restore_session(
    serialized: SerializedSession,
    pty_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
) -> Result<SessionTree> {
    let mut tree = SessionTree::new();
    tree.set_next_id(serialized.next_id);
    tree.root_children = serialized.root_children;
    tree.active_project = serialized.active_project;
    tree.active_group = serialized.active_group;
    tree.active_window = serialized.active_window;
    tree.shell = serialized.shell;

    for (id, node) in serialized.nodes {
        let restored = match node {
            SerializedNode::Project {
                name,
                working_dir,
                children,
                env_profile,
                on_project_stop,
            } => Node::Project(ProjectNode {
                name,
                working_dir: PathBuf::from(working_dir),
                children,
                env_profile,
                on_project_stop,
            }),
            SerializedNode::Group {
                name,
                parent,
                children,
                working_dir,
                worktree_path,
                layout_mode,
                split_tree,
                next_pane_id,
                active_pane,
                env_profile,
                layout_preset,
            } => Node::Group(GroupNode {
                name,
                parent,
                children,
                working_dir: working_dir.map(PathBuf::from),
                worktree_path: worktree_path.map(PathBuf::from),
                layout_mode,
                split_tree,
                next_pane_id,
                active_pane,
                env_profile,
                layout_preset,
            }),
            SerializedNode::Window {
                name,
                parent,
                master_fd,
                child_pid,
                rows,
                cols,
                screen_dump,
                env_profile,
                on_pane_close,
            } => {
                set_cloexec(master_fd)?;
                let pty = PtyHandle::from_raw_parts(
                    master_fd,
                    child_pid,
                    rows,
                    cols,
                    &screen_dump,
                    pty_tx.clone(),
                    id,
                )?;
                Node::Window(WindowNode {
                    name,
                    parent,
                    pty,
                    ai_status: None,
                    last_cpu_time: 0,
                    env_profile,
                    on_pane_close,
                })
            }
        };
        tree.nodes.insert(id, restored);
    }

    Ok(tree)
}

// ── Client handler ────────────────────────────────────────────────────

async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
    pty_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
) -> Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ServerMsg>();

    // Register as active client and send initial state
    let client_id = {
        let mut st = state.lock().await;
        let client_id = st.next_client_id;
        st.next_client_id += 1;
        st.clients.insert(client_id, client_tx.clone());

        let tab_state = st.session.tab_state();
        let _ = client_tx.send(tab_state);

        if let Some(wid) = st.session.active_window {
            if let Some(data) = st.session.screen_dump(wid) {
                let _ = client_tx.send(ServerMsg::ScreenDump {
                    window_id: wid,
                    data,
                });
            }
        }
        // Also send screen dumps for tiled windows
        for wid in st.session.active_tiled_windows() {
            if Some(wid) != st.session.active_window {
                if let Some(data) = st.session.screen_dump(wid) {
                    let _ = client_tx.send(ServerMsg::ScreenDump {
                        window_id: wid,
                        data,
                    });
                }
            }
        }
        client_id
    };

    // Writer task
    let writer_clone = Arc::clone(&writer);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            let mut w = writer_clone.lock().await;
            if protocol::write_msg(&mut *w, &msg).await.is_err() {
                break;
            }
        }
    });

    // Reader loop
    loop {
        let msg: Option<ClientMsg> = protocol::read_msg(&mut reader).await?;
        let msg = match msg {
            Some(m) => m,
            None => break,
        };

        let mut st = state.lock().await;
        match msg {
            ClientMsg::Input { data } => {
                if let Some(w) = st.session.active_window_mut() {
                    if let Err(e) = w.pty.write(&data) {
                        warn!("PTY write error: {}", e);
                    }
                }
            }
            ClientMsg::Resize { cols, rows } => {
                st.client_sizes.insert(client_id, (cols, rows));
                let (eff_cols, eff_rows) = st.effective_size();
                let term_rows = eff_rows.saturating_sub(1);
                let eff_cols = eff_cols;
                let _ = st.session.resize_all(term_rows, eff_cols);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::SelectProject { id } => {
                st.session.select_project(id);
                let tab = st.session.tab_state();
                st.broadcast(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                for wid in st.session.active_tiled_windows() {
                    if Some(wid) != st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::SelectGroup { id } => {
                st.session.select_group(id);
                let tab = st.session.tab_state();
                st.broadcast(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                for wid in st.session.active_tiled_windows() {
                    if Some(wid) != st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::SelectWindow { id } => {
                st.session.select_window(id);
                let tab = st.session.tab_state();
                st.broadcast(tab);
                if let Some(data) = st.session.screen_dump(id) {
                    st.broadcast(ServerMsg::ScreenDump {
                        window_id: id,
                        data,
                    });
                }
                for wid in st.session.active_tiled_windows() {
                    if Some(wid) != st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::NewWindow { name } => {
                if let Some(group_id) = st.session.active_group {
                    let (cols, rows) = st.effective_size();
                    let term_rows = rows.saturating_sub(1);
                    let cols = cols;
                    let win_name = name.unwrap_or_else(|| {
                        let count =
                            if let Some(Node::Group(g)) = st.session.nodes.get(&group_id) {
                                g.children.len()
                            } else {
                                0
                            };
                        format!("shell-{}", count + 1)
                    });
                    if let Ok(id) = st.session.add_window(
                        group_id,
                        win_name.clone(),
                        term_rows,
                        cols,
                        pty_tx.clone(),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ) {
                        st.session.select_window(id);
                        let tab = st.session.tab_state();
                        st.broadcast(tab);
                    }
                }
            }
            ClientMsg::NewGroup { name } => {
                if let Some(project_id) = st.session.active_project {
                    let grp_name = name.unwrap_or_else(|| {
                        if let Some(Node::Project(p)) = st.session.nodes.get(&project_id) {
                            format!("group-{}", p.children.len() + 1)
                        } else {
                            "group".to_string()
                        }
                    });
                    let group_id = st.session.add_group(project_id, grp_name, None, None);
                    let (cols, rows) = st.effective_size();
                    let term_rows = rows.saturating_sub(1);
                    let cols = cols;
                    if let Ok(wid) = st.session.add_window(
                        group_id,
                        "shell".to_string(),
                        term_rows,
                        cols,
                        pty_tx.clone(),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ) {
                        st.session.select_group(group_id);
                        st.session.select_window(wid);
                        let tab = st.session.tab_state();
                        st.broadcast(tab);
                    }
                }
            }
            ClientMsg::NewProject { name, path } => {
                let cwd = path.unwrap_or_else(||
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                );
                let proj_name = name.unwrap_or_else(|| {
                    cwd.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "default".to_string())
                });
                let project_id = st.session.add_project(proj_name, cwd);
                let group_id =
                    st.session
                        .add_group(project_id, "default".to_string(), None, None);
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(1);
                let cols = cols;
                if let Ok(wid) = st.session.add_window(
                    group_id,
                    "shell".to_string(),
                    term_rows,
                    cols,
                    pty_tx.clone(),
                    None,
                    None,
                    None,
                    None,
                    None,
                ) {
                    st.session.select_project(project_id);
                    st.session.select_window(wid);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                }
            }
            ClientMsg::Subscribe => {
                let tab = st.session.tab_state();
                let _ = client_tx.send(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        let _ = client_tx.send(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                for wid in st.session.active_tiled_windows() {
                    if Some(wid) != st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            let _ = client_tx.send(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::LoadPreset { name } => {
                match config::load_preset(&name) {
                    Ok(preset) => {
                        let (cols, rows) = st.effective_size();
                        let term_rows = rows.saturating_sub(1);
                        let cols = cols;
                        let mut first_project_id = None;
                        for proj_preset in &preset.projects {
                            let proj_dir = PathBuf::from(&proj_preset.path);
                            // Run lifecycle hooks
                            if let Some(ref cmd) = proj_preset.on_project_start {
                                drop(st);
                                run_hook(cmd, &proj_dir).await;
                                st = state.lock().await;
                            }
                            if config::is_first_start(&proj_preset.name) {
                                if let Some(ref cmd) = proj_preset.on_project_first_start {
                                    drop(st);
                                    run_hook(cmd, &proj_dir).await;
                                    st = state.lock().await;
                                }
                            } else if let Some(ref cmd) = proj_preset.on_project_restart {
                                drop(st);
                                run_hook(cmd, &proj_dir).await;
                                st = state.lock().await;
                            }

                            let project_id = st.session.find_project_by_name(&proj_preset.name)
                                .unwrap_or_else(|| {
                                    st.session.add_project(
                                        proj_preset.name.clone(),
                                        proj_dir,
                                    )
                                });
                            if proj_preset.env_profile.is_some() {
                                st.session.set_env_profile(project_id, proj_preset.env_profile.clone());
                            }
                            if first_project_id.is_none() {
                                first_project_id = Some(project_id);
                            }
                            for grp_preset in &proj_preset.groups {
                                let group_id = if let Some(existing) = st.session.find_group_by_name(project_id, &grp_preset.name) {
                                    existing
                                } else {
                                    let group_dir =
                                        grp_preset.path.as_ref().map(|p| PathBuf::from(p));
                                    let wt_path =
                                        grp_preset.worktree_branch.as_ref().and_then(|branch| {
                                            worktree::create(
                                                &PathBuf::from(&proj_preset.path),
                                                branch,
                                            )
                                            .ok()
                                        });
                                    let working_dir = group_dir.or_else(|| wt_path.clone());
                                    st.session.add_group(
                                        project_id,
                                        grp_preset.name.clone(),
                                        working_dir,
                                        wt_path,
                                    )
                                };
                                if grp_preset.env_profile.is_some() {
                                    st.session.set_env_profile(group_id, grp_preset.env_profile.clone());
                                }
                                if grp_preset.windows.is_empty() {
                                    if st.session.find_window_by_name(group_id, "shell").is_none() {
                                        let _ = st.session.add_window(
                                            group_id,
                                            "shell".to_string(),
                                            term_rows,
                                            cols,
                                            pty_tx.clone(),
                                            None,
                                            None,
                                            None,
                                            proj_preset.pre_window.clone(),
                                            None,
                                        );
                                    }
                                } else {
                                    for win_preset in &grp_preset.windows {
                                        if st.session.find_window_by_name(group_id, &win_preset.name).is_none() {
                                            if let Ok(wid) = st.session.add_window(
                                                group_id,
                                                win_preset.name.clone(),
                                                term_rows,
                                                cols,
                                                pty_tx.clone(),
                                                win_preset.command.clone(),
                                                win_preset.path.as_ref().map(PathBuf::from),
                                                win_preset.env_profile.clone(),
                                                proj_preset.pre_window.clone(),
                                                win_preset.on_pane_close.clone(),
                                            ) {
                                                if let Some(ref hook) = win_preset.on_pane_open {
                                                    if let Some(Node::Window(w)) = st.session.nodes.get_mut(&wid) {
                                                        let _ = w.pty.write(format!("{}\n", hook).as_bytes());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Apply layout preset if specified
                                if let Some(ref layout) = grp_preset.layout {
                                    if let Some(Node::Group(g)) = st.session.nodes.get_mut(&group_id) {
                                        let window_ids: Vec<NodeId> = g.children.clone();
                                        if window_ids.len() > 1 {
                                            let panes: Vec<(u32, NodeId)> = window_ids.iter().enumerate()
                                                .map(|(i, &wid)| (i as u32, wid)).collect();
                                            g.layout_mode = LayoutMode::Tiled;
                                            g.split_tree = crate::session::build_layout_tree(&panes, layout);
                                            g.next_pane_id = panes.len() as u32;
                                            g.active_pane = Some(0);
                                            g.layout_preset = Some(layout.clone());
                                        } else if window_ids.len() == 1 {
                                            g.layout_mode = LayoutMode::Tiled;
                                            g.split_tree = Some(protocol::SplitTree::Leaf { pane_id: 0, window_id: window_ids[0] });
                                            g.next_pane_id = 1;
                                            g.active_pane = Some(0);
                                            g.layout_preset = Some(layout.clone());
                                        }
                                    }
                                }
                            }
                            // Apply startup focus
                            if let Some(ref startup_group) = proj_preset.startup_group {
                                if let Some(gid) = st.session.find_group_by_name(project_id, startup_group) {
                                    st.session.select_group(gid);
                                    if let Some(ref startup_window) = proj_preset.startup_window {
                                        if let Some(wid) = st.session.find_window_by_name(gid, startup_window) {
                                            st.session.select_window(wid);
                                        }
                                    }
                                }
                            } else if let Some(ref startup_window) = proj_preset.startup_window {
                                if let Some(Node::Project(p)) = st.session.nodes.get(&project_id) {
                                    let groups = p.children.clone();
                                    for gid in groups {
                                        if let Some(wid) = st.session.find_window_by_name(gid, startup_window) {
                                            st.session.select_group(gid);
                                            st.session.select_window(wid);
                                            break;
                                        }
                                    }
                                }
                            }
                            // Store on_project_stop hook
                            if proj_preset.on_project_stop.is_some() {
                                if let Some(Node::Project(p)) = st.session.nodes.get_mut(&project_id) {
                                    p.on_project_stop = proj_preset.on_project_stop.clone();
                                }
                            }
                        }
                        // Jump to the first project's first window
                        if let Some(pid) = first_project_id {
                            st.session.select_project(pid);
                        }
                        let tab = st.session.tab_state();
                        st.broadcast(tab);
                    }
                    Err(e) => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Failed to load preset: {}", e),
                        });
                    }
                }
            }
            ClientMsg::SetProjectDir => {
                let msg = st
                    .session
                    .set_project_dir()
                    .unwrap_or_else(|| "No active window".to_string());
                let _ = client_tx.send(ServerMsg::Info { message: msg });
            }
            ClientMsg::SetGroupDir => {
                let msg = st
                    .session
                    .set_group_dir()
                    .unwrap_or_else(|| "No active window".to_string());
                let _ = client_tx.send(ServerMsg::Info { message: msg });
            }
            ClientMsg::SavePreset { name, force, include } => {
                let preset_name = name
                    .or_else(|| st.preset_name.clone())
                    .unwrap_or_else(|| {
                        st.session
                            .root_children
                            .first()
                            .and_then(|id| match st.session.nodes.get(id) {
                                Some(Node::Project(p)) => Some(p.name.clone()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "default".to_string())
                    });
                if !force && config::preset_exists(&preset_name) {
                    let _ = client_tx.send(ServerMsg::ConfirmOverwrite {
                        name: preset_name,
                    });
                } else {
                    let preset = st.session.to_preset_filtered(include.as_ref());
                    match config::save_preset(&preset_name, &preset) {
                        Ok(_) => {
                            st.preset_name = Some(preset_name.clone());
                            let _ = client_tx.send(ServerMsg::Info {
                                message: format!("Saved preset: {}", preset_name),
                            });
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Failed to save: {}", e),
                            });
                        }
                    }
                }
            }
            ClientMsg::NextAiWindow => {
                if st.session.cycle_ai_window(true) {
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info {
                        message: "No AI sessions".to_string(),
                    });
                }
            }
            ClientMsg::PrevAiWindow => {
                if st.session.cycle_ai_window(false) {
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info {
                        message: "No AI sessions".to_string(),
                    });
                }
            }
            ClientMsg::MoveWindowToNewProject => {
                if let Some(wid) = st.session.active_window {
                    let cwd = st.session.window_cwd(wid).unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                    });
                    let proj_name = cwd
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "project".to_string());
                    let project_id = st.session.add_project(proj_name, cwd);
                    let group_id =
                        st.session
                            .add_group(project_id, "default".to_string(), None, None);
                    st.session.move_window_to_group(wid, group_id);
                    st.session.select_project(project_id);
                    st.session.active_group = Some(group_id);
                    st.session.active_window = Some(wid);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                }
            }
            ClientMsg::MoveWindowToNewGroup => {
                if let (Some(wid), Some(project_id)) =
                    (st.session.active_window, st.session.active_project)
                {
                    let cwd = st.session.window_cwd(wid).unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                    });
                    let grp_name = cwd
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "group".to_string());
                    let group_id =
                        st.session
                            .add_group(project_id, grp_name, Some(cwd), None);
                    st.session.move_window_to_group(wid, group_id);
                    st.session.select_group(group_id);
                    st.session.active_window = Some(wid);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                }
            }
            ClientMsg::Rename { id, name } => {
                match st.session.nodes.get_mut(&id) {
                    Some(Node::Project(p)) => p.name = name,
                    Some(Node::Group(g)) => g.name = name,
                    Some(Node::Window(w)) => w.name = name,
                    None => {}
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
            }
            ClientMsg::CloseWindow => {
                if let Some(wid) = st.session.active_window {
                    // Extract on_pane_close hook before removing
                    let hook_info = if let Some(Node::Window(w)) = st.session.nodes.get(&wid) {
                        w.on_pane_close.as_ref().map(|cmd| {
                            let dir = st.session.window_working_dir(w.parent);
                            (cmd.clone(), dir)
                        })
                    } else {
                        None
                    };
                    st.session.remove_window(wid);
                    if let Some((cmd, dir)) = hook_info {
                        tokio::spawn(async move { run_hook(&cmd, &dir).await });
                    }
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    if let Some(new_wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(new_wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: new_wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::CloseNode { id } => {
                match st.session.nodes.get(&id) {
                    Some(Node::Window(w)) => {
                        let hook_info = w.on_pane_close.as_ref().map(|cmd| {
                            let dir = st.session.window_working_dir(w.parent);
                            (cmd.clone(), dir)
                        });
                        st.session.remove_window(id);
                        if let Some((cmd, dir)) = hook_info {
                            tokio::spawn(async move { run_hook(&cmd, &dir).await });
                        }
                    }
                    Some(Node::Group(_)) => {
                        if let Some((project_dir, wt_path)) = st.session.remove_group(id) {
                            if let Err(e) = worktree::remove(&project_dir, &wt_path, false) {
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Worktree cleanup failed: {}", e),
                                });
                            }
                        }
                    }
                    Some(Node::Project(p)) => {
                        // Run on_project_stop hook before removing
                        let hook_info = p.on_project_stop.as_ref().map(|cmd| {
                            (cmd.clone(), p.working_dir.clone())
                        });
                        let wt_infos = st.session.remove_project(id);
                        for (project_dir, wt_path) in wt_infos {
                            if let Err(e) = worktree::remove(&project_dir, &wt_path, false) {
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Worktree cleanup failed: {}", e),
                                });
                            }
                        }
                        if let Some((cmd, dir)) = hook_info {
                            tokio::spawn(async move { run_hook(&cmd, &dir).await });
                        }
                    }
                    None => {}
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::RebaseMain => {
                if let Some(gid) = st.session.active_group {
                    let working_dir = st.session.window_working_dir(gid);
                    let output = std::process::Command::new("git")
                        .args(["rebase", "main"])
                        .current_dir(&working_dir)
                        .output();
                    match output {
                        Ok(o) if o.status.success() => {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            let msg = stdout.trim();
                            let _ = client_tx.send(ServerMsg::Info {
                                message: if msg.is_empty() {
                                    "Rebase complete".to_string()
                                } else {
                                    msg.to_string()
                                },
                            });
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Rebase failed: {}", stderr.trim()),
                            });
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Failed to run git: {}", e),
                            });
                        }
                    }
                }
            }
            ClientMsg::MergeIntoMain => {
                let (project_dir, branch) = if let Some(gid) = st.session.active_group {
                    match st.session.nodes.get(&gid) {
                        Some(Node::Group(g)) => {
                            let branch = g.worktree_path.as_ref().and_then(|p| {
                                p.file_name().map(|n| n.to_string_lossy().to_string())
                            });
                            let proj_dir =
                                if let Some(Node::Project(p)) = st.session.nodes.get(&g.parent) {
                                    Some(p.working_dir.clone())
                                } else {
                                    None
                                };
                            (proj_dir, branch)
                        }
                        _ => (None, None),
                    }
                } else {
                    (None, None)
                };

                match (project_dir, branch) {
                    (Some(dir), Some(branch)) => {
                        let output = std::process::Command::new("git")
                            .args(["merge", &branch])
                            .current_dir(&dir)
                            .output();
                        match output {
                            Ok(o) if o.status.success() => {
                                let _ = client_tx.send(ServerMsg::Info {
                                    message: format!("Merged {} into main", branch),
                                });
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Merge failed: {}", stderr.trim()),
                                });
                            }
                            Err(e) => {
                                let _ = client_tx.send(ServerMsg::Error {
                                    message: format!("Failed to run git: {}", e),
                                });
                            }
                        }
                    }
                    _ => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: "Not a worktree group".to_string(),
                        });
                    }
                }
            }
            ClientMsg::NewWorktreeGroup { branch } => {
                if let Some(project_id) = st.session.active_project {
                    let project_dir = match st.session.nodes.get(&project_id) {
                        Some(Node::Project(p)) => p.working_dir.clone(),
                        _ => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: "No active project".to_string(),
                            });
                            continue;
                        }
                    };
                    if !worktree::is_git_repo(&project_dir) {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Not a git repo: {}", project_dir.display()),
                        });
                        continue;
                    }
                    match worktree::create(&project_dir, &branch) {
                        Ok(wt_path) => {
                            let group_id = st.session.add_group(
                                project_id,
                                branch.clone(),
                                Some(wt_path.clone()),
                                Some(wt_path),
                            );
                            let (cols, rows) = st.effective_size();
                            let term_rows = rows.saturating_sub(1);
                            let cols = cols;
                            if let Ok(wid) = st.session.add_window(
                                group_id,
                                "shell".to_string(),
                                term_rows,
                                cols,
                                pty_tx.clone(),
                                None,
                                None,
                                None,
                                None,
                                None,
                            ) {
                                st.session.select_group(group_id);
                                st.session.select_window(wid);
                                let tab = st.session.tab_state();
                                st.broadcast(tab);
                                let _ = client_tx.send(ServerMsg::Info {
                                    message: format!("Worktree: {}", branch),
                                });
                            }
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Worktree failed: {}", e),
                            });
                        }
                    }
                }
            }
            ClientMsg::ListBranches => {
                if let Some(project_id) = st.session.active_project {
                    let project_dir = match st.session.nodes.get(&project_id) {
                        Some(Node::Project(p)) => p.working_dir.clone(),
                        _ => continue,
                    };
                    let branches = worktree::list_branches(&project_dir);
                    let _ = client_tx.send(ServerMsg::BranchList { branches });
                }
            }
            ClientMsg::ListPresets => match config::list_presets() {
                Ok(presets) => {
                    let _ = client_tx.send(ServerMsg::PresetList { presets });
                }
                Err(e) => {
                    let _ = client_tx.send(ServerMsg::Error {
                        message: format!("Failed to list presets: {}", e),
                    });
                }
            },
            ClientMsg::CloseGroup { force } => {
                if let Some(group_id) = st.session.active_group {
                    let is_wt = matches!(
                        st.session.nodes.get(&group_id),
                        Some(Node::Group(g)) if g.worktree_path.is_some()
                    );
                    if is_wt && !force {
                        let dirty = match st.session.nodes.get(&group_id) {
                            Some(Node::Group(g)) => g
                                .worktree_path
                                .as_ref()
                                .map(|p| worktree::is_dirty(p))
                                .unwrap_or(false),
                            _ => false,
                        };
                        if dirty {
                            let _ = client_tx.send(ServerMsg::Error {
                                message:
                                    "Worktree has uncommitted changes. Use force to remove."
                                        .to_string(),
                            });
                            continue;
                        }
                    }
                    if let Some((project_dir, wt_path)) = st.session.remove_group(group_id) {
                        if let Err(e) = worktree::remove(&project_dir, &wt_path, force) {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Worktree cleanup failed: {}", e),
                            });
                        }
                    }
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    if let Some(wid) = st.session.active_window {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                }
            }
            ClientMsg::ToggleLayout => {
                st.session.toggle_layout();
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(1);
                let cols = cols;
                if let Err(e) = st.session.resize_all(term_rows, cols) {
                    warn!("resize_all failed: {}", e);
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::SplitPane { direction } => {
                st.session.split_pane(direction);
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(1);
                let cols = cols;
                if let Err(e) = st.session.resize_all(term_rows, cols) {
                    warn!("resize_all failed: {}", e);
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::CloseSplit => {
                st.session.close_split();
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(3);
                let cols = cols.saturating_sub(2);
                let _ = st.session.resize_all(term_rows, cols);
                let tab = st.session.tab_state();
                st.broadcast(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
                if let Some(wid) = st.session.active_window {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::SwapSplitDirection => {
                st.session.swap_split_direction();
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(1);
                let cols = cols;
                if let Err(e) = st.session.resize_all(term_rows, cols) {
                    warn!("resize_all failed: {}", e);
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::CyclePaneContent { forward } => {
                if st.session.cycle_pane_content(forward) {
                    let (cols, rows) = st.effective_size();
                    let term_rows = rows.saturating_sub(1);
                    let cols = cols;
                    let _ = st.session.resize_all(term_rows, cols);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    for wid in st.session.active_tiled_windows() {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info {
                        message: "No other windows to swap".to_string(),
                    });
                }
            }
            ClientMsg::SwapActivePaneWith { window_id } => {
                if st.session.swap_active_pane_with(window_id) {
                    let (cols, rows) = st.effective_size();
                    let term_rows = rows.saturating_sub(3);
                    let cols = cols.saturating_sub(2);
                    let _ = st.session.resize_all(term_rows, cols);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    for wid in st.session.active_tiled_windows() {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info {
                        message: "Cannot swap: not in tiled mode or window already visible".to_string(),
                    });
                }
            }
            ClientMsg::CyclePaneContentGlobal { forward } => {
                if st.session.cycle_pane_content_global(forward) {
                    let (cols, rows) = st.effective_size();
                    let term_rows = rows.saturating_sub(3);
                    let cols = cols.saturating_sub(2);
                    let _ = st.session.resize_all(term_rows, cols);
                    let tab = st.session.tab_state();
                    st.broadcast(tab);
                    for wid in st.session.active_tiled_windows() {
                        if let Some(data) = st.session.screen_dump(wid) {
                            st.broadcast(ServerMsg::ScreenDump {
                                window_id: wid,
                                data,
                            });
                        }
                    }
                } else {
                    let _ = client_tx.send(ServerMsg::Info {
                        message: "No other windows to swap".to_string(),
                    });
                }
            }
            ClientMsg::FocusPane { direction } => {
                st.session.focus_pane(direction);
                let tab = st.session.tab_state();
                st.broadcast(tab);
            }
            ClientMsg::ResizePane { direction } => {
                st.session.resize_pane(direction);
                let (cols, rows) = st.effective_size();
                let term_rows = rows.saturating_sub(1);
                let cols = cols;
                if let Err(e) = st.session.resize_all(term_rows, cols) {
                    warn!("resize_all failed: {}", e);
                }
                let tab = st.session.tab_state();
                st.broadcast(tab);
                for wid in st.session.active_tiled_windows() {
                    if let Some(data) = st.session.screen_dump(wid) {
                        st.broadcast(ServerMsg::ScreenDump {
                            window_id: wid,
                            data,
                        });
                    }
                }
            }
            ClientMsg::InputToWindow { window_id, data } => {
                if let Some(Node::Window(w)) = st.session.nodes.get_mut(&window_id) {
                    if let Err(e) = w.pty.write(&data) {
                        warn!("PTY write error: {}", e);
                    }
                }
            }
            ClientMsg::RequestTree => {
                let projects = st.session.full_tree();
                let _ = client_tx.send(ServerMsg::FullTree {
                    projects,
                    active_project: st.session.active_project,
                    active_group: st.session.active_group,
                    active_window: st.session.active_window,
                    preset_name: st.preset_name.clone(),
                });
            }
            ClientMsg::Detach => break,
            ClientMsg::Shutdown => {
                info!("Shutdown requested");
                let _ = std::fs::remove_file(protocol::socket_path());
                // Run on_project_stop hooks before exiting
                let project_hooks: Vec<(String, PathBuf)> = st.session.root_children.iter()
                    .filter_map(|&id| match st.session.nodes.get(&id) {
                        Some(Node::Project(p)) => p.on_project_stop.as_ref().map(|cmd| (cmd.clone(), p.working_dir.clone())),
                        _ => None,
                    })
                    .collect();
                drop(st);
                for (cmd, dir) in project_hooks {
                    run_hook(&cmd, &dir).await;
                }
                std::process::exit(0);
            }
            ClientMsg::Reload => {
                info!("Client {} requested reload", client_id);
                drop(st);
                trigger_reload();
                break;
            }
            ClientMsg::ListEnvProfiles => {
                match config::list_env_profiles() {
                    Ok(profiles) => {
                        let _ = client_tx.send(ServerMsg::EnvProfileList { profiles });
                    }
                    Err(e) => {
                        let _ = client_tx.send(ServerMsg::Error {
                            message: format!("Failed to list env profiles: {}", e),
                        });
                    }
                }
            }
            ClientMsg::SetEnvProfile { scope, profile } => {
                st.session.set_env_profile(scope, profile.clone());
                let profile_desc = profile.as_deref().unwrap_or("none");
                let _ = client_tx.send(ServerMsg::Info {
                    message: format!("Env profile set to: {}", profile_desc),
                });
                let tab = st.session.tab_state();
                st.broadcast(tab);
            }
            ClientMsg::SourceEnvProfile { profile } => {
                if let Some(wid) = st.session.active_window {
                    match st.session.source_env_profile(wid, &profile) {
                        Ok(_) => {
                            let _ = client_tx.send(ServerMsg::Info {
                                message: format!("Sourced env profile: {}", profile),
                            });
                        }
                        Err(e) => {
                            let _ = client_tx.send(ServerMsg::Error {
                                message: format!("Failed to source env: {}", e),
                            });
                        }
                    }
                }
            }
        }
    }

    {
        let mut st = state.lock().await;
        st.clients.remove(&client_id);
        st.client_sizes.remove(&client_id);
        if !st.clients.is_empty() {
            let (cols, rows) = st.effective_size();
            let term_rows = rows.saturating_sub(1);
            let cols = cols;
            let _ = st.session.resize_all(term_rows, cols);
        }
    }
    writer_task.abort();
    info!("Client {} detached", client_id);
    Ok(())
}
