mod handlers;
// Not gated with #[cfg(target_os)] — this crate exclusively targets
// Linux/Android and will never be built for other platforms.
mod pty_fallback;
mod scrollback;
mod types;

use axum::{
    routing::{get, post},
    Router,
};

use axum::http::HeaderValue;
use dashmap::DashMap;
use std::env;
use std::io::Write;
use std::sync::OnceLock;
use std::{io::ErrorKind, net::Ipv4Addr, sync::Arc};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use handlers::*;
use types::Sessions;

static DEFAULT_COMMAND: OnceLock<String> = OnceLock::new();

pub fn set_default_command(cmd: String) {
    let _ = DEFAULT_COMMAND.set(cmd);
}

pub fn get_default_command() -> Option<&'static str> {
    DEFAULT_COMMAND.get().map(|s| s.as_str())
}

pub async fn start_server(host: Ipv4Addr, port: u16, allow_any_origin: bool) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=info", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let sessions: Sessions = Arc::new(DashMap::new());

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

    let app = Router::new()
        .route("/", get(|| async { "Rust based AcodeX server" }))
        .route("/terminals", post(create_terminal))
        .route("/terminals/{pid}/resize", post(resize_terminal))
        .route("/terminals/{pid}", get(terminal_websocket))
        .route("/terminals/{pid}/terminate", post(terminate_terminal))
        .route("/execute-command", post(execute_command))
        .route("/status", get(|| async { "OK" }))
        .with_state(sessions)
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    let addr: std::net::SocketAddr = (host, port).into();

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!("listening on {}", listener.local_addr().unwrap());

            // Notify parent process via FIFO that the server is ready to accept
            // connections. The parent shell creates a named pipe and sets
            // AXS_READY_PIPE; we write "READY\n" and close. The parent's blocking
            // `read` returns immediately — no HTTP polling needed.
            //
            // open() on a FIFO write-end blocks until a reader opens it. This is
            // acceptable and should never happen in practice:
            // - Only callers that explicitly set AXS_READY_PIPE opt into this
            //   path; legacy callers are unaffected.
            // - A caller that sets the variable is expected to follow the pipe
            //   protocol (`mkfifo; axs &; read < pipe`), so a reader is always
            //   present before axs reaches this point.
            // - A caller that sets the variable yet deliberately violates the
            //   protocol deserves no fallback — blocking is a visible symptom
            //   that exposes the misconfiguration rather than hiding it.
            if let Ok(pipe_path) = env::var("AXS_READY_PIPE") {
                if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&pipe_path) {
                    let _ = f.write_all(b"READY\n");
                }
            }

            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Server error: {}", e);
            }
        }
        Err(e) => {
            if e.kind() == ErrorKind::AddrInUse {
                tracing::error!("Port is already in use please kill all other instances of axs server or stop any other process or app that maybe be using port {}", port);
            } else {
                tracing::error!("Failed to bind: {}", e);
            }
        }
    }
}
