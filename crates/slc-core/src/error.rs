//! SLC error type.

use thiserror::Error;

pub type SlcResult<T> = Result<T, SlcError>;

#[derive(Debug, Error)]
pub enum SlcError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("document not found: {0}")]
    NotFound(String),
    #[error("LLM/embedding error: {0}")]
    Llm(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("limit reached: {0}")]
    Limit(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for SlcError {
    fn from(e: rusqlite::Error) -> Self {
        SlcError::Storage(e.to_string())
    }
}
