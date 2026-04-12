use super::get_default_command;
use super::pty_fallback::fallback_open_and_spawn;
use super::scrollback::Scrollback;
use super::types::*;
use crate::utils::parse_u16;
use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    Json,
};
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use std::io::Write;
use std::time::SystemTime;
use std::{
    io::Read,
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio::sync::Mutex;
use tokio::task::spawn_blocking;

pub struct TerminalSession {
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub child_killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub scrollback: Arc<Scrollback>,
    pub output_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>>,
    pub exit_status: Arc<std::sync::Mutex<Option<bool>>>,
    pub exit_notify: Arc<tokio::sync::Notify>,
    pub last_accessed: Arc<Mutex<SystemTime>>,
}

pub async fn create_terminal(
    State(sessions): State<Sessions>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let rows = parse_u16(&options.rows, "rows").expect("failed");
    let cols = parse_u16(&options.cols, "cols").expect("failed");
    tracing::info!("Creating new terminal with cols={}, rows={}", cols, rows);

    let mut program = String::from("login");
    let mut args: Vec<String> = Vec::new();
    if let Some(cmd) = get_default_command() {
        let parts: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
        if !parts.is_empty() {
            program = parts[0].clone();
            if parts.len() > 1 {
                args = parts[1..].to_vec();
            }
        }
    }

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    // --- Try the standard portable-pty path first ---
    let pty_system = native_pty_system();
    let openpty_result = pty_system.openpty(size);

    let std_result = match openpty_result {
        Ok(pair) => {
            let mut cmd = CommandBuilder::new(&program);
            if !args.is_empty() {
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                cmd.args(arg_refs);
            }
            match pair.slave.spawn_command(cmd) {
                Ok(child) => Ok((pair.master, child)),
                Err(e) => {
                    // openpty succeeded but spawn failed — this is a command
                    // error (e.g. missing program), not a PTY capability issue.
                    // Do NOT fall back; report immediately.
                    tracing::error!("spawn_command failed: {}", e);
                    return Json(ErrorResponse {
                        error: format!("Failed to spawn command: {e}"),
                    })
                    .into_response();
                }
            }
        }
        Err(e) => Err(e),
    };

    // --- If openpty itself failed, fall back to TIOCGPTPEER ---
    let (master, mut child) = match std_result {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                "Standard openpty failed ({}), trying TIOCGPTPEER fallback",
                e
            );
            match fallback_open_and_spawn(size, &program, &args) {
                Ok((master, child)) => (master, child),
                Err(fb_err) => {
                    tracing::error!("TIOCGPTPEER fallback also failed: {}", fb_err);
                    return Json(ErrorResponse {
                        error: format!("Failed to open PTY: {e}; TIOCGPTPEER fallback: {fb_err}"),
                    })
                    .into_response();
                }
            }
        }
    };

    // --- Common session setup ---
    let pid = child.process_id().unwrap_or(0);

    let reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to clone PTY reader: {}", e);
            let _ = child.kill();
            let _ = child.wait();
            return Json(ErrorResponse {
                error: format!("Failed to clone PTY reader: {e}"),
            })
            .into_response();
        }
    };
    let writer = match master.take_writer() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(e) => {
            tracing::error!("Failed to take PTY writer: {}", e);
            let _ = child.kill();
            let _ = child.wait();
            return Json(ErrorResponse {
                error: format!("Failed to take PTY writer: {e}"),
            })
            .into_response();
        }
    };
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(master));
    let child_killer = Arc::new(Mutex::new(child.clone_killer()));

    let scrollback = Arc::new(Scrollback::new(pid));
    let output_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let exit_status: Arc<std::sync::Mutex<Option<bool>>> = Arc::new(std::sync::Mutex::new(None));
    let exit_notify = Arc::new(tokio::sync::Notify::new());

    // Background PTY reader — runs for the session lifetime
    {
        let scrollback = scrollback.clone();
        let output_tx = output_tx.clone();
        spawn_blocking(move || {
            let mut reader = reader;
            let mut read_buffer = [0u8; 8192];
            loop {
                let n = match reader.read(&mut read_buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };

                let data = &read_buffer[..n];
                let _ = scrollback.append(data);

                if let Ok(guard) = output_tx.try_lock() {
                    if let Some(ref tx) = *guard {
                        let _ = tx.try_send(data.to_vec());
                    }
                }
            }
            tracing::info!("Background PTY reader exited for PID {}", pid);
        });
    }

    // Background child waiter — signals when process exits
    {
        let exit_status = exit_status.clone();
        let exit_notify = exit_notify.clone();
        let child = Arc::new(std::sync::Mutex::new(child));
        spawn_blocking(move || {
            let mut child_guard = child.lock().unwrap();
            let success = match child_guard.wait() {
                Ok(status) => status.success(),
                Err(_) => false,
            };
            *exit_status.lock().unwrap() = Some(success);
            exit_notify.notify_waiters();
            tracing::info!(
                "Background child waiter exited for PID {} (success={})",
                pid,
                success
            );
        });
    }

    let session = TerminalSession {
        master,
        child_killer,
        writer,
        scrollback,
        output_tx,
        exit_status,
        exit_notify,
        last_accessed: Arc::new(Mutex::new(SystemTime::now())),
    };

    sessions.insert(pid, session);
    tracing::info!("Terminal created successfully with PID: {}", pid);
    (axum::http::StatusCode::OK, pid.to_string()).into_response()
}

pub async fn resize_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let rows = parse_u16(&options.rows, "rows").expect("Failed");
    let cols = parse_u16(&options.cols, "cols").expect("Failed");
    tracing::info!("Resizing terminal {} to cols={}, rows={}", pid, cols, rows);

    if let Some(session) = sessions.get(&pid) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        match session.master.lock().await.resize(size) {
            Ok(_) => Json(serde_json::json!({"success": true})).into_response(),
            Err(e) => Json(ErrorResponse {
                error: format!("Failed to resize: {e}"),
            })
            .into_response(),
        }
    } else {
        Json(ErrorResponse {
            error: "Session not found".to_string(),
        })
        .into_response()
    }
}

pub async fn terminal_websocket(
    ws: WebSocketUpgrade,
    Path(pid): Path<u32>,
    State(sessions): State<Sessions>,
) -> impl IntoResponse {
    tracing::info!("WebSocket connection request for terminal {}", pid);
    ws.on_upgrade(move |socket| handle_socket(socket, pid, sessions))
}

async fn handle_socket(socket: WebSocket, pid: u32, sessions: Sessions) {
    let (mut sender, mut receiver) = socket.split();

    let (writer, scrollback, output_tx_arc, exit_status_arc, exit_notify) = {
        let Some(session) = sessions.get(&pid) else {
            tracing::error!("Session {} not found", pid);
            return;
        };

        *session.last_accessed.lock().await = SystemTime::now();
        tracing::info!("WebSocket connection established for terminal {}", pid);

        (
            session.writer.clone(),
            session.scrollback.clone(),
            session.output_tx.clone(),
            session.exit_status.clone(),
            session.exit_notify.clone(),
        )
    };

    // Check if process already exited
    let already_exited = {
        let guard = exit_status_arc.lock().unwrap();
        *guard
    };
    if let Some(success) = already_exited {
        let exit_message = ProcessExitMessage {
            exit_code: Some(if success { 0 } else { 1 }),
            signal: None,
            message: if success {
                "Process exited successfully"
            } else {
                "Process exited with non-zero status"
            }
            .to_string(),
        };
        let exit_json = serde_json::to_string(&exit_message).unwrap_or_default();
        let _ = sender
            .send(Message::Text(
                format!("{{\"type\":\"exit\",\"data\":{exit_json}}}").into(),
            ))
            .await;
        sessions.remove(&pid);
        return;
    }

    // Create output channel for this WS connection
    let (ws_output_tx, mut ws_output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    // Send full scrollback history, then atomically enable live forwarding before the
    // PTY reader can append more bytes to the scrollback file. Without that ordering,
    // the first session can replay the initial MOTD from scrollback and then receive the
    // same bytes again from the live channel during the same handshake window.
    let scrollback_for_replay = scrollback.clone();
    let output_tx_for_replay = output_tx_arc.clone();
    let ws_output_tx_for_replay = ws_output_tx.clone();
    match spawn_blocking(move || {
        scrollback_for_replay.read_tail_and_then(MAX_SCROLLBACK_BYTES, || {
            let mut guard = output_tx_for_replay.lock().unwrap();
            *guard = Some(ws_output_tx_for_replay);
        })
    })
    .await
    {
        Ok(Ok((contents, _))) if !contents.is_empty() => {
            let _ = sender.send(Message::Binary(Bytes::from(contents))).await;
        }
        Ok(Ok((_contents, _))) => {}
        Ok(Err(e)) => {
            tracing::warn!("Failed to read scrollback for terminal {}: {}", pid, e);
            // Scrollback read failed, but we still need to enable live forwarding
            // so the WebSocket receives PTY output going forward.
            let mut guard = output_tx_arc.lock().unwrap();
            *guard = Some(ws_output_tx.clone());
        }
        _ => {
            // spawn_blocking itself failed; still enable forwarding.
            let mut guard = output_tx_arc.lock().unwrap();
            *guard = Some(ws_output_tx.clone());
        }
    }

    // WS input → PTY writer channel
    let (ws_input_tx, ws_input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let write_handle = {
        let writer = writer.clone();
        spawn_blocking(move || {
            while let Ok(data) = ws_input_rx.recv() {
                let mut guard = writer.blocking_lock();
                if guard.write_all(&data).is_err() || guard.flush().is_err() {
                    break;
                }
            }
        })
    };

    // Main loop with output coalescing
    let mut coalesce_buf: Vec<u8> = Vec::with_capacity(16384);
    let mut interval = tokio::time::interval(Duration::from_millis(8));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !coalesce_buf.is_empty() {
                    let frame = std::mem::replace(&mut coalesce_buf, Vec::with_capacity(16384));
                    if sender.send(Message::Binary(Bytes::from(frame))).await.is_err() {
                        break;
                    }
                }
            }
            data = ws_output_rx.recv() => {
                match data {
                    Some(data) => {
                        coalesce_buf.extend_from_slice(&data);
                        if coalesce_buf.len() >= 8192 {
                            let frame = std::mem::replace(&mut coalesce_buf, Vec::with_capacity(16384));
                            if sender.send(Message::Binary(Bytes::from(frame))).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => {
                        if !coalesce_buf.is_empty() {
                            let _ = sender.send(Message::Binary(Bytes::from(std::mem::take(&mut coalesce_buf)))).await;
                        }
                        break;
                    }
                }
            }
            _ = exit_notify.notified() => {
                // Give the reader a moment to flush remaining output
                tokio::time::sleep(Duration::from_millis(50)).await;

                while let Ok(data) = ws_output_rx.try_recv() {
                    coalesce_buf.extend_from_slice(&data);
                }
                if !coalesce_buf.is_empty() {
                    let _ = sender.send(Message::Binary(Bytes::from(std::mem::take(&mut coalesce_buf)))).await;
                }

                let success = exit_status_arc.lock().unwrap().unwrap_or(false);
                let exit_message = ProcessExitMessage {
                    exit_code: Some(if success { 0 } else { 1 }),
                    signal: None,
                    message: if success {
                        "Process exited successfully"
                    } else {
                        "Process exited with non-zero status"
                    }
                    .to_string(),
                };
                let exit_json = serde_json::to_string(&exit_message).unwrap_or_default();
                let _ = sender
                    .send(Message::Text(
                        format!("{{\"type\":\"exit\",\"data\":{exit_json}}}").into(),
                    ))
                    .await;

                sessions.remove(&pid);
                break;
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(message)) => {
                        let data = match message {
                            Message::Text(text) => text.as_bytes().to_vec(),
                            Message::Binary(data) => data.to_vec(),
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        if ws_input_tx.send(data).is_err() {
                            break;
                        }
                    }
                    None | Some(Err(_)) => break,
                }
            }
        }
    }

    // Disconnect: clear the output sender so background reader stops forwarding
    {
        let mut guard = output_tx_arc.lock().unwrap();
        *guard = None;
    }

    drop(ws_input_tx);
    let _ = write_handle.await;

    tracing::info!("WebSocket disconnected for terminal {}", pid);
}

pub async fn terminate_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
) -> impl IntoResponse {
    tracing::info!("Terminating terminal {}", pid);

    if let Some((_, session)) = sessions.remove(&pid) {
        let result = session
            .child_killer
            .lock()
            .await
            .kill()
            .map_err(|e| e.to_string());

        drop(session.writer.lock().await);
        session.scrollback.cleanup();

        match result {
            Ok(_) => {
                tracing::info!("Terminal {} terminated successfully", pid);
                Json(serde_json::json!({"success": true})).into_response()
            }
            Err(e) => {
                tracing::error!("Failed to terminate terminal {}: {}", pid, e);
                Json(ErrorResponse {
                    error: format!("Failed to terminate terminal {pid}: {e}"),
                })
                .into_response()
            }
        }
    } else {
        tracing::error!("Failed to terminate terminal {}: session not found", pid);
        Json(ErrorResponse {
            error: "Session not found".to_string(),
        })
        .into_response()
    }
}

pub async fn execute_command(Json(options): Json<ExecuteCommandOption>) -> impl IntoResponse {
    let cwd = options.cwd.or(options.u_cwd).unwrap_or("".to_string());

    tracing::info!(
        command = %options.command,
        cwd = %cwd,
        "Executing command"
    );

    let shell = String::from("sh");
    let cwd = if cwd.is_empty() {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    } else {
        PathBuf::from(cwd)
    };

    if !cwd.exists() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(CommandResponse {
                output: String::new(),
                error: Some("Working directory does not exist".to_string()),
            }),
        )
            .into_response();
    }

    let command = options.command.clone();

    let result = spawn_blocking(move || {
        let pty_system = native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(size)?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.args(["-c", &command]);
        cmd.cwd(cwd);

        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();

        let read_thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let timeout_duration = Duration::from_secs(30);
        let start_time = SystemTime::now();
        let mut output = Vec::new();

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(data) => {
                    output.extend(data);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if start_time.elapsed().unwrap_or_default() > timeout_duration {
                        child.kill()?;
                        return Err("Command execution timed out".into());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }

        drop(writer);
        let _ = read_thread.join();
        child.wait()?;

        Ok::<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>(output)
    })
    .await;

    match result {
        Ok(Ok(output)) => {
            let output_str = String::from_utf8_lossy(&output).into_owned();

            let ansi_regex =
                Regex::new(r"\x1B\[([0-9]{1,2}(;[0-9]{1,2})?)?[m|K]|\x1B\[[0-9]+[A-Za-z]").unwrap();
            let cleaned_output = ansi_regex.replace_all(&output_str, "").to_string();

            tracing::info!(
                output_length = cleaned_output.len(),
                "Command completed successfully"
            );

            (
                axum::http::StatusCode::OK,
                Json(CommandResponse {
                    output: cleaned_output,
                    error: None,
                }),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::error!("Command execution failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(CommandResponse {
                    output: String::new(),
                    error: Some(e.to_string()),
                }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Blocking task failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(CommandResponse {
                    output: String::new(),
                    error: Some("Internal server error".to_string()),
                }),
            )
                .into_response()
        }
    }
}
