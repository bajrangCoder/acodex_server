use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::Ipv4Addr,
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const MAX_BUFFER_SIZE: usize = 1_000_000;

struct CircularBuffer {
    data: Vec<u8>,
    position: usize,
    max_size: usize,
}

#[allow(dead_code)]
struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    buffer: Arc<Mutex<CircularBuffer>>,
    last_accessed: Arc<Mutex<SystemTime>>,
}

type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;

#[derive(Deserialize)]
struct TerminalOptions {
    cols: serde_json::Value,
    rows: serde_json::Value,
}

#[derive(Deserialize)]
struct ExecuteCommandOption {
    command: String,
    cwd: String,
}

#[derive(Serialize)]
pub struct CommandResponse {
    output: String,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

impl CircularBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            data: Vec::with_capacity(max_size),
            position: 0,
            max_size,
        }
    }

    fn write(&mut self, new_data: &[u8]) {
        for &byte in new_data {
            if self.data.len() < self.max_size {
                self.data.push(byte);
            } else {
                self.data[self.position] = byte;
                self.position = (self.position + 1) % self.max_size;
            }
        }
    }

    fn get_contents(&self) -> Vec<u8> {
        if self.data.len() < self.max_size {
            self.data.clone()
        } else {
            let mut result = Vec::with_capacity(self.max_size);
            result.extend_from_slice(&self.data[self.position..]);
            result.extend_from_slice(&self.data[..self.position]);
            result
        }
    }
}

#[tokio::main]
pub async fn start_server(host: Ipv4Addr, port: u16) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=info", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/terminals", post(create_terminal))
        .route("/terminals/:pid/resize", post(resize_terminal))
        .route("/terminals/:pid", get(terminal_websocket))
        .route("/terminals/:pid/terminate", post(terminate_terminal))
        .route("/execute-command", post(execute_command))
        .with_state(sessions)
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    let addr: std::net::SocketAddr = (host, port).into();

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

// temporary to maintain compatibility with older AcodeX versions
fn parse_u16(value: &serde_json::Value, field_name: &str) -> Result<u16, String> {
    match value {
        serde_json::Value::Number(n) if n.is_u64() => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| format!("{} must be a valid u16.", field_name)),
        serde_json::Value::String(s) => s
            .parse::<u16>()
            .map_err(|_| format!("{} must be a valid u16 string.", field_name)),
        _ => Err(format!(
            "{} must be a number or a valid string.",
            field_name
        )),
    }
}

async fn create_terminal(
    State(sessions): State<Sessions>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let rows = parse_u16(&options.rows, "rows").expect("failed");
    let cols = parse_u16(&options.cols, "cols").expect("failed");
    tracing::info!("Creating new terminal with cols={}, rows={}", cols, rows);

    let pty_system = native_pty_system();

    let shell = &std::env::var("SHELL").unwrap_or_else(|_| String::from("bash"));

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

                    let session = TerminalSession {
                        master,
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

async fn resize_terminal(
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

async fn terminal_websocket(
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
        drop(session_lock);

        // Send initial buffer contents
        let buffer_guard = buffer.lock().await;
        if let Ok(initial_content) = String::from_utf8(buffer_guard.get_contents()) {
            if !initial_content.is_empty() {
                let _ = sender.send(Message::Text(initial_content)).await;
            }
        }
        drop(buffer_guard);

        // Handle PTY output to WebSocket
        let pty_to_ws = {
            let buffer = buffer.clone();
            tokio::spawn(async move {
                let mut read_buffer = [0u8; 1024];
                loop {
                    let n = {
                        let mut reader = reader.lock().await;
                        match reader.read(&mut read_buffer) {
                            Ok(n) if n > 0 => n,
                            _ => break,
                        }
                    };

                    let data = &read_buffer[..n];

                    // Update circular buffer
                    let mut buffer_guard = buffer.lock().await;
                    buffer_guard.write(data);
                    drop(buffer_guard);

                    if let Ok(text) = String::from_utf8(data.to_vec()) {
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                }
            })
        };

        // Rest of your handle_socket implementation remains the same
        let ws_to_pty = {
            tokio::spawn(async move {
                while let Some(Ok(message)) = receiver.next().await {
                    let mut writer = writer.lock().await;
                    match message {
                        Message::Text(text) => {
                            if let Err(e) = writer.write_all(text.as_bytes()) {
                                tracing::error!("Failed to write to terminal {}: {}", pid, e);
                                break;
                            }
                        }
                        Message::Binary(data) => {
                            if let Err(e) = writer.write_all(&data) {
                                tracing::error!(
                                    "Failed to write binary data to terminal {}: {}",
                                    pid,
                                    e
                                );
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }

                    if let Err(e) = writer.flush() {
                        tracing::error!("Failed to flush terminal {}: {}", pid, e);
                        break;
                    }
                }
            })
        };

        // Wait for either task to complete
        tokio::select! {
            _ = pty_to_ws => {
                tracing::info!("PTY to WebSocket task completed for terminal {}", pid);
            }
            _ = ws_to_pty => {
                tracing::info!("WebSocket to PTY task completed for terminal {}", pid);
            }
        }
    } else {
        tracing::error!("Session {} not found", pid);
    }
}

async fn terminate_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
) -> impl IntoResponse {
    tracing::info!("Terminating terminal {}", pid);
    let mut sessions = sessions.lock().await;
    if sessions.remove(&pid).is_some() {
        tracing::info!("Terminal {} terminated successfully", pid);
        Json(serde_json::json!({"success": true})).into_response()
    } else {
        tracing::error!("Failed to terminate terminal {}: session not found", pid);
        Json(ErrorResponse {
            error: "Session not found".to_string(),
        })
        .into_response()
    }
}

async fn execute_command(Json(options): Json<ExecuteCommandOption>) -> impl IntoResponse {
    tracing::info!(
        command = %options.command,
        cwd = %options.cwd,
        "Executing command"
    );

    let shell = &std::env::var("SHELL").unwrap_or_else(|_| String::from("bash"));

    // Validate working directory
    let cwd = if options.cwd.is_empty() {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    } else {
        PathBuf::from(&options.cwd)
    };

    if !cwd.exists() {
        tracing::error!(path = ?cwd, "Working directory does not exist");
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(CommandResponse {
                output: String::new(),
                error: Some("Working directory does not exist".to_string()),
            }),
        )
            .into_response();
    }

    // Set up PTY
    let pty_system = native_pty_system();

    // Create PTY pair
    let size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = match pty_system.openpty(size) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "Failed to open PTY");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(CommandResponse {
                    output: String::new(),
                    error: Some("Failed to create PTY".to_string()),
                }),
            )
                .into_response();
        }
    };

    let mut cmd = CommandBuilder::new(shell);
    cmd.args(["-c", &options.command]);
    cmd.cwd(cwd);

    // Spawn the command
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn command");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(CommandResponse {
                    output: String::new(),
                    error: Some("Failed to spawn command".to_string()),
                }),
            )
                .into_response();
        }
    };
    drop(pair.slave);

    // Set up output reading
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();

    // Write command and read output in separate threads
    std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).ok();
        tx.send(output).ok();
    });

    // Wait for command completion with timeout
    let timeout_duration = Duration::from_secs(30);
    if (timeout(timeout_duration, async {
        match child.wait() {
            Ok(status) => {
                tracing::info!(
                     exit_code = ?status.exit_code(),
                     "Command completed"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "Command failed");
            }
        }
    })
    .await)
        .is_err()
    {
        tracing::warn!("Command execution timed out");
        child.kill().ok();
        return (
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Json(CommandResponse {
                output: String::new(),
                error: Some("Command execution timed out".to_string()),
            }),
        )
            .into_response();
    }

    // Clean up resources
    drop(writer);
    drop(pair.master);

    // Get command output
    let output = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(output) => String::from_utf8_lossy(&output).into_owned(),
        Err(_) => {
            tracing::error!("Failed to receive command output");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(CommandResponse {
                    output: String::new(),
                    error: Some("Failed to receive command output".to_string()),
                }),
            )
                .into_response();
        }
    };

    // Clean ANSI escape sequences
    let ansi_regex =
        Regex::new(r"\x1B\[([0-9]{1,2}(;[0-9]{1,2})?)?[m|K]|\x1B\[[0-9]+[A-Za-z]").unwrap();
    let cleaned_output = ansi_regex.replace_all(&output, "").to_string();

    tracing::info!(
        output_length = cleaned_output.len(),
        "Command output processed"
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
