use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    State,
};
use axum::http::HeaderValue;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
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
    let mut buffer = vec![0u8; 8192];
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                if tx
                    .send(Message::Binary(buffer[..n].to_vec().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
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
            }
            Ok(Message::Text(text)) => {
                if let Err(err) = stdin.write_all(text.as_bytes()).await {
                    tracing::error!(error = %err, "Failed to write text frame to LSP");
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
