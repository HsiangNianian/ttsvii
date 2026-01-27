use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMessage {
    pub task_id: String,
    pub timestamp: String,
    pub level: String, // "info", "warn", "error"
    pub message: String,
}
