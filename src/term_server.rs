use axum::{
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use nix::pty::{openpty, Winsize};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    net::Ipv4Addr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::{Child, Command, Stdio},
    sync::Arc,
};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

struct TerminalSession {
    master_fd: Arc<Mutex<OwnedFd>>,
    child: Child,
    buffer: String,
}

type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;

#[derive(Deserialize)]
struct TerminalOptions {
    cols: u16,
    rows: u16,
}

#[derive(Serialize)]
struct TerminalResponse {
    pid: u32,
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
    let window_size = Winsize {
        ws_row: options.rows,
        ws_col: options.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    match openpty(&window_size, None) {
        Ok(pty) => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("/bin/sh"));

            let mut cmd = Command::new(&shell);
            cmd.stdin(unsafe { Stdio::from_raw_fd(pty.slave.as_raw_fd()) });
            cmd.stdout(unsafe { Stdio::from_raw_fd(pty.slave.as_raw_fd()) });
            cmd.stderr(unsafe { Stdio::from_raw_fd(pty.slave.as_raw_fd()) });

            match cmd.spawn() {
                Ok(child) => {
                    let pid = child.id();

                    let session = TerminalSession {
                        master_fd: Arc::new(Mutex::new(pty.master)),
                        child,
                        buffer: String::new(),
                    };

                    sessions.lock().await.insert(pid, session);
                    Json(TerminalResponse { pid }).into_response()
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
        let size = Winsize {
            ws_row: options.rows,
            ws_col: options.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let master_fd = session.master_fd.lock().await;
        match unsafe { libc::ioctl(master_fd.as_raw_fd(), libc::TIOCSWINSZ, &size as *const _) } {
            0 => Json(serde_json::json!({"success": true})).into_response(),
            _ => Json(ErrorResponse {
                error: "Failed to resize".to_string(),
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
    let (sender, mut receiver) = socket.split();

    let session_lock = sessions.lock().await;
    if let Some(session) = session_lock.get(&pid) {
        let master_fd = session.master_fd.clone();
        drop(session_lock);

        // Set close-on-exec and nonblocking for master fd
        let raw_fd = master_fd.lock().await.as_raw_fd();
        if let Err(e) = set_close_on_exec(raw_fd, true) {
            eprintln!("Failed to set close-on-exec: {}", e);
        }
        if let Err(e) = set_nonblocking(raw_fd) {
            eprintln!("Failed to set nonblocking: {}", e);
        }

        let ws_sender = Arc::new(Mutex::new(sender));
        let ws_sender_clone = ws_sender.clone();
        let read_master_fd = master_fd.clone();

        // Read from PTY
        tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            loop {
                let n = {
                    let mut file =
                        unsafe { File::from_raw_fd(read_master_fd.lock().await.as_raw_fd()) };
                    match file.read(&mut buffer) {
                        Ok(n) if n > 0 => {
                            std::mem::forget(file); // Don't close the fd
                            n
                        }
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

        // Write to PTY
        while let Some(Ok(message)) = receiver.next().await {
            let data = match message {
                axum::extract::ws::Message::Text(ref text) => text.as_bytes().to_vec(),
                axum::extract::ws::Message::Binary(ref data) => data.to_vec(),
                _ => continue,
            };

            let mut file = unsafe { File::from_raw_fd(master_fd.lock().await.as_raw_fd()) };

            let write_result = file.write_all(&data);
            std::mem::forget(file); // Don't close the fd

            if write_result.is_err() {
                break;
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

use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use std::os::unix::io::RawFd;

fn set_close_on_exec(fd: RawFd, close_on_exec: bool) -> std::io::Result<()> {
    let old_flags = match fcntl(fd, FcntlArg::F_GETFD) {
        Ok(flags) => FdFlag::from_bits_truncate(flags),
        Err(err) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("fcntl F_GETFD: {}", err),
            ));
        }
    };

    let mut new_flags = old_flags;
    new_flags.set(FdFlag::FD_CLOEXEC, close_on_exec);

    if old_flags == new_flags {
        return Ok(());
    }

    match fcntl(fd, FcntlArg::F_SETFD(new_flags)) {
        Ok(_) => Ok(()),
        Err(err) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("fcntl F_SETFD: {}", err),
        )),
    }
}

fn get_close_on_exec(fd: RawFd) -> std::io::Result<bool> {
    match fcntl(fd, FcntlArg::F_GETFD) {
        Ok(flags) => Ok(FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)),
        Err(err) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("fcntl F_GETFD: {}", err),
        )),
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let old_flags = match fcntl(fd, FcntlArg::F_GETFL) {
        Ok(flags) => OFlag::from_bits_truncate(flags),
        Err(err) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("fcntl F_GETFL: {}", err),
            ));
        }
    };

    let mut new_flags = old_flags;
    new_flags.set(OFlag::O_NONBLOCK, true);

    if old_flags != new_flags {
        match fcntl(fd, FcntlArg::F_SETFL(new_flags)) {
            Ok(_) => Ok(()),
            Err(err) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("fcntl F_SETFL: {}", err),
            )),
        }
    } else {
        Ok(())
    }
}
