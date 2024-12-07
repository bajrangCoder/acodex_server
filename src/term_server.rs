use axum::{
    extract::{Path, State, WebSocketUpgrade},
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

struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    buffer: String,
}

type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;

#[derive(Deserialize)]
struct TerminalOptions {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
pub async fn start_server(host: Ipv4Addr, port: u16) {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/terminals", post(create_terminal))
        .route("/terminals/:pid/resize", post(resize_terminal))
        .route("/terminals/:pid/ws", get(terminal_websocket))
        .route("/terminals/:pid/terminate", post(terminate_terminal))
        .with_state(sessions)
        .layer(cors);

    let addr: std::net::SocketAddr = (host, port).into();
    println!("Starting server on {:?}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn create_terminal(
    State(sessions): State<Sessions>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let pty_system = native_pty_system();

    let shell = &std::env::var("SHELL").unwrap_or_else(|_| String::from("bash"));

    let size = PtySize {
        rows: options.rows,
        cols: options.cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    match pty_system.openpty(size) {
        Ok(pair) => {
            let cmd = CommandBuilder::new(shell);
            match pair.slave.spawn_command(cmd) {
                Ok(child) => {
                    let pid = child.process_id().unwrap_or(0);
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
                Err(e) => Json(ErrorResponse {
                    error: format!("Failed to spawn command: {}", e),
                })
                .into_response(),
            }
        }
        Err(e) => Json(ErrorResponse {
            error: format!("Failed to open PTY: {}", e),
        })
        .into_response(),
    }
}

async fn resize_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
    Json(options): Json<TerminalOptions>,
) -> impl IntoResponse {
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(&pid) {
        let size = PtySize {
            rows: options.rows,
            cols: options.cols,
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
    ws.on_upgrade(move |socket| handle_socket(socket, pid, sessions))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, pid: u32, sessions: Sessions) {
    let (mut sender, mut receiver) = socket.split();

    let session_lock = sessions.lock().await;
    if let Some(session) = session_lock.get(&pid) {
        let reader = session.reader.clone();
        let writer = session.writer.clone();
        drop(session_lock);

        // Handle incoming data from PTY
        let ws_sender = Arc::new(Mutex::new(sender));
        let ws_sender_clone = ws_sender.clone();

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
                    if let Err(_) = ws_sender_clone
                        .lock()
                        .await
                        .send(axum::extract::ws::Message::Text(text))
                        .await
                    {
                        break;
                    }
                }
            }
        });

        // Handle incoming WebSocket messages
        while let Some(Ok(message)) = receiver.next().await {
            match message {
                axum::extract::ws::Message::Text(text) => {
                    let mut writer = writer.lock().await;
                    if writer.write_all(text.as_bytes()).is_err() {
                        break;
                    }
                }
                axum::extract::ws::Message::Binary(data) => {
                    let mut writer = writer.lock().await;
                    if writer.write_all(&data).is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
}

async fn terminate_terminal(
    State(sessions): State<Sessions>,
    Path(pid): Path<u32>,
) -> impl IntoResponse {
    let mut sessions = sessions.lock().await;
    if sessions.remove(&pid).is_some() {
        Json(serde_json::json!({"success": true})).into_response()
    } else {
        Json(ErrorResponse {
            error: "Session not found".to_string(),
        })
        .into_response()
    }
}
