//! `POST /v1/<method>`: one JSON body per [`QueueService`] method.

use q_core::{
    Actor, ActorKind, BlockRequest, CancelRequest, CaptureRequest, ClaimRequest, CompleteRequest,
    CreateFeatureRequest, DeleteRequest, HeartbeatRequest, ListFilter, QueueError, QueueService,
    ReadyRequest, RecoverRequest, ReleaseRequest, StartRequest, TreeQuery,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::auth::{Principal, Role};
use crate::wire::{
    EditBody, EditFeatureBody, EmptyBody, ErrorBody, IdBody, ReopenBody, HUMAN_ONLY_METHODS,
};

/// Run one method against the queue. Shared by the HTTP handler and tests.
pub fn dispatch(
    queue: &dyn QueueService,
    method: &str,
    body: &[u8],
    principal: &Principal,
) -> Result<Value, ErrorBody> {
    if principal.role == Role::Agent && HUMAN_ONLY_METHODS.contains(&method) {
        return Err(ErrorBody::new(
            "forbidden",
            format!("{method} requires a human token"),
        ));
    }
    match method {
        "capture" => {
            let mut request: CaptureRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.capture(request))
        }
        "list" => {
            let filter: ListFilter = parse(body)?;
            reply(queue.list(filter))
        }
        "get" => {
            let IdBody { id } = parse(body)?;
            reply(queue.get(id))
        }
        "edit" => {
            let EditBody { id, mut request } = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.edit(id, request))
        }
        "mark_ready" => {
            let mut request: ReadyRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.mark_ready(request))
        }
        "block" => {
            let mut request: BlockRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.block(request))
        }
        "cancel" => {
            let mut request: CancelRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.cancel(request))
        }
        "delete" => {
            let mut request: DeleteRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.delete(request))
        }
        "claim_next" => {
            let request: ClaimRequest = parse(body)?;
            reply(queue.claim_next(request))
        }
        "heartbeat" => {
            let mut request: HeartbeatRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.heartbeat(request))
        }
        "start" => {
            let mut request: StartRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.start(request))
        }
        "complete" => {
            let mut request: CompleteRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.complete(request))
        }
        "release" => {
            let mut request: ReleaseRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.release(request))
        }
        "recover_stale" => {
            let mut request: RecoverRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.recover_stale(request))
        }
        "events" => {
            let IdBody { id } = parse(body)?;
            reply(queue.events(id))
        }
        "status" => {
            let EmptyBody {} = parse(body)?;
            reply(queue.status())
        }
        "reopen" => {
            let ReopenBody { id, mut actor } = parse(body)?;
            stamp(&mut actor, principal);
            reply(queue.reopen(id, actor))
        }
        "create_feature" => {
            let request: CreateFeatureRequest = parse(body)?;
            reply(queue.create_feature(request))
        }
        "list_features" => {
            let EmptyBody {} = parse(body)?;
            reply(queue.list_features())
        }
        "get_feature" => {
            let IdBody { id } = parse(body)?;
            reply(queue.get_feature(id))
        }
        "edit_feature" => {
            let EditFeatureBody { id, request } = parse(body)?;
            reply(queue.edit_feature(id, request))
        }
        "delete_feature" => {
            let IdBody { id } = parse(body)?;
            reply(queue.delete_feature(id))
        }
        "tree" => {
            let query: TreeQuery = parse(body)?;
            reply(queue.tree(query))
        }
        other => Err(ErrorBody::new(
            "not_found",
            format!("unknown method {other}"),
        )),
    }
}

/// Agent tokens cannot act as humans. The actor id stays if the caller set
/// one (an agent pool may share a token), otherwise it is the token name.
pub fn stamp(actor: &mut Actor, principal: &Principal) {
    if principal.role == Role::Agent {
        actor.kind = ActorKind::Agent;
        if actor.id.as_deref().is_none_or(str::is_empty) {
            actor.id = Some(principal.name.clone());
        }
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ErrorBody> {
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body)
        .map_err(|err| ErrorBody::new("invalid_input", format!("invalid request body: {err}")))
}

fn reply<T: Serialize>(result: Result<T, QueueError>) -> Result<Value, ErrorBody> {
    match result {
        Ok(value) => serde_json::to_value(value)
            .map_err(|err| ErrorBody::new("database", format!("cannot encode reply: {err}"))),
        Err(error) => Err(ErrorBody::from(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_stamped_and_cannot_mark_ready() {
        let agent = Principal {
            name: "bot".into(),
            role: Role::Agent,
        };
        let mut actor = Actor::human(Some("pierric".into()));
        stamp(&mut actor, &agent);
        assert_eq!(actor.kind, ActorKind::Agent);
        assert_eq!(actor.id.as_deref(), Some("pierric"));
        let mut blank = Actor::human(None);
        stamp(&mut blank, &agent);
        assert_eq!(blank.id.as_deref(), Some("bot"));

        struct Never;
        impl QueueService for Never {
            fn capture(&self, _: CaptureRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn list(&self, _: ListFilter) -> Result<Vec<q_core::TaskSummary>, QueueError> {
                unreachable!()
            }
            fn get(&self, _: i64) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn edit(&self, _: i64, _: q_core::EditRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn mark_ready(&self, _: ReadyRequest) -> Result<q_core::ReadyOutcome, QueueError> {
                unreachable!()
            }
            fn block(&self, _: BlockRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn cancel(&self, _: CancelRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn delete(&self, _: DeleteRequest) -> Result<q_core::DeleteOutcome, QueueError> {
                unreachable!()
            }
            fn claim_next(&self, _: ClaimRequest) -> Result<q_core::ClaimOutcome, QueueError> {
                unreachable!()
            }
            fn heartbeat(&self, _: HeartbeatRequest) -> Result<q_core::Claim, QueueError> {
                unreachable!()
            }
            fn start(&self, _: StartRequest) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn complete(&self, _: CompleteRequest) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn release(&self, _: ReleaseRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn recover_stale(
                &self,
                _: RecoverRequest,
            ) -> Result<Vec<q_core::RecoveryRecord>, QueueError> {
                unreachable!()
            }
            fn events(&self, _: i64) -> Result<Vec<q_core::Event>, QueueError> {
                unreachable!()
            }
            fn status(&self) -> Result<q_core::QueueStatus, QueueError> {
                unreachable!()
            }
            fn reopen(&self, _: i64, _: Actor) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn create_feature(
                &self,
                _: CreateFeatureRequest,
            ) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn list_features(&self) -> Result<Vec<q_core::Feature>, QueueError> {
                unreachable!()
            }
            fn get_feature(&self, _: i64) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn edit_feature(
                &self,
                _: i64,
                _: q_core::EditFeatureRequest,
            ) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn delete_feature(&self, _: i64) -> Result<q_core::DeleteFeatureOutcome, QueueError> {
                unreachable!()
            }
            fn tree(&self, _: TreeQuery) -> Result<q_core::TaskTree, QueueError> {
                unreachable!()
            }
        }
        let body = serde_json::to_vec(&ReadyRequest {
            task_id: 1,
            actor: Actor::human(None),
        })
        .unwrap();
        let denied = dispatch(&Never, "mark_ready", &body, &agent).unwrap_err();
        assert_eq!(denied.code, "forbidden");
        let reopen = serde_json::to_vec(&ReopenBody {
            id: 1,
            actor: Actor::human(None),
        })
        .unwrap();
        assert_eq!(
            dispatch(&Never, "reopen", &reopen, &agent)
                .unwrap_err()
                .code,
            "forbidden"
        );
        assert_eq!(
            dispatch(&Never, "nope", b"{}", &agent).unwrap_err().code,
            "not_found"
        );
        assert_eq!(
            dispatch(&Never, "get", b"not json", &agent)
                .unwrap_err()
                .code,
            "invalid_input"
        );
    }
}
