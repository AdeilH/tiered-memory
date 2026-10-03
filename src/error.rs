//! Error type for the whole engine.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error(
        "embedder mismatch: stored vectors come from `{db}` but this engine runs `{current}`. \
         Re-embed the store (POST /v1/reindex or MemoryEngine::reindex) or use the original embedder."
    )]
    EmbedderMismatch { db: String, current: String },

    #[error("user `{0}` not found")]
    UserNotFound(String),

    #[error("memory `{0}` not found")]
    MemoryNotFound(String),

    #[error("project `{0}` not found")]
    ProjectNotFound(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    #[error("embedder error: {0}")]
    Embedder(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl MemoryError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        MemoryError::Invalid(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, MemoryError>;
