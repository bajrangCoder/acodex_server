use super::handlers::TerminalSession;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_SCROLLBACK_BYTES: usize = 262_144; // 256 KB

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

#[derive(Debug, Serialize)]
pub struct ProcessExitMessage {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub message: String,
}

pub type Sessions = Arc<DashMap<u32, TerminalSession>>;
