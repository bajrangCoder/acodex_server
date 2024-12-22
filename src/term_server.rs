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
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::Ipv4Addr,
    sync::Arc,
};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[allow(dead_code)]
struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    buffer: String,
}

type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;

#[derive(Deserialize)]
struct TerminalOptions {
    cols: serde_json::Value,
    rows: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
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
                        buffer: String::new(),
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
        tracing::info!("WebSocket connection established for terminal {}", pid);
        let reader = session.reader.clone();
        let writer = session.writer.clone();
        drop(session_lock); // Drop the lock early to prevent deadlock

        // Handle PTY output to WebSocket
        let pty_to_ws = {
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                loop {
                    let n = {
                        let mut reader = reader.lock().await;
                        match reader.read(&mut buffer) {
                            Ok(n) if n > 0 => n,
                            _ => break,
                        }
                    };

                    if let Ok(text) = String::from_utf8(buffer[..n].to_vec()) {
                        if sender.send(Message::Text(text)).await.is_err() {
                            tracing::error!(
                                "Failed to send WebSocket message for terminal {}",
                                pid
                            );
                            break;
                        }
                    }
                }
            })
        };

        // Handle WebSocket input to PTY
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
