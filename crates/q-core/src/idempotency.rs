//! Idempotency keys for task capture.
//!
//! An explicit key names one intent across retries and machines. When the
//! caller omits it, q derives `content:<sha256>` from the title, body, kind,
//! repo, and project. The same derived or explicit key is one queue item.

use sha2::{Digest, Sha256};

use crate::QueueError;

const CONTENT_PREFIX: &str = "content:";
const MAX_EXPLICIT_LEN: usize = 256;

/// Resolve the key stored on a new task.
///
/// Explicit keys are trimmed and stored as given. Derived keys use a stable
/// `q-idempotency-v1` canonical form so two machines hash the same intent to
/// the same key.
pub fn resolve_idempotency_key(
    explicit: Option<&str>,
    title: &str,
    body: Option<&str>,
    kind: &str,
    repo: Option<&str>,
    project: Option<&str>,
) -> Result<String, QueueError> {
    if let Some(explicit) = explicit {
        let key = explicit.trim();
        if key.is_empty() {
            return Err(QueueError::InvalidInput(
                "idempotency key must not be empty".into(),
            ));
        }
        if key.len() > MAX_EXPLICIT_LEN {
            return Err(QueueError::InvalidInput(
                "idempotency key must be at most 256 bytes".into(),
            ));
        }
        if key.chars().any(|ch| ch == '\n' || ch == '\r' || ch == '\0') {
            return Err(QueueError::InvalidInput(
                "idempotency key must be a single line".into(),
            ));
        }
        return Ok(key.to_string());
    }
    let canonical = format!(
        "q-idempotency-v1\n{}\n{}\n{}\n{}\n{}\n",
        collapse(title),
        collapse(body.unwrap_or("")),
        kind.trim(),
        collapse(repo.unwrap_or("")),
        collapse(project.unwrap_or("")),
    );
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(format!("{CONTENT_PREFIX}{digest:x}"))
}

fn collapse(text: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_key_wins_over_content() {
        let key = resolve_idempotency_key(
            Some("  ticket-9  "),
            "Fix the bug",
            None,
            "implementation",
            None,
            None,
        )
        .unwrap();
        assert_eq!(key, "ticket-9");
    }

    #[test]
    fn derived_key_ignores_whitespace_and_matches_across_calls() {
        let left = resolve_idempotency_key(
            None,
            "Fix   the bug",
            Some("body\n"),
            "implementation",
            Some("github.com/acme/demo"),
            Some("demo"),
        )
        .unwrap();
        let right = resolve_idempotency_key(
            None,
            " Fix the bug ",
            Some("body"),
            "implementation",
            Some("github.com/acme/demo"),
            Some("demo"),
        )
        .unwrap();
        assert_eq!(left, right);
        assert!(left.starts_with("content:"));
        assert_ne!(
            left,
            resolve_idempotency_key(
                None,
                "Fix the other bug",
                Some("body"),
                "implementation",
                Some("github.com/acme/demo"),
                Some("demo"),
            )
            .unwrap()
        );
    }
}
