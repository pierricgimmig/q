use crate::model::TaskStatus;
use crate::QueueError;

pub fn transition_allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Held, Ready)
            | (Held, Blocked)
            | (Held, Cancelled)
            | (Ready, Held)
            | (Ready, Claimed)
            | (Ready, Blocked)
            | (Ready, Cancelled)
            | (Claimed, InProgress)
            | (Claimed, Ready)
            | (Claimed, Blocked)
            | (InProgress, Review)
            | (InProgress, Done)
            | (InProgress, Blocked)
            | (InProgress, Ready)
            | (Review, Done)
            | (Review, InProgress)
            | (Review, Blocked)
            | (Blocked, Held)
            | (Blocked, Ready)
            | (Blocked, Cancelled)
            | (Done, Ready)
            | (Cancelled, Held)
    )
}

pub fn ensure_transition(from: TaskStatus, to: TaskStatus) -> Result<(), QueueError> {
    if transition_allowed(from, to) {
        Ok(())
    } else {
        Err(QueueError::InvalidTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::*;

    const ALL: [TaskStatus; 8] = [
        Held, Ready, Claimed, InProgress, Review, Blocked, Done, Cancelled,
    ];

    #[test]
    fn legal_transitions_match_the_state_machine() {
        let legal = [
            (Held, Ready),
            (Held, Blocked),
            (Held, Cancelled),
            (Ready, Held),
            (Ready, Claimed),
            (Ready, Blocked),
            (Ready, Cancelled),
            (Claimed, InProgress),
            (Claimed, Ready),
            (Claimed, Blocked),
            (InProgress, Review),
            (InProgress, Done),
            (InProgress, Blocked),
            (InProgress, Ready),
            (Review, Done),
            (Review, InProgress),
            (Review, Blocked),
            (Blocked, Held),
            (Blocked, Ready),
            (Blocked, Cancelled),
            (Done, Ready),
            (Cancelled, Held),
        ];
        for (from, to) in legal {
            assert!(
                transition_allowed(from, to),
                "{from} → {to} should be legal"
            );
        }
    }

    #[test]
    fn forbidden_transitions_are_rejected() {
        let forbidden = [
            (Held, Claimed),
            (Held, InProgress),
            (Held, Done),
            (Held, Review),
            (Ready, Done),
            (Ready, InProgress),
            (Ready, Review),
            (Claimed, Done),
            (Claimed, Review),
            (Claimed, Cancelled),
            (Done, InProgress),
            (Done, Held),
            (Cancelled, Ready),
            (Cancelled, Done),
            (Review, Ready),
            (Review, Claimed),
        ];
        for (from, to) in forbidden {
            assert!(
                !transition_allowed(from, to),
                "{from} → {to} should be forbidden"
            );
            assert!(ensure_transition(from, to).is_err());
        }
    }

    #[test]
    fn self_transitions_are_forbidden() {
        for status in ALL {
            assert!(!transition_allowed(status, status));
        }
    }
}
