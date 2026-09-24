//! Wire format shared by the server and the client.
//!
//! Every [`q_core::QueueService`] method is one `POST /v1/<method>` with a
//! JSON body. A success is HTTP 200 with the method's result as JSON. A
//! failure is a 4xx or 5xx with an [`ErrorBody`] that round-trips to
//! [`QueueError`] on the client, so callers see the same error variants they
//! would see against a local database.

use q_core::{Actor, QueueError, TaskStatus};
use serde::{Deserialize, Serialize};

pub const API_PREFIX: &str = "/v1";
pub const HEALTH_PATH: &str = "/v1/health";

/// Method names. The path is `/v1/<name>`.
pub const METHODS: &[&str] = &[
    "capture",
    "list",
    "get",
    "edit",
    "mark_ready",
    "block",
    "cancel",
    "delete",
    "claim_next",
    "heartbeat",
    "start",
    "complete",
    "release",
    "recover_stale",
    "events",
    "status",
    "reopen",
    "create_feature",
    "list_features",
    "get_feature",
    "edit_feature",
    "delete_feature",
    "tree",
];

/// Methods that make work claimable. Only human tokens may call them.
pub const HUMAN_ONLY_METHODS: &[&str] = &["mark_ready", "reopen"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthBody {
    pub ok: bool,
    pub version: String,
}

/// Body for `/v1/get`, `/v1/events`, `/v1/get_feature`, `/v1/delete_feature`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdBody {
    pub id: i64,
}

/// Body for `/v1/edit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditBody {
    pub id: i64,
    pub request: q_core::EditRequest,
}

/// Body for `/v1/edit_feature`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFeatureBody {
    pub id: i64,
    pub request: q_core::EditFeatureRequest,
}

/// Body for `/v1/reopen`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReopenBody {
    pub id: i64,
    pub actor: Actor,
}

/// Body for `/v1/status`, `/v1/list_features`: no arguments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyBody {}

/// Error payload. `code` is [`QueueError::code`] plus the transport codes
/// `unauthorized`, `forbidden`, and `transport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<TaskStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

impl ErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            task_id: None,
            feature: None,
            from: None,
            to: None,
        }
    }

    /// HTTP status for this error.
    pub fn http_status(&self) -> u16 {
        match self.code.as_str() {
            "not_found" => 404,
            "invalid_transition" | "conflict" | "claim_expired" | "token_mismatch" => 409,
            "invalid_input" => 400,
            "unauthorized" => 401,
            "forbidden" => 403,
            _ => 500,
        }
    }
}

impl From<&QueueError> for ErrorBody {
    fn from(error: &QueueError) -> Self {
        let mut body = ErrorBody::new(error.code(), error.to_string());
        match error {
            QueueError::NotFound(id) => body.task_id = Some(*id),
            QueueError::FeatureNotFound(name) => body.feature = Some(name.clone()),
            QueueError::InvalidTransition { from, to } => {
                body.from = Some(*from);
                body.to = Some(*to);
            }
            QueueError::Database(inner) | QueueError::Transport(inner) => {
                body.message = inner.clone();
            }
            _ => {}
        }
        body
    }
}

impl From<ErrorBody> for QueueError {
    fn from(body: ErrorBody) -> Self {
        match (
            body.code.as_str(),
            body.task_id,
            body.feature,
            body.from,
            body.to,
        ) {
            ("not_found", Some(id), _, _, _) => QueueError::NotFound(id),
            ("not_found", None, Some(feature), _, _) => QueueError::FeatureNotFound(feature),
            ("not_found", None, None, _, _) => QueueError::InvalidInput(body.message),
            ("invalid_transition", _, _, Some(from), Some(to)) => {
                QueueError::InvalidTransition { from, to }
            }
            ("token_mismatch", ..) => QueueError::TokenMismatch,
            ("claim_expired", ..) => QueueError::ClaimExpired,
            ("invalid_input", ..) => QueueError::InvalidInput(body.message),
            ("conflict", ..) => QueueError::Conflict(body.message),
            ("unauthorized", ..) | ("forbidden", ..) => {
                QueueError::Transport(format!("{}: {}", body.code, body.message))
            }
            ("transport", ..) => QueueError::Transport(body.message),
            _ => QueueError::Database(body.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_errors_round_trip() {
        let cases = vec![
            QueueError::NotFound(7),
            QueueError::FeatureNotFound("auth".into()),
            QueueError::InvalidTransition {
                from: TaskStatus::Inbox,
                to: TaskStatus::Done,
            },
            QueueError::TokenMismatch,
            QueueError::ClaimExpired,
            QueueError::InvalidInput("bad".into()),
            QueueError::Conflict("busy".into()),
            QueueError::Database("boom".into()),
            QueueError::Transport("down".into()),
        ];
        for error in cases {
            let body = ErrorBody::from(&error);
            let json = serde_json::to_string(&ErrorEnvelope { error: body }).unwrap();
            let back: ErrorEnvelope = serde_json::from_str(&json).unwrap();
            let restored = QueueError::from(back.error);
            assert_eq!(restored.code(), error.code(), "{error}");
            assert_eq!(restored.to_string(), error.to_string());
        }
    }
}
