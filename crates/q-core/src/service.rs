use crate::model::{
    ArtifactContent, BlockRequest, CancelRequest, CaptureRequest, ClaimOutcome, ClaimRequest,
    CompleteRequest, CreateFeatureRequest, DeleteFeatureOutcome, DeleteOutcome, DeleteRequest,
    EditFeatureRequest, EditRequest, Event, Feature, HeartbeatRequest, HoldRequest, ListFilter,
    LogRequest, QueueStatus, ReadyOutcome, ReadyRequest, RecoverRequest, RecoveryRecord,
    ReleaseRequest, StartRequest, Task, TaskDetail, TaskSummary, TaskTree, TreeQuery,
};
use crate::QueueError;

/// The only mutation API for the queue.
///
/// Both the CLI and the MCP server call these methods. Adapters must not
/// issue their own SQL or implement a parallel state machine.
pub trait QueueService: Send + Sync {
    fn capture(&self, request: CaptureRequest) -> Result<Task, QueueError>;
    /// List task summaries.
    ///
    /// `done` and `cancelled` are omitted when `filter.status` is unset and
    /// `filter.include_terminal` is false. An explicit status is returned as
    /// requested. Rows are ordered by feature title (blank last), then project
    /// (blank last), then `updated_at` descending.
    ///
    /// `filter.feature`, when set, is a feature id or a unique title.
    fn list(&self, filter: ListFilter) -> Result<Vec<TaskSummary>, QueueError>;
    fn get(&self, id: i64) -> Result<TaskDetail, QueueError>;
    fn edit(&self, id: i64, request: EditRequest) -> Result<Task, QueueError>;
    fn mark_ready(&self, request: ReadyRequest) -> Result<ReadyOutcome, QueueError>;
    /// Move a ready or blocked task back to `held` so agents cannot claim it
    /// until a human runs `mark_ready` again.
    fn hold(&self, request: HoldRequest) -> Result<Task, QueueError>;
    fn block(&self, request: BlockRequest) -> Result<Task, QueueError>;
    fn cancel(&self, request: CancelRequest) -> Result<Task, QueueError>;
    /// Hard-delete a task and rows that reference it.
    ///
    /// Claims, events, artifacts, and dependency edges cascade with the task
    /// (or are deleted in the same transaction). The event log does not survive,
    /// so delete does not take a reason and there is no retained `task_deleted`
    /// record. An unexpired claim is rejected unless `force` is set, in which
    /// case that claim is cleared too.
    fn delete(&self, request: DeleteRequest) -> Result<DeleteOutcome, QueueError>;
    fn claim_next(&self, request: ClaimRequest) -> Result<ClaimOutcome, QueueError>;
    /// Release the claim, increment the failure count, and return the task to ready.
    fn fail(&self, request: crate::model::FailRequest) -> Result<Task, QueueError>;
    /// Append a short status note to a claimed task.
    fn note(&self, request: crate::model::NoteRequest) -> Result<TaskDetail, QueueError>;
    fn heartbeat(&self, request: HeartbeatRequest) -> Result<crate::model::Claim, QueueError>;
    fn start(&self, request: StartRequest) -> Result<TaskDetail, QueueError>;
    fn complete(&self, request: CompleteRequest) -> Result<TaskDetail, QueueError>;
    fn release(&self, request: ReleaseRequest) -> Result<Task, QueueError>;
    fn recover_stale(&self, request: RecoverRequest) -> Result<Vec<RecoveryRecord>, QueueError>;
    /// Append a note and/or artifacts to a task's log. See [`LogRequest`].
    ///
    /// A note is a `task_note` event whose payload carries the message and
    /// the progress percent when given; progress is also stored on the task.
    /// Each artifact is stored (with its content when given) and recorded as
    /// an `artifact_added` event. Allowed in any status; never changes it.
    fn log(&self, request: LogRequest) -> Result<TaskDetail, QueueError>;
    /// Fetch one artifact with its stored content.
    fn artifact(&self, artifact_id: i64) -> Result<ArtifactContent, QueueError>;
    /// Every event for a task, oldest first. This is the task's log.
    fn events(&self, task_id: i64) -> Result<Vec<Event>, QueueError>;
    fn status(&self) -> Result<QueueStatus, QueueError>;
    fn reopen(&self, id: i64, actor: crate::model::Actor) -> Result<Task, QueueError>;

    /// Create a feature. A feature is a label that can group tasks across repos.
    fn create_feature(&self, request: CreateFeatureRequest) -> Result<Feature, QueueError>;
    fn list_features(&self) -> Result<Vec<Feature>, QueueError>;
    fn get_feature(&self, id: i64) -> Result<Feature, QueueError>;
    fn edit_feature(&self, id: i64, request: EditFeatureRequest) -> Result<Feature, QueueError>;
    /// Delete a feature. Tasks that referenced it keep their rows; `feature_id` is set to null.
    fn delete_feature(&self, id: i64) -> Result<DeleteFeatureOutcome, QueueError>;

    /// Dependency tree. Read-only.
    ///
    /// Children are tasks the parent depends on, so reading downward is the
    /// order to finish work. Pass a task id for one tree, or a feature id or
    /// unique title for a forest of that feature. Feature roots are tasks in
    /// the feature that no other task in the feature depends on. Dependencies
    /// outside the feature are included and marked external. A node expanded
    /// earlier is returned again with `already_shown` and no children. A cycle
    /// sets `cycle` and stops. An empty feature is an empty forest, not an error.
    fn tree(&self, query: TreeQuery) -> Result<TaskTree, QueueError>;
}
