use anyhow::{Context, Result};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};

/// Configuration for creating a new terminal session.
pub struct TerminalSessionConfig {
    pub working_directory: PathBuf,
    pub environment: HashMap<String, String>,
    pub shell: Option<String>,
    /// Custom arguments for the shell program. When set, overrides the default
    /// login shell flags (`-l`). Used to wrap shells with sandbox-exec, etc.
    pub shell_args: Option<Vec<String>>,
    pub initial_command: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

/// Status of a terminal session.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum TerminalStatus {
    Running,
    Exited,
}

/// Public DTO describing a terminal session.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalSessionInfo {
    pub id: String,
    pub label: String,
    pub project_path: Option<String>,
    pub workspace_name: Option<String>,
    pub working_directory: String,
    pub status: TerminalStatus,
}

/// Metadata attached to a session for labeling.
pub struct SessionMetadata {
    pub label: String,
    pub project_path: Option<String>,
    pub workspace_name: Option<String>,
}

/// A live terminal session backed by a PTY.
struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    _killer: Box<dyn ChildKiller + Send + Sync>,
    status: TerminalStatus,
    exit_code: Option<u32>,
    metadata: SessionMetadata,
    working_directory: String,
    /// Signal to stop the reader task.
    cancel_tx: Option<mpsc::Sender<()>>,
}

const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

type SessionMap = Arc<RwLock<HashMap<String, Arc<Mutex<TerminalSession>>>>>;

/// Manages all terminal sessions.
#[derive(Clone)]
pub struct TerminalManager {
    sessions: SessionMap,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawn a new terminal session. Returns the session info and a receiver
    /// that streams PTY output chunks.
    pub async fn create_session(
        &self,
        config: TerminalSessionConfig,
        metadata: SessionMetadata,
    ) -> Result<(TerminalSessionInfo, mpsc::Receiver<Vec<u8>>)> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size).context("Failed to open PTY")?;

        let shell = config.shell.unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell);

        if let Some(args) = config.shell_args {
            for arg in &args {
                cmd.arg(arg);
            }
        } else if shell.ends_with("zsh") || shell.ends_with("bash") {
            // Login shell flag
            cmd.arg("-l");
        }

        cmd.cwd(&config.working_directory);

        for (key, val) in &config.environment {
            cmd.env(key, val);
        }

        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell")?;

        // Drop the slave side — we only interact via the master
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        let id = uuid::Uuid::new_v4().to_string();
        let working_dir = config.working_directory.display().to_string();

        let info = TerminalSessionInfo {
            id: id.clone(),
            label: metadata.label.clone(),
            project_path: metadata.project_path.clone(),
            workspace_name: metadata.workspace_name.clone(),
            working_directory: working_dir.clone(),
            status: TerminalStatus::Running,
        };

        // Channel for PTY output -> frontend
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);

        // Channel to cancel the reader task
        let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);

        // Clone the reader from the master before moving master into session
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;

        // Spawn a blocking reader task
        let session_id = id.clone();
        let sessions_ref = Arc::clone(&self.sessions);
        let killer = child.clone_killer();
        let child = Arc::new(Mutex::new(child));
        let child_for_exit = Arc::clone(&child);

        tokio::spawn(async move {
            loop {
                let exit_result = {
                    let mut child = child_for_exit.lock().await;
                    match child.try_wait() {
                        Ok(Some(status)) => Some(Some(status.exit_code())),
                        Ok(None) => None,
                        Err(_) => Some(None),
                    }
                };

                if let Some(code) = exit_result {
                    let map = sessions_ref.read().await;
                    if let Some(session) = map.get(&session_id) {
                        let mut s = session.lock().await;
                        s.status = TerminalStatus::Exited;
                        s.exit_code = code;
                    }
                    break;
                }

                tokio::time::sleep(EXIT_POLL_INTERVAL).await;
            }
        });

        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                if cancel_rx.try_recv().is_ok() {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let session = Arc::new(Mutex::new(TerminalSession {
            master: pair.master,
            writer,
            _killer: killer,
            status: TerminalStatus::Running,
            exit_code: None,
            metadata,
            working_directory: working_dir,
            cancel_tx: Some(cancel_tx),
        }));

        // If there's an initial command, write it after a brief delay
        if let Some(initial_cmd) = config.initial_command {
            let sess = Arc::clone(&session);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let mut s = sess.lock().await;
                let cmd_with_newline = format!("{}\n", initial_cmd);
                let _ = s.writer.write_all(cmd_with_newline.as_bytes());
            });
        }

        self.sessions.write().await.insert(id, session);

        Ok((info, output_rx))
    }

    /// Write input data to a session's PTY.
    pub async fn write_input(&self, session_id: &str, data: &[u8]) -> Result<()> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(session_id).context("Session not found")?;
        let mut s = session.lock().await;
        s.writer.write_all(data).context("Failed to write to PTY")?;
        Ok(())
    }

    /// Resize a session's PTY.
    pub async fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(session_id).context("Session not found")?;
        let s = session.lock().await;
        s.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        Ok(())
    }

    /// Close and clean up a session.
    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        let session = self.sessions.write().await.remove(session_id);
        if let Some(session) = session {
            let mut s = session.lock().await;
            if let Some(tx) = s.cancel_tx.take() {
                let _ = tx.send(()).await;
            }
            // Dropping master + writer closes the PTY and kills the child
        }
        Ok(())
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Vec<TerminalSessionInfo> {
        let sessions = self.sessions.read().await;
        let mut infos = Vec::with_capacity(sessions.len());
        for (id, session) in sessions.iter() {
            let s = session.lock().await;
            infos.push(TerminalSessionInfo {
                id: id.clone(),
                label: s.metadata.label.clone(),
                project_path: s.metadata.project_path.clone(),
                workspace_name: s.metadata.workspace_name.clone(),
                working_directory: s.working_directory.clone(),
                status: s.status.clone(),
            });
        }
        infos
    }

    /// Get info for a single session.
    pub async fn get_session(&self, session_id: &str) -> Option<TerminalSessionInfo> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(session_id)?;
        let s = session.lock().await;
        Some(TerminalSessionInfo {
            id: session_id.to_string(),
            label: s.metadata.label.clone(),
            project_path: s.metadata.project_path.clone(),
            workspace_name: s.metadata.workspace_name.clone(),
            working_directory: s.working_directory.clone(),
            status: s.status.clone(),
        })
    }

    /// Close all sessions (used on app exit).
    pub async fn close_all(&self) {
        let ids: Vec<String> = self.sessions.read().await.keys().cloned().collect();
        for id in ids {
            let _ = self.close_session(&id).await;
        }
    }

}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect the user's default shell.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
}
