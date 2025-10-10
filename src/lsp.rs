use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    State,
};
use axum::http::HeaderValue;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::process::ExitStatus;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, Duration, Instant};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct LspBridgeConfig {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone)]
struct LspState {
    config: Arc<LspBridgeConfig>,
}

pub async fn start_lsp_server(
    host: Ipv4Addr,
    port: u16,
    allow_any_origin: bool,
    config: LspBridgeConfig,
) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=info", env!("CARGO_CRATE_NAME")).into()
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
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/", get(upgrade_lsp_bridge))
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .layer(cors);

    let addr: std::net::SocketAddr = (host, port).into();

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!("listening on {}", listener.local_addr().unwrap());

            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Server error: {}", e);
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                tracing::error!("Port is already in use please kill all other instances of axs server or stop any other process or app that maybe be using port {}", port);
            } else {
                tracing::error!("Failed to bind: {}", e);
            }
        }
    }
}

async fn upgrade_lsp_bridge(
    ws: WebSocketUpgrade,
    State(state): State<LspState>,
) -> impl IntoResponse {
    let config = state.config.clone();
    ws.on_upgrade(move |socket| async move {
        if let Err(err) = run_bridge(socket, config).await {
            tracing::error!(error = %err, "LSP bridge session ended with error");
        }
    })
}

async fn run_bridge(socket: WebSocket, config: Arc<LspBridgeConfig>) -> Result<(), String> {
    let mut command = Command::new(&config.program);
    command.args(&config.args);
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn LSP command '{}': {e}", config.program))?;

    tracing::info!(
        program = %config.program,
        args = ?config.args,
        "WebSocket client connected; LSP process spawned",
    );

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture LSP stdout".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to capture LSP stdin".to_string())?;
    let stderr = child.stderr.take();

    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            if let Err(err) = forward_stderr(stderr).await {
                tracing::error!(error = %err, "Failed to read LSP stderr");
            }
        });
    }

    let (mut ws_sender, ws_receiver) = socket.split();
    let (ws_send_tx, mut ws_send_rx) = mpsc::channel::<Message>(32);
    let (client_closed_tx, client_closed_rx) = oneshot::channel::<()>();

    let stdout_task = {
        let tx = ws_send_tx.clone();
        tokio::spawn(async move { forward_stdout(stdout, tx).await })
    };

    let ws_sender_task = tokio::spawn(async move {
        while let Some(msg) = ws_send_rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_sender.close().await;
    });

    let ws_to_child_task = {
        let tx = ws_send_tx.clone();
        tokio::spawn(async move {
            forward_client_messages(ws_receiver, stdin, tx, client_closed_tx).await
        })
    };

    let exit_status = monitor_child(&mut child, client_closed_rx).await?;

    let _ = ws_send_tx.send(Message::Close(None)).await;
    drop(ws_send_tx);

    let _ = ws_to_child_task.await;
    let _ = stdout_task.await;
    let _ = ws_sender_task.await;

    if exit_status.success() {
        tracing::info!("LSP command exited cleanly");
    } else {
        tracing::warn!(?exit_status, "LSP command exited with non-zero status");
    }

    Ok(())
}

async fn forward_stdout(mut stdout: tokio::process::ChildStdout, tx: mpsc::Sender<Message>) {
    let mut buf = vec![0u8; 8192];
    let mut decoder = LspMessageFramer::default();

    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(err) = decoder.push(&buf[..n]) {
                    tracing::error!(error = %err, "Failed to decode LSP stdout stream");
                    break;
                }

                while let Some(frame) = decoder.next_message() {
                    let message = match String::from_utf8(frame.clone()) {
                        Ok(text) => Message::Text(text.into()),
                        Err(_) => Message::Binary(frame.into()),
                    };

                    if tx.send(message).await.is_err() {
                        return;
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::error!(error = %err, "Failed to read from LSP stdout");
                break;
            }
        }
    }
}

async fn forward_client_messages(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    mut stdin: tokio::process::ChildStdin,
    tx: mpsc::Sender<Message>,
    shutdown_tx: oneshot::Sender<()>,
) {
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                if let Err(err) = stdin.write_all(&data).await {
                    tracing::error!(error = %err, "Failed to write binary frame to LSP");
                    break;
                }
                if let Err(err) = stdin.flush().await {
                    tracing::error!(error = %err, "Failed to flush LSP stdin");
                    break;
                }
            }
            Ok(Message::Text(text)) => {
                if let Err(err) = stdin.write_all(text.as_bytes()).await {
                    tracing::error!(error = %err, "Failed to write text frame to LSP");
                    break;
                }
                if let Err(err) = stdin.flush().await {
                    tracing::error!(error = %err, "Failed to flush LSP stdin");
                    break;
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = tx.send(Message::Pong(payload)).await;
                continue;
            }
            Ok(Message::Pong(_)) => {
                continue;
            }
            Ok(Message::Close(frame)) => {
                let _ = tx.send(Message::Close(frame.clone())).await;
                break;
            }
            Err(err) => {
                tracing::error!(error = %err, "WebSocket receive error");
                break;
            }
        }
    }

    let _ = stdin.shutdown().await;
    let _ = shutdown_tx.send(());
}

#[derive(Default)]
struct LspMessageFramer {
    buffer: Vec<u8>,
    messages: VecDeque<Vec<u8>>,
}

impl LspMessageFramer {
    fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.buffer.extend_from_slice(chunk);

        loop {
            let Some(header_end) = find_header_terminator(&self.buffer) else {
                break;
            };

            let header = &self.buffer[..header_end];
            let content_length = parse_content_length(header)?;
            let frame_len = header_end + 4 + content_length; // include delimiter

            if self.buffer.len() < frame_len {
                break;
            }

            let frame = self.buffer.drain(..frame_len).collect::<Vec<u8>>();
            self.messages.push_back(frame);
        }

        Ok(())
    }

    fn next_message(&mut self) -> Option<Vec<u8>> {
        self.messages.pop_front()
    }
}

fn find_header_terminator(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(header: &[u8]) -> Result<usize, String> {
    let header_str =
        std::str::from_utf8(header).map_err(|_| "Invalid UTF-8 in LSP header".to_string())?;

    for line in header_str.split("\r\n") {
        let mut parts = line.splitn(2, ':');
        let key = parts.next().map(str::trim);
        let value = parts.next().map(str::trim);

        if let (Some(key), Some(value)) = (key, value) {
            if key.eq_ignore_ascii_case("content-length") {
                return value
                    .parse::<usize>()
                    .map_err(|_| format!("Invalid Content-Length header: {value}"));
            }
        }
    }

    Err("Missing Content-Length header".to_string())
}

async fn forward_stderr(stderr: ChildStderr) -> Result<(), std::io::Error> {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break;
        }

        tracing::warn!(target: "lsp_stderr", message = %line.trim_end());
    }

    Ok(())
}

async fn monitor_child(
    child: &mut Child,
    mut client_closed: oneshot::Receiver<()>,
) -> Result<ExitStatus, String> {
    loop {
        tokio::select! {
            res = &mut client_closed => {
                if res.is_err() {
                    tracing::debug!("LSP client channel dropped without close signal");
                }

                let deadline = Instant::now() + GRACEFUL_SHUTDOWN;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => return Ok(status),
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                break;
                            }
                            sleep(EXIT_POLL_INTERVAL).await;
                        }
                        Err(err) => return Err(format!("Failed to poll LSP process: {err}")),
                    }
                }

                child
                    .kill()
                    .await
                    .map_err(|e| format!("Failed to terminate LSP process: {e}"))?;
                return child
                    .wait()
                    .await
                    .map_err(|e| format!("Failed to await LSP process exit: {e}"));
            }
            _ = sleep(EXIT_POLL_INTERVAL) => {
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(err) => return Err(format!("Failed to poll LSP process: {err}")),
                }
            }
        }
    }
}
