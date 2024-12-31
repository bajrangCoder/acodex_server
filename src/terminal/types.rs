use super::handlers::TerminalSession;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

pub const MAX_BUFFER_SIZE: usize = 1_000_000;

#[derive(Deserialize)]
pub struct TerminalOptions {
    pub cols: serde_json::Value,
    pub rows: serde_json::Value,
}

#[derive(Deserialize)]
pub struct ExecuteCommandOption {
    pub command: String,
    pub cwd: Option<String>,
    pub u_cwd: Option<String>,
}

#[derive(Serialize)]
pub struct CommandResponse {
    pub output: String,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub type Sessions = Arc<Mutex<HashMap<u32, TerminalSession>>>;
