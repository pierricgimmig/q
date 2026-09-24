//! [`RemoteQueue`]: a [`QueueService`] that forwards every call to a `q serve`
//! authority over HTTP. The CLI and the MCP adapter use it unchanged in place
//! of the local SQLite queue.

use std::time::Duration;

use q_core::{
    Actor, BlockRequest, CancelRequest, CaptureRequest, Claim, ClaimOutcome, ClaimRequest,
    CompleteRequest, CreateFeatureRequest, DeleteFeatureOutcome, DeleteOutcome, DeleteRequest,
    EditFeatureRequest, EditRequest, Event, Feature, HeartbeatRequest, ListFilter, QueueError,
    QueueService, QueueStatus, ReadyOutcome, ReadyRequest, RecoverRequest, RecoveryRecord,
    ReleaseRequest, StartRequest, Task, TaskDetail, TaskSummary, TaskTree, TreeQuery,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::wire::{
    EditBody, EditFeatureBody, EmptyBody, ErrorEnvelope, HealthBody, IdBody, ReopenBody,
    API_PREFIX, HEALTH_PATH,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub struct RemoteQueue {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl RemoteQueue {
    /// `url` is the server origin, for example `http://127.0.0.1:7777` or
    /// `https://q.example.com`. A trailing slash is ignored.
    pub fn new(url: &str, token: Option<String>) -> Result<Self, QueueError> {
        let base = url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(QueueError::InvalidInput(format!(
                "server URL must start with http:// or https://: {url}"
            )));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .timeout_write(READ_TIMEOUT)
            .build();
        Ok(Self {
            base,
            token: token.filter(|token| !token.trim().is_empty()),
            agent,
        })
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    /// Unauthenticated liveness probe.
    pub fn health(&self) -> Result<HealthBody, QueueError> {
        let url = format!("{}{HEALTH_PATH}", self.base);
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|err| self.map_error(err))?;
        response
            .into_json()
            .map_err(|err| QueueError::Transport(format!("malformed health reply: {err}")))
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &impl Serialize,
    ) -> Result<T, QueueError> {
        let url = format!("{}{API_PREFIX}/{method}", self.base);
        let mut request = self.agent.post(&url);
        if let Some(token) = &self.token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = request.send_json(body).map_err(|err| self.map_error(err))?;
        response
            .into_json()
            .map_err(|err| QueueError::Transport(format!("malformed reply from {method}: {err}")))
    }

    fn map_error(&self, error: ureq::Error) -> QueueError {
        match error {
            ureq::Error::Status(code, response) => {
                let text = response.into_string().unwrap_or_default();
                match serde_json::from_str::<ErrorEnvelope>(&text) {
                    Ok(envelope) => QueueError::from(envelope.error),
                    Err(_) => QueueError::Transport(format!(
                        "{} returned HTTP {code}: {}",
                        self.base,
                        text.trim()
                    )),
                }
            }
            ureq::Error::Transport(transport) => QueueError::Transport(format!(
                "cannot reach q server at {}: {transport}",
                self.base
            )),
        }
    }
}

impl QueueService for RemoteQueue {
    fn capture(&self, request: CaptureRequest) -> Result<Task, QueueError> {
        self.call("capture", &request)
    }

    fn list(&self, filter: ListFilter) -> Result<Vec<TaskSummary>, QueueError> {
        self.call("list", &filter)
    }

    fn get(&self, id: i64) -> Result<TaskDetail, QueueError> {
        self.call("get", &IdBody { id })
    }

    fn edit(&self, id: i64, request: EditRequest) -> Result<Task, QueueError> {
        self.call("edit", &EditBody { id, request })
    }

    fn mark_ready(&self, request: ReadyRequest) -> Result<ReadyOutcome, QueueError> {
        self.call("mark_ready", &request)
    }

    fn block(&self, request: BlockRequest) -> Result<Task, QueueError> {
        self.call("block", &request)
    }

    fn cancel(&self, request: CancelRequest) -> Result<Task, QueueError> {
        self.call("cancel", &request)
    }

    fn delete(&self, request: DeleteRequest) -> Result<DeleteOutcome, QueueError> {
        self.call("delete", &request)
    }

    fn claim_next(&self, request: ClaimRequest) -> Result<ClaimOutcome, QueueError> {
        self.call("claim_next", &request)
    }

    fn heartbeat(&self, request: HeartbeatRequest) -> Result<Claim, QueueError> {
        self.call("heartbeat", &request)
    }

    fn start(&self, request: StartRequest) -> Result<TaskDetail, QueueError> {
        self.call("start", &request)
    }

    fn complete(&self, request: CompleteRequest) -> Result<TaskDetail, QueueError> {
        self.call("complete", &request)
    }

    fn release(&self, request: ReleaseRequest) -> Result<Task, QueueError> {
        self.call("release", &request)
    }

    fn recover_stale(&self, request: RecoverRequest) -> Result<Vec<RecoveryRecord>, QueueError> {
        self.call("recover_stale", &request)
    }

    fn events(&self, task_id: i64) -> Result<Vec<Event>, QueueError> {
        self.call("events", &IdBody { id: task_id })
    }

    fn status(&self) -> Result<QueueStatus, QueueError> {
        self.call("status", &EmptyBody {})
    }

    fn reopen(&self, id: i64, actor: Actor) -> Result<Task, QueueError> {
        self.call("reopen", &ReopenBody { id, actor })
    }

    fn create_feature(&self, request: CreateFeatureRequest) -> Result<Feature, QueueError> {
        self.call("create_feature", &request)
    }

    fn list_features(&self) -> Result<Vec<Feature>, QueueError> {
        self.call("list_features", &EmptyBody {})
    }

    fn get_feature(&self, id: i64) -> Result<Feature, QueueError> {
        self.call("get_feature", &IdBody { id })
    }

    fn edit_feature(&self, id: i64, request: EditFeatureRequest) -> Result<Feature, QueueError> {
        self.call("edit_feature", &EditFeatureBody { id, request })
    }

    fn delete_feature(&self, id: i64) -> Result<DeleteFeatureOutcome, QueueError> {
        self.call("delete_feature", &IdBody { id })
    }

    fn tree(&self, query: TreeQuery) -> Result<TaskTree, QueueError> {
        self.call("tree", &query)
    }
}
