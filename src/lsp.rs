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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};

use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
pub struct LspBridgeConfig {
    pub program: String,
    pub args: Vec<String>,
}

/// Unique client identifier
type ClientId = u64;

/// Message to be sent to the LSP stdin
struct LspInputMessage {
    /// The raw bytes to write (with Content-Length header already included)
    data: Vec<u8>,
}

/// Shared state for the LSP process
struct SharedLspProcess {
    /// Channel to send messages to LSP stdin
    stdin_tx: mpsc::Sender<LspInputMessage>,
    /// Broadcast channel for LSP stdout messages
    stdout_tx: broadcast::Sender<Vec<u8>>,
    /// Handle to monitor the LSP process (kept alive for task lifetime)
    #[allow(dead_code)]
    monitor_handle: tokio::task::JoinHandle<()>,
    /// Counter for active clients
    active_clients: Arc<AtomicU64>,
}

/// State shared across all WebSocket connections
struct LspState {
    config: Arc<LspBridgeConfig>,
    /// The shared LSP process, if spawned
    lsp_process: Arc<RwLock<Option<SharedLspProcess>>>,
    /// Mutex to ensure only one process is spawned at a time
    spawn_lock: Arc<Mutex<()>>,
    /// Counter for generating unique client IDs
    client_id_counter: Arc<AtomicU64>,
}

impl Clone for LspState {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            lsp_process: self.lsp_process.clone(),
            spawn_lock: self.spawn_lock.clone(),
            client_id_counter: self.client_id_counter.clone(),
        }
    }
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
        "Starting LSP bridge server (single-process mode)",
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
        lsp_process: Arc::new(RwLock::new(None)),
        spawn_lock: Arc::new(Mutex::new(())),
        client_id_counter: Arc::new(AtomicU64::new(1)),
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
    ws.on_upgrade(move |socket| async move {
        if let Err(err) = handle_client(socket, state).await {
            tracing::error!(error = %err, "LSP bridge session ended with error");
        }
    })
}

/// Ensures the LSP process is running, spawning it if necessary
async fn ensure_lsp_process(state: &LspState) -> Result<(), String> {
    // Quick check without lock
    {
        let guard = state.lsp_process.read().await;
        if guard.is_some() {
            return Ok(());
        }
    }

    // Acquire spawn lock to prevent race conditions
    let _spawn_guard = state.spawn_lock.lock().await;

    // Double-check after acquiring lock
    {
        let guard = state.lsp_process.read().await;
        if guard.is_some() {
            return Ok(());
        }
    }

    // Spawn the LSP process
    spawn_lsp_process(state).await
}

/// Spawns a new LSP process and sets up communication channels
async fn spawn_lsp_process(state: &LspState) -> Result<(), String> {
    let config = &state.config;

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
        "LSP process spawned (shared across all clients)",
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

    // Handle stderr logging
    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            if let Err(err) = forward_stderr(stderr).await {
                tracing::error!(error = %err, "Failed to read LSP stderr");
            }
        });
    }

    // Create channels for communication
    let (stdin_tx, stdin_rx) = mpsc::channel::<LspInputMessage>(256);
    let (stdout_tx, _) = broadcast::channel::<Vec<u8>>(256);
    let active_clients = Arc::new(AtomicU64::new(0));

    // Spawn stdin writer task
    let stdin_handle = tokio::spawn(forward_to_lsp_stdin(stdin, stdin_rx));

    // Spawn stdout reader task
    let stdout_tx_clone = stdout_tx.clone();
    let stdout_handle = tokio::spawn(forward_lsp_stdout(stdout, stdout_tx_clone));

    // Spawn monitor task
    let lsp_process_ref = state.lsp_process.clone();
    let config_clone = state.config.clone();
    let active_clients_clone = active_clients.clone();

    let monitor_handle = tokio::spawn(async move {
        // Wait for either stdin or stdout tasks to complete (indicating LSP exit)
        tokio::select! {
            _ = stdin_handle => {
                tracing::debug!("LSP stdin task completed");
            }
            _ = stdout_handle => {
                tracing::debug!("LSP stdout task completed");
            }
        }

        // Wait for the child process to exit
        let exit_result = child.wait().await;

        match exit_result {
            Ok(status) => {
                if status.success() {
                    tracing::info!("LSP process exited cleanly");
                } else {
                    tracing::warn!(?status, "LSP process exited with non-zero status");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to wait for LSP process");
            }
        }

        // Clear the shared process so a new one can be spawned
        {
            let mut guard = lsp_process_ref.write().await;
            *guard = None;
        }

        // If there are still active clients, we should respawn
        let remaining_clients = active_clients_clone.load(Ordering::SeqCst);
        if remaining_clients > 0 {
            tracing::warn!(
                clients = remaining_clients,
                "LSP process died while clients still connected",
            );
        }

        tracing::info!(
            program = %config_clone.program,
            "LSP process terminated, will respawn on next client request"
        );
    });

    // Store the shared process
    {
        let mut guard = state.lsp_process.write().await;
        *guard = Some(SharedLspProcess {
            stdin_tx,
            stdout_tx,
            monitor_handle,
            active_clients,
        });
    }

    Ok(())
}

/// Handles a single WebSocket client connection
async fn handle_client(socket: WebSocket, state: LspState) -> Result<(), String> {
    let client_id = state.client_id_counter.fetch_add(1, Ordering::SeqCst);

    tracing::info!(client_id, "WebSocket client connected");

    // Ensure LSP process is running
    ensure_lsp_process(&state).await?;

    // Get access to the shared LSP process
    let (stdin_tx, mut stdout_rx) = {
        let guard = state.lsp_process.read().await;
        let lsp = guard
            .as_ref()
            .ok_or_else(|| "LSP process not available".to_string())?;

        // Increment active client count
        lsp.active_clients.fetch_add(1, Ordering::SeqCst);

        (lsp.stdin_tx.clone(), lsp.stdout_tx.subscribe())
    };

    let (mut ws_sender, ws_receiver) = socket.split();
    let (ws_send_tx, mut ws_send_rx) = mpsc::channel::<Message>(32);
    let (client_closed_tx, client_closed_rx) = oneshot::channel::<()>();

    // Task to forward messages from WebSocket to LSP stdin
    let stdin_tx_clone = stdin_tx.clone();
    let ws_send_tx_clone = ws_send_tx.clone();
    let ws_to_lsp_task = tokio::spawn(async move {
        forward_client_to_lsp(
            ws_receiver,
            stdin_tx_clone,
            ws_send_tx_clone,
            client_closed_tx,
            client_id,
        )
        .await
    });

    // Task to forward LSP stdout to this WebSocket
    let ws_send_tx_clone = ws_send_tx.clone();
    let lsp_to_ws_task = tokio::spawn(async move {
        loop {
            match stdout_rx.recv().await {
                Ok(data) => {
                    let message = match String::from_utf8(data.clone()) {
                        Ok(text) => Message::Text(text.into()),
                        Err(_) => Message::Binary(data.into()),
                    };

                    if ws_send_tx_clone.send(message).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        client_id,
                        skipped,
                        "Client lagged behind, some messages were skipped"
                    );
                }
            }
        }
    });

    // Task to send messages through WebSocket
    let ws_sender_task = tokio::spawn(async move {
        while let Some(msg) = ws_send_rx.recv().await {
            if let Message::Close(_) = msg {
                let _ = ws_sender.send(msg).await;
                break;
            }
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_sender.close().await;
    });

    // Wait for client to close
    let _ = client_closed_rx.await;

    // Clean up
    let _ = ws_send_tx.send(Message::Close(None)).await;
    drop(ws_send_tx);

    lsp_to_ws_task.abort();
    let _ = ws_to_lsp_task.await;
    let _ = ws_sender_task.await;

    // Decrement active client count
    {
        let guard = state.lsp_process.read().await;
        if let Some(lsp) = guard.as_ref() {
            let remaining = lsp.active_clients.fetch_sub(1, Ordering::SeqCst) - 1;
            tracing::info!(
                client_id,
                remaining_clients = remaining,
                "WebSocket client disconnected"
            );
        }
    }

    Ok(())
}

/// Forwards messages from WebSocket client to LSP stdin
async fn forward_client_to_lsp(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    stdin_tx: mpsc::Sender<LspInputMessage>,
    ws_tx: mpsc::Sender<Message>,
    shutdown_tx: oneshot::Sender<()>,
    client_id: ClientId,
) {
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                // Binary messages are sent as-is (already has Content-Length header)
                if stdin_tx
                    .send(LspInputMessage {
                        data: data.to_vec(),
                    })
                    .await
                    .is_err()
                {
                    tracing::error!(client_id, "Failed to send to LSP stdin channel");
                    break;
                }
            }
            Ok(Message::Text(text)) => {
                // Text messages need Content-Length header added
                let body = text.as_bytes();
                let header = format!("Content-Length: {}\r\n\r\n", body.len());
                let mut data = Vec::with_capacity(header.len() + body.len());
                data.extend_from_slice(header.as_bytes());
                data.extend_from_slice(body);

                if stdin_tx.send(LspInputMessage { data }).await.is_err() {
                    tracing::error!(client_id, "Failed to send to LSP stdin channel");
                    break;
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = ws_tx.send(Message::Pong(payload)).await;
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                let _ = ws_tx.send(Message::Close(frame)).await;
                break;
            }
            Err(err) => {
                tracing::error!(client_id, error = %err, "WebSocket receive error");
                break;
            }
        }
    }

    let _ = shutdown_tx.send(());
}

/// Forwards messages from channel to LSP stdin
async fn forward_to_lsp_stdin(mut stdin: ChildStdin, mut rx: mpsc::Receiver<LspInputMessage>) {
    while let Some(msg) = rx.recv().await {
        if let Err(err) = stdin.write_all(&msg.data).await {
            tracing::error!(error = %err, "Failed to write to LSP stdin");
            break;
        }
        if let Err(err) = stdin.flush().await {
            tracing::error!(error = %err, "Failed to flush LSP stdin");
            break;
        }
    }

    let _ = stdin.shutdown().await;
}

/// Reads LSP stdout and broadcasts to all connected clients
async fn forward_lsp_stdout(mut stdout: ChildStdout, tx: broadcast::Sender<Vec<u8>>) {
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
                    // Broadcast to all connected clients
                    // It's okay if there are no receivers
                    let _ = tx.send(frame);
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

            let headers = &self.buffer[..header_end];
            let content_length = parse_content_length(headers)?;
            let body_start = header_end + 4;
            let frame_len = body_start + content_length;

            if self.buffer.len() < frame_len {
                break;
            }

            let body = self.buffer[body_start..frame_len].to_vec();
            self.buffer.drain(..frame_len);
            self.messages.push_back(body);
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
