mod buffer;
mod handlers;
mod types;

use axum::{
    routing::{get, post},
    Router,
};

use std::{collections::HashMap, sync::Arc};
use std::{io::ErrorKind, net::Ipv4Addr};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use handlers::*;

pub type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;

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
        .route("/terminals/{pid}/resize", post(resize_terminal))
        .route("/terminals/{pid}", get(terminal_websocket))
        .route("/terminals/{pid}/terminate", post(terminate_terminal))
        .route("/execute-command", post(execute_command))
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
