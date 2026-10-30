//! JSON wire format shared by `lafd` and `lafdctl`.
//!
//! The socket speaks newline-delimited JSON: one request per line, one
//! response per line.

use serde::{Deserialize, Serialize};

/// One command sent to the daemon.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Request {
    pub cmd: String,
    /// Trigger name, or the player query for player commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Trigger timeout override in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// The daemon's reply. Exactly one of `result` and `error` is set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(result: impl Serialize) -> Self {
        Self {
            ok: true,
            result: serde_json::to_value(result).ok(),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(message.into()),
        }
    }
}

/// Current focus owner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusInfo {
    /// MPRIS bus name for automatic focus, or an arbitrary label for a manual
    /// trigger.
    pub owner: String,
    /// `exclusive` or `transient`.
    pub mode: String,
    pub locked: bool,
    pub auto: bool,
    pub reason: String,
    /// Seconds since the focus was acquired.
    pub since_secs: u64,
}

/// One MPRIS player in a status response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerStatus {
    pub name: String,
    pub identity: String,
    pub status: String,
    pub is_focus: bool,
    pub paused_by_service: bool,
}

/// Result of the `status` command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub focus: Option<FocusInfo>,
    pub paused_by_service: Vec<String>,
    pub players: Vec<PlayerStatus>,
}
