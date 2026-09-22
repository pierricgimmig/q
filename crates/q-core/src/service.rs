use crate::model::{
    BlockRequest, CancelRequest, CaptureRequest, ClaimOutcome, ClaimRequest, CompleteRequest,
    DeleteOutcome, DeleteRequest, EditRequest, Event, HeartbeatRequest, ListFilter, QueueStatus,
    ReadyOutcome, ReadyRequest, RecoverRequest, RecoveryRecord, ReleaseRequest, StartRequest, Task,
    TaskDetail, TaskSummary,
};
use crate::QueueError;

/// The only mutation API for the queue.
///
/// Both the CLI and the MCP server call these methods. Adapters must not
/// issue their own SQL or implement a parallel state machine.
pub trait QueueService: Send + Sync {
    fn capture(&self, request: CaptureRequest) -> Result<Task, QueueError>;
    fn list(&self, filter: ListFilter) -> Result<Vec<TaskSummary>, QueueError>;
    fn get(&self, id: i64) -> Result<TaskDetail, QueueError>;
    fn edit(&self, id: i64, request: EditRequest) -> Result<Task, QueueError>;
    fn mark_ready(&self, request: ReadyRequest) -> Result<ReadyOutcome, QueueError>;
    fn block(&self, request: BlockRequest) -> Result<Task, QueueError>;
    fn cancel(&self, request: CancelRequest) -> Result<Task, QueueError>;
    /// Hard-delete a task and rows that reference it.
    ///
    /// Claims, events, artifacts, and dependency edges cascade with the task
    /// (or are deleted in the same transaction). The event log does not survive,
    /// so there is no retained `task_deleted` record. An unexpired claim is
    /// rejected unless `force` is set, in which case that claim is cleared too.
    fn delete(&self, request: DeleteRequest) -> Result<DeleteOutcome, QueueError>;
    fn claim_next(&self, request: ClaimRequest) -> Result<ClaimOutcome, QueueError>;
    fn heartbeat(&self, request: HeartbeatRequest) -> Result<crate::model::Claim, QueueError>;
    fn start(&self, request: StartRequest) -> Result<TaskDetail, QueueError>;
    fn complete(&self, request: CompleteRequest) -> Result<TaskDetail, QueueError>;
    fn release(&self, request: ReleaseRequest) -> Result<Task, QueueError>;
    fn recover_stale(&self, request: RecoverRequest) -> Result<Vec<RecoveryRecord>, QueueError>;
    fn events(&self, task_id: i64) -> Result<Vec<Event>, QueueError>;
    fn status(&self) -> Result<QueueStatus, QueueError>;
    fn reopen(&self, id: i64, actor: crate::model::Actor) -> Result<Task, QueueError>;
}
