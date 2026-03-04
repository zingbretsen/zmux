use anyhow::Result;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::protocol::NodeId;

pub struct PtyHandle {
    /// Raw PTY master file descriptor. We own this and close it on drop.
    pub master_fd: RawFd,
    writer: std::fs::File,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub child_pid: Option<u32>,
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Send SIGHUP to the child process (standard signal for terminal hangup)
        if let Some(pid) = self.child_pid {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGHUP);
            }
        }
        if self.master_fd >= 0 {
            unsafe {
                libc::close(self.master_fd);
            }
        }
    }
}

fn set_pty_size(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if ret != 0 {
        anyhow::bail!(
            "ioctl TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn spawn_reader_thread(
    master_fd: RawFd,
    parser: Arc<Mutex<vt100::Parser>>,
    notify_tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    let reader_fd = unsafe { libc::dup(master_fd) };
    assert!(reader_fd >= 0, "dup for reader failed");
    let mut reader = unsafe { std::fs::File::from_raw_fd(reader_fd) };

    // Writer fd for responding to terminal queries (e.g. cursor position)
    let responder_fd = unsafe { libc::dup(master_fd) };
    assert!(responder_fd >= 0, "dup for query responder failed");
    let mut responder = unsafe { std::fs::File::from_raw_fd(responder_fd) };

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let bytes = buf[..n].to_vec();
                    let mut p = parser.lock().unwrap();
                    p.process(&bytes);

                    // Respond to Device Status Report (CSI 6 n) — cursor position query.
                    // Programs like fzf send this to determine where to draw their UI.
                    // Without a response, they block until timeout.
                    if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                        let (row, col) = p.screen().cursor_position();
                        drop(p);
                        let response = format!("\x1b[{};{}R", row + 1, col + 1);
                        let _ = responder.write_all(response.as_bytes());
                    }

                    let _ = notify_tx.send(bytes);
                }
            }
        }
    });
}

impl PtyHandle {
    /// Spawn a shell with optional extra env vars. Returns raw PTY output bytes through the channel.
    pub fn spawn_in(
        rows: u16,
        cols: u16,
        cwd: &Path,
        env: &HashMap<String, String>,
        shell_override: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Vec<u8>>)> {
        // Open PTY pair
        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        let ret = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret != 0 {
            anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
        }

        // Set initial size on slave
        set_pty_size(slave, rows, cols)?;

        let shell = shell_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));

        // Spawn child process with slave PTY as stdin/stdout/stderr
        let child = {
            use std::os::unix::process::CommandExt;

            let stdin_fd = unsafe { libc::dup(slave) };
            let stdout_fd = unsafe { libc::dup(slave) };
            let stderr_fd = unsafe { libc::dup(slave) };

            let mut cmd = std::process::Command::new(&shell);
            // Set argv[0] to "-shellname" so the shell runs as a login shell.
            // This is the POSIX convention used by login(1) and tmux, ensuring
            // that login profiles (~/.zprofile, ~/.bash_profile, etc.) are sourced.
            let shell_basename = Path::new(&shell)
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("sh"))
                .to_string_lossy();
            cmd.arg0(format!("-{}", shell_basename));
            cmd.current_dir(cwd);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.stdin(unsafe { std::process::Stdio::from_raw_fd(stdin_fd) });
            cmd.stdout(unsafe { std::process::Stdio::from_raw_fd(stdout_fd) });
            cmd.stderr(unsafe { std::process::Stdio::from_raw_fd(stderr_fd) });

            unsafe {
                cmd.pre_exec(move || {
                    // Create new session
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Set controlling terminal
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            cmd.spawn()?
        };

        let child_pid = Some(child.id());

        // Close slave in parent
        unsafe {
            libc::close(slave);
        }

        // Create writer (dup of master)
        let writer_fd = unsafe { libc::dup(master) };
        assert!(writer_fd >= 0, "dup for writer failed");
        let writer = unsafe { std::fs::File::from_raw_fd(writer_fd) };

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();

        spawn_reader_thread(master, Arc::clone(&parser), notify_tx);

        Ok((
            PtyHandle {
                master_fd: master,
                writer,
                parser,
                child_pid,
            },
            notify_rx,
        ))
    }

    /// Restore a PtyHandle from a preserved raw fd after exec() reload.
    pub fn from_raw_parts(
        master_fd: RawFd,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
        screen_dump: &[u8],
        pty_output_tx: mpsc::UnboundedSender<(NodeId, Vec<u8>)>,
        window_id: NodeId,
    ) -> Result<Self> {
        // Create writer from dup'd fd
        let writer_fd = unsafe { libc::dup(master_fd) };
        if writer_fd < 0 {
            anyhow::bail!("dup for writer failed: {}", std::io::Error::last_os_error());
        }
        let writer = unsafe { std::fs::File::from_raw_fd(writer_fd) };

        // Create parser and replay screen dump to restore visual state
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        parser.lock().unwrap().process(screen_dump);

        // Spawn reader thread that forwards to the pty_output channel with window_id
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
        spawn_reader_thread(master_fd, Arc::clone(&parser), notify_tx);

        // Bridge: forward from per-pty channel to the shared server channel
        tokio::spawn(async move {
            while let Some(bytes) = notify_rx.recv().await {
                if pty_output_tx.send((window_id, bytes)).is_err() {
                    break;
                }
            }
            // PTY EOF sentinel
            let _ = pty_output_tx.send((window_id, Vec::new()));
        });

        Ok(PtyHandle {
            master_fd,
            writer,
            parser,
            child_pid,
        })
    }

    /// Take ownership of the master fd, preventing close on drop.
    /// Used before exec() to preserve the fd.
    pub fn take_master_fd(&mut self) -> RawFd {
        let fd = self.master_fd;
        self.master_fd = -1;
        fd
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        set_pty_size(self.master_fd, rows, cols)?;
        self.parser.lock().unwrap().set_size(rows, cols);
        Ok(())
    }

    /// Get the current working directory of the shell process.
    pub fn cwd(&self) -> Option<PathBuf> {
        let pid = self.child_pid?;
        // Linux: /proc/{pid}/cwd symlink
        #[cfg(target_os = "linux")]
        {
            return std::fs::read_link(format!("/proc/{}/cwd", pid)).ok();
        }
        // macOS: use lsof -a -p {pid} -d cwd -Fn
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("lsof")
                .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if let Some(path) = line.strip_prefix('n') {
                    return Some(PathBuf::from(path));
                }
            }
            None
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }
}
