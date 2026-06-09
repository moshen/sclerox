use thiserror::Error;

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum OlError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("index error: {0}")]
    Index(String),

    #[error("embedding error: {0}")]
    Embedding(String),
}
