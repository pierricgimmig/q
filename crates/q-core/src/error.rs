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
    Insufficient(String),
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
            Self::Insufficient(_) => "insufficient_specification",
            Self::InvalidInput(_) => "invalid_input",
            Self::Conflict(_) => "conflict",
            Self::Database(_) => "database",
        }
    }

    pub fn insufficient(missing: &[String]) -> Self {
        let mut lines = vec!["task is not sufficiently specified".to_string()];
        for item in missing {
            lines.push(format!("warning: missing recommended section: {item}"));
        }
        lines.push("re-run with --force to mark ready".to_string());
        Self::Insufficient(lines.join("\n"))
    }
}
