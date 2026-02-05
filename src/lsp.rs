//! LSP WebSocket Proxy

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderValue;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use bytes::{Buf, BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use nom::{
    branch::alt,
    bytes::streaming::{is_not, tag, take_until},
    character::streaming::{char, crlf, digit1, space0},
    combinator::{map, map_res, opt},
    multi::length_data,
    sequence::{delimited, terminated, tuple},
    IResult,
};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::str;
use std::sync::Arc;
use std::time::Instant;
use sysinfo::{Pid, System};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
pub struct LspBridgeConfig {
    pub program: String,
    pub args: Vec<String>,
}

struct LspProcessInfo {
    pid: u32,
    started_at: Instant,
}

type ProcessRegistry = Arc<RwLock<HashMap<u32, LspProcessInfo>>>;

#[derive(Clone)]
struct LspState {
    config: Arc<LspBridgeConfig>,
    processes: ProcessRegistry,
}

#[derive(Serialize)]
struct LspProcessStatus {
    pid: u32,
    uptime_secs: u64,
    memory_bytes: u64,
}

#[derive(Serialize)]
struct LspStatusResponse {
    program: String,
    processes: Vec<LspProcessStatus>,
}

/// Guard that cleans up the port file on drop
struct PortFileGuard {
    path: std::path::PathBuf,
}

impl Drop for PortFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
            tracing::debug!("Cleaned up port file: {:?}", self.path);
        }
    }
}

/// Get the port file path for a given server and session
fn get_port_file_path(program: &str, session: Option<&str>) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(home)
        .join(".axs")
        .join("lsp_ports");

    // Use just the binary name (not full path)
    let server_name = std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);

    let filename = match session {
        Some(s) => format!("{}_{}", server_name, s),
        None => format!("{}_{}", server_name, std::process::id()),
    };

    dir.join(filename)
}

/// Write port to discovery file
fn write_port_file(path: &std::path::Path, port: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, port.to_string())
}

pub async fn start_lsp_server(
    host: Ipv4Addr,
    port: Option<u16>,
    session: Option<String>,
    allow_any_origin: bool,
    config: LspBridgeConfig,
) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=info,tower_http=info", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!(
        program = %config.program,
        args = ?config.args,
        "Starting LSP bridge server",
    );

    let cors = if allow_any_origin {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let localhost = "https://localhost"
            .parse::<HeaderValue>()
            .expect("valid origin");
        CorsLayer::new()
            .allow_origin(localhost)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let state = LspState {
        config: Arc::new(config.clone()),
        processes: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(upgrade_lsp_bridge))
        .route("/status", get(get_lsp_status))
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .layer(cors);

    // Use specified port or 0 for auto-selection
    let bind_port = port.unwrap_or(0);
    let addr: std::net::SocketAddr = (host, bind_port).into();

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let actual_addr = listener.local_addr().unwrap();
            let actual_port = actual_addr.port();

            tracing::info!("listening on {}", actual_addr);

            // Write port to discovery file
            let port_file_path = get_port_file_path(&config.program, session.as_deref());
            if let Err(e) = write_port_file(&port_file_path, actual_port) {
                tracing::warn!("Failed to write port file: {}", e);
            } else {
                tracing::info!("Port file: {:?}", port_file_path);
            }

            // Guard will clean up port file on drop
            let _guard = PortFileGuard {
                path: port_file_path,
            };

            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Server error: {}", e);
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                tracing::error!(
                    "Port {} is already in use. Please kill other instances or apps using this port.",
                    bind_port
                );
            } else {
                tracing::error!("Failed to bind: {}", e);
            }
        }
    }
}

async fn upgrade_lsp_bridge(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<LspState>,
) -> impl IntoResponse {
    let config = state.config.clone();
    let processes = state.processes.clone();
    ws.on_upgrade(move |socket| async move {
        tracing::info!("connected");
        if let Err(err) = run_bridge(socket, config, processes).await {
            tracing::error!(error = %err, "connection error");
        }
        tracing::info!("disconnected");
    })
}

async fn get_lsp_status(
    axum::extract::State(state): axum::extract::State<LspState>,
) -> Json<LspStatusResponse> {
    let processes = state.processes.read().await;
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let process_stats: Vec<LspProcessStatus> = processes
        .values()
        .filter_map(|info| {
            let uptime_secs = info.started_at.elapsed().as_secs();
            let memory_bytes = sys
                .process(Pid::from_u32(info.pid))
                .map(|p| p.memory())
                .unwrap_or(0);

            Some(LspProcessStatus {
                pid: info.pid,
                uptime_secs,
                memory_bytes,
            })
        })
        .collect();

    Json(LspStatusResponse {
        program: state.config.program.clone(),
        processes: process_stats,
    })
}

/// Run the bridge between a WebSocket client and an LSP server process
async fn run_bridge(
    socket: WebSocket,
    config: Arc<LspBridgeConfig>,
    processes: ProcessRegistry,
) -> Result<(), String> {
    let mut command = Command::new(&config.program);
    command.args(&config.args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    tracing::info!(
        "starting {} in {:?}",
        config.program,
        std::env::current_dir()
    );

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn LSP command '{}': {e}", config.program))?;

    tracing::trace!("running {}", config.program);

    let pid = child.id().ok_or_else(|| "Failed to get LSP process ID".to_string())?;

    {
        let mut procs = processes.write().await;
        procs.insert(pid, LspProcessInfo {
            pid,
            started_at: Instant::now(),
        });
    }

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to capture LSP stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture LSP stdout".to_string())?;

    if let Some(stderr) = child.stderr.take() {
        let program_name = config.program.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                eprintln!("[LSP-STDERR:{}] {}", program_name, line);
                tracing::warn!(target: "lsp_stderr", program = %program_name, "{}", line);
            }
        });
    }

    let cleanup_processes = processes.clone();

    // Create framed readers/writers
    let mut server_send = FramedWrite::new(stdin, LspFrameCodec::default());
    let mut server_recv = FramedRead::new(stdout, LspFrameCodec::default());

    // Split WebSocket
    let (mut client_send, client_recv) = socket.split();

    // Process client messages, filtering to just what we care about
    let mut client_recv = client_recv.filter_map(filter_map_ws_message).boxed();

    let mut client_msg = client_recv.next();
    let mut server_msg = server_recv.next();

    loop {
        tokio::select! {
            // From Client
            from_client = &mut client_msg => {
                match from_client {
                    // Text message from client
                    Some(Ok(ClientMessage::Text(text))) => {
                        tracing::trace!("-> {}", if text.len() > 200 { &text[..200] } else { &text });
                        if let Err(e) = server_send.send(text).await {
                            tracing::error!(error = %e, "failed to send to server");
                            break;
                        }
                    }

                    // Ping from client
                    Some(Ok(ClientMessage::Ping(data))) => {
                        if client_send.send(Message::Pong(data.into())).await.is_err() {
                            break;
                        }
                    }

                    // Pong from client (keep-alive response)
                    Some(Ok(ClientMessage::Pong)) => {
                        tracing::trace!("received pong");
                    }

                    // Close from client
                    Some(Ok(ClientMessage::Close)) => {
                        tracing::info!("received Close message");
                        break;
                    }

                    // WebSocket error
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "websocket error");
                        break;
                    }

                    // Connection closed
                    None => {
                        tracing::info!("connection closed");
                        break;
                    }
                }

                client_msg = client_recv.next();
            }

            // From Server
            from_server = &mut server_msg => {
                match from_server {
                    // Serialized LSP message
                    Some(Ok(text)) => {
                        tracing::trace!("<- {}", if text.len() > 200 { &text[..200] } else { &text });
                        if client_send.send(Message::Text(text.into())).await.is_err() {
                            tracing::error!("failed to send to client");
                            break;
                        }
                    }

                    // Codec error
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "codec error");
                    }

                    // Server exited
                    None => {
                        tracing::error!("server process exited unexpectedly");
                        let _ = client_send.send(Message::Close(None)).await;
                        break;
                    }
                }

                server_msg = server_recv.next();
            }
        }
    }

    {
        let mut procs = cleanup_processes.write().await;
        procs.remove(&pid);
    }

    Ok(())
}

// Client message handling

enum ClientMessage {
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

async fn filter_map_ws_message(
    msg: Result<Message, axum::Error>,
) -> Option<Result<ClientMessage, axum::Error>> {
    match msg {
        Ok(Message::Text(text)) => Some(Ok(ClientMessage::Text(text.to_string()))),
        Ok(Message::Binary(data)) => {
            // Try to decode as text
            match String::from_utf8(data.to_vec()) {
                Ok(text) => Some(Ok(ClientMessage::Text(text))),
                Err(_) => None, // Ignore non-UTF8 binary
            }
        }
        Ok(Message::Ping(data)) => Some(Ok(ClientMessage::Ping(data.to_vec()))),
        Ok(Message::Pong(_)) => Some(Ok(ClientMessage::Pong)),
        Ok(Message::Close(_)) => Some(Ok(ClientMessage::Close)),
        Err(e) => Some(Err(e)),
    }
}

#[derive(Debug)]
pub enum CodecError {
    MissingHeader,
    InvalidLength,
    InvalidType,
    Encode(std::io::Error),
    Utf8(std::str::Utf8Error),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeader => write!(f, "missing required `Content-Length` header"),
            Self::InvalidLength => write!(f, "unable to parse content length"),
            Self::InvalidType => write!(f, "unable to parse content type"),
            Self::Encode(e) => write!(f, "failed to encode frame: {}", e),
            Self::Utf8(e) => write!(f, "frame contains invalid UTF8: {}", e),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(e) => Some(e),
            Self::Utf8(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CodecError {
    fn from(error: std::io::Error) -> Self {
        Self::Encode(error)
    }
}

impl From<std::str::Utf8Error> for CodecError {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::Utf8(error)
    }
}

#[derive(Clone, Debug, Default)]
pub struct LspFrameCodec {
    remaining_bytes: usize,
}

impl Encoder<String> for LspFrameCodec {
    type Error = CodecError;

    fn encode(&mut self, item: String, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if !item.is_empty() {
            // Reserve space: "Content-Length: " (16) + digits + "\r\n\r\n" (4) + body
            dst.reserve(item.len() + number_of_digits(item.len()) + 20);
            let mut writer = dst.writer();
            write!(writer, "Content-Length: {}\r\n\r\n{}", item.len(), item)?;
            writer.flush()?;
        }
        Ok(())
    }
}

impl Decoder for LspFrameCodec {
    type Item = String;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if self.remaining_bytes > src.len() {
            return Ok(None);
        }

        match parse_message(src) {
            Ok((remaining, message)) => {
                let message = str::from_utf8(message)?.to_string();
                let len = src.len() - remaining.len();
                src.advance(len);
                self.remaining_bytes = 0;
                // Ignore empty frame
                if message.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(message))
                }
            }

            Err(nom::Err::Incomplete(nom::Needed::Size(needed))) => {
                self.remaining_bytes = needed.get();
                Ok(None)
            }

            Err(nom::Err::Incomplete(nom::Needed::Unknown)) => Ok(None),

            Err(nom::Err::Error(err)) | Err(nom::Err::Failure(err)) => {
                let code = err.code;
                let parsed_bytes = src.len() - err.input.len();
                src.advance(parsed_bytes);
                match find_next_message(src) {
                    Ok((_, position)) => src.advance(position),
                    Err(_) => src.advance(src.len()),
                }
                match code {
                    nom::error::ErrorKind::Digit | nom::error::ErrorKind::MapRes => {
                        Err(CodecError::InvalidLength)
                    }
                    nom::error::ErrorKind::Char | nom::error::ErrorKind::IsNot => {
                        Err(CodecError::InvalidType)
                    }
                    _ => Err(CodecError::MissingHeader),
                }
            }
        }
    }
}

#[inline]
fn number_of_digits(mut n: usize) -> usize {
    let mut num_digits = 0;
    while n > 0 {
        n /= 10;
        num_digits += 1;
    }
    num_digits
}

// LSP Message Parser

/// Get JSON message from input using the Content-Length header.
fn parse_message(input: &[u8]) -> IResult<&[u8], &[u8]> {
    let content_len = delimited(tag("Content-Length: "), digit1, crlf);

    let utf8 = alt((tag("utf-8"), tag("utf8")));
    let charset = tuple((char(';'), space0, tag("charset="), utf8));
    let content_type = tuple((tag("Content-Type: "), is_not(";\r"), opt(charset), crlf));

    let header = terminated(terminated(content_len, opt(content_type)), crlf);

    let header = map_res(header, str::from_utf8);
    let length = map_res(header, |s: &str| s.parse::<usize>());
    let mut message = length_data(length);

    message(input)
}

fn find_next_message(input: &[u8]) -> IResult<&[u8], usize> {
    map(take_until("Content-Length"), |s: &[u8]| s.len())(input)
}
