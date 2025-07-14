use super::buffer::CircularBuffer;
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
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
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
    pub child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    pub reader: Arc<Mutex<Box<dyn Read + Send>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub buffer: Arc<Mutex<CircularBuffer>>,
    pub last_accessed: Arc<Mutex<SystemTime>>,
}

pub async fn create_terminal(
    State(sessions): State<Sessions>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let rows = parse_u16(&options.rows, "rows").expect("failed");
    let cols = parse_u16(&options.cols, "cols").expect("failed");
    tracing::info!("Creating new terminal with cols={}, rows={}", cols, rows);

    let pty_system = native_pty_system();

    let shell = String::from("login");

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    match pty_system.openpty(size) {
        Ok(pair) => {
            let cmd = CommandBuilder::new(shell);
            match pair.slave.spawn_command(cmd) {
                Ok(child) => {
                    let pid = child.process_id().unwrap_or(0);
                    tracing::info!("Terminal created successfully with PID: {}", pid);
                    drop(pair.slave); // Release slave after spawning

                    let reader = Arc::new(Mutex::new(pair.master.try_clone_reader().unwrap()));
                    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
                    let master = Arc::new(Mutex::new(pair.master as Box<dyn MasterPty + Send>));
                    let child_killer = Arc::new(Mutex::new(child.clone_killer()));

                    let session = TerminalSession {
                        master,
                        child_killer,
                        child: Arc::new(Mutex::new(child)),
                        reader,
                        writer,
                        buffer: Arc::new(Mutex::new(CircularBuffer::new(MAX_BUFFER_SIZE))),
                        last_accessed: Arc::new(Mutex::new(SystemTime::now())),
                    };

                    sessions.lock().await.insert(pid, session);
                    (axum::http::StatusCode::OK, pid.to_string()).into_response()
                }
                Err(e) => {
                    tracing::error!("Failed to spawn command: {}", e);
                    Json(ErrorResponse {
                        error: format!("Failed to spawn command: {}", e),
                    })
                    .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to open PTY: {}", e);
            Json(ErrorResponse {
                error: format!("Failed to open PTY: {}", e),
            })
            .into_response()
        }
    }
}

pub async fn resize_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let rows = parse_u16(&options.rows, "rows").expect("Failed");
    let cols = parse_u16(&options.cols, "cols").expect("Failed");
    tracing::info!("Resizing terminal {} to cols={}, rows={}", pid, cols, rows);
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(&pid) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        match session.master.lock().await.resize(size) {
            Ok(_) => Json(serde_json::json!({"success": true})).into_response(),
            Err(e) => Json(ErrorResponse {
                error: format!("Failed to resize: {}", e),
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

    let session_lock = sessions.lock().await;
    if let Some(session) = session_lock.get(&pid) {
        // Update last accessed time
        let mut last_accessed = session.last_accessed.lock().await;
        *last_accessed = SystemTime::now();
        drop(last_accessed);

        tracing::info!("WebSocket connection established for terminal {}", pid);
        let reader = session.reader.clone();
        let writer = session.writer.clone();
        let buffer = session.buffer.clone();
        let child = session.child.clone();
        drop(session_lock);

        // Send initial buffer contents
        let buffer_guard = buffer.lock().await;
        let contents = buffer_guard.get_contents();
        if !contents.is_empty() {
            let _ = sender.send(Message::Binary(contents)).await;
        }
        drop(buffer_guard);

        // Create channels for communication
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(100);
        let (exit_tx, mut exit_rx) = tokio::sync::mpsc::channel::<portable_pty::ExitStatus>(1);

        let pty_to_ws = {
            let reader = reader.clone();
            let child = child.clone();

            tokio::spawn(async move {
                // Spawn read task
                let read_task = spawn_blocking({
                    let reader = reader.clone();
                    let output_tx = output_tx.clone();
                    move || {
                        let mut read_buffer = [0u8; 1024];
                        loop {
                            let n = {
                                let mut reader_guard = reader.blocking_lock();
                                match reader_guard.read(&mut read_buffer) {
                                    Ok(n) if n > 0 => n,
                                    _ => break,
                                }
                            };

                            let data = read_buffer[..n].to_vec();
                            if output_tx.blocking_send(data).is_err() {
                                break;
                            }
                        }
                    }
                });

                // Spawn wait task
                let wait_task = spawn_blocking({
                    let child = child.clone();
                    let exit_tx = exit_tx.clone();
                    move || {
                        let mut child_guard = child.blocking_lock();
                        if let Ok(exit_status) = child_guard.wait() {
                            let _ = exit_tx.blocking_send(exit_status);
                        }
                    }
                });

                // Wait for tasks to complete
                let _ = tokio::join!(read_task, wait_task);
            })
        };

        // Handle output and exit events
        let output_handler = tokio::spawn(async move {
            loop {
                tokio::select! {
                    data = output_rx.recv() => {
                        if let Some(data) = data {
                            let mut buffer_guard = buffer.lock().await;
                            buffer_guard.write(&data);
                            drop(buffer_guard);

                            if sender.send(Message::Binary(Bytes::from(data))).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    exit_status = exit_rx.recv() => {
                        if let Some(exit_status) = exit_status {
                            let exit_message = if exit_status.success() {
                                ProcessExitMessage {
                                    exit_code: Some(0),
                                    signal: None,
                                    message: "Process exited successfully".to_string(),
                                }
                            } else {
                                ProcessExitMessage {
                                    exit_code: Some(1),
                                    signal: None,
                                    message: "Process exited with non-zero status".to_string(),
                                }
                            };

                            // Send exit message to WebSocket
                            let exit_json = serde_json::to_string(&exit_message).unwrap_or_else(|_|
                                "{\"exit_code\":1,\"signal\":null,\"message\":\"Process exited\"}".to_string()
                            );

                            let _ = sender.send(Message::Text(format!("{{\"type\":\"exit\",\"data\":{}}}", exit_json).into())).await;

                            // Remove session from sessions map
                            let mut sessions_guard = sessions.lock().await;
                            sessions_guard.remove(&pid);
                            break;
                        }
                    }
                }
            }
        });

        // Handle WebSocket input to PTY
        let ws_to_pty = {
            let writer = writer.clone();
            tokio::spawn(async move {
                let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
                let tx = std::sync::Arc::new(tx);
                let tx_clone = tx.clone();

                let write_handle = spawn_blocking(move || {
                    while let Ok(data) = rx.recv() {
                        let mut writer_guard = writer.blocking_lock();
                        if writer_guard.write_all(&data).is_err() || writer_guard.flush().is_err() {
                            break;
                        }
                    }
                });

                while let Some(Ok(message)) = receiver.next().await {
                    let data: axum::body::Bytes = match message {
                        Message::Text(text) => Bytes::from(text),
                        Message::Binary(data) => data,
                        Message::Close(_) => break,
                        _ => continue,
                    };

                    if tx_clone.send(data.to_vec()).is_err() {
                        break;
                    }
                }

                // Clean up
                drop(tx_clone);
                let _ = write_handle.await;
            })
        };

        // Wait for any task to complete
        tokio::select! {
            _ = pty_to_ws => {
                tracing::info!("PTY to WebSocket task completed for terminal {}", pid);
            }
            _ = output_handler => {
                tracing::info!("Output handler completed for terminal {}", pid);
            }
            _ = ws_to_pty => {
                tracing::info!("WebSocket to PTY task completed for terminal {}", pid);
            }
        }
    } else {
        tracing::error!("Session {} not found", pid);
    }
}

pub async fn terminate_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
) -> impl IntoResponse {
    tracing::info!("Terminating terminal {}", pid);
    let mut sessions = sessions.lock().await;

    if let Some(session) = sessions.remove(&pid) {
        let result = {
            session
                .child_killer
                .lock()
                .await
                .kill()
                .map_err(|e| e.to_string())
        };
        drop(session.writer.lock().await);
        drop(session.reader.lock().await);

        // Clear the circular buffer
        if let Ok(mut buffer) = session.buffer.try_lock() {
            buffer.data.clear();
        }

        match result {
            Ok(_) => {
                tracing::info!("Terminal {} terminated successfully", pid);
                Json(serde_json::json!({"success": true})).into_response()
            }
            Err(e) => {
                tracing::error!("Failed to terminate terminal {}: {}", pid, e);
                Json(ErrorResponse {
                    error: format!("Failed to terminate terminal {}: {}", pid, e),
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

    let shell = String::from("login");
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

    // Execute command in a blocking task
    let result = spawn_blocking(move || {
        // Set up PTY
        let pty_system = native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(size)?;

        // Set up command
        let mut cmd = CommandBuilder::new(shell);
        cmd.args(["-c", &command]);
        cmd.cwd(cwd);

        // Spawn the command
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        // Create channel for reading output
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // Spawn a thread for reading
        let read_thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Collect output with timeout
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

            // Check if process has finished
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }

        // Clean up resources
        drop(writer);
        let _ = read_thread.join();
        child.wait()?;

        Ok::<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>(output)
    })
    .await;

    // Process the result
    match result {
        Ok(Ok(output)) => {
            let output_str = String::from_utf8_lossy(&output).into_owned();

            // Clean ANSI escape sequences
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
