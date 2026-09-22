use crate::model::TaskStatus;

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("task {0} not found")]
    NotFound(i64),
    #[error("invalid transition from {from} to {to}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    #[error("claim token does not match an active claim")]
    TokenMismatch,
    #[error("claim lease has expired")]
    ClaimExpired,
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Database(String),
}

impl QueueError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::InvalidTransition { .. } => "invalid_transition",
            Self::TokenMismatch => "token_mismatch",
            Self::ClaimExpired => "claim_expired",
            Self::InvalidInput(_) => "invalid_input",
            Self::Conflict(_) => "conflict",
            Self::Database(_) => "database",
        }
    }
}
