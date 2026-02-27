use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub child_pid: Option<u32>,
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
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = shell_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id();
        drop(pair.slave);

        let mut writer = pair.master.take_writer()?;
        // Clear the screen so the new shell doesn't show leftover terminal content
        writer.write_all(b"\x1b[2J\x1b[H")?;
        let mut reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));

        let parser_clone = Arc::clone(&parser);
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        parser_clone.lock().unwrap().process(&bytes);
                        let _ = notify_tx.send(bytes);
                    }
                }
            }
        });

        Ok((
            PtyHandle {
                master: pair.master,
                writer,
                parser,
                child_pid,
            },
            notify_rx,
        ))
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.lock().unwrap().set_size(rows, cols);
        Ok(())
    }

    /// Get the current working directory of the shell process (Linux only).
    pub fn cwd(&self) -> Option<PathBuf> {
        let pid = self.child_pid?;
        std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
    }
}
