//! Deterministic eligibility checks used inside the claim transaction.
//!
//! The queue never asks an LLM which task to run. Higher priority wins, then
//! older creation time, then smaller id — that ordering is applied by the
//! store before the first eligible row is chosen.

use q_core::{normalize_repo_url, ClaimRequest, RiskLevel, TaskKind, TaskStatus};

#[derive(Debug, Clone)]
pub struct EligibilityTask {
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub risk: RiskLevel,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub agent_pool: Option<String>,
    pub required_capabilities: Vec<String>,
    pub dependency_statuses: Vec<TaskStatus>,
}

pub fn is_eligible(
    task: &EligibilityTask,
    request: &ClaimRequest,
    active_in_project: i64,
    max_parallel_jobs: Option<i64>,
    allow_external_actions: bool,
) -> bool {
    if task.status != TaskStatus::Ready {
        return false;
    }
    if !request.allowed_repos.is_empty() {
        let repo = match task.repo.as_deref() {
            Some(repo) => normalize_repo_url(repo),
            None => return false,
        };
        let allowed = request
            .allowed_repos
            .iter()
            .any(|candidate| normalize_repo_url(candidate) == repo);
        if !allowed {
            return false;
        }
    }
    if !request.allowed_projects.is_empty() {
        match task.project.as_deref() {
            Some(project)
                if request
                    .allowed_projects
                    .iter()
                    .any(|candidate| candidate == project) => {}
            _ => return false,
        }
    }
    if !request.allowed_kinds.is_empty() && !request.allowed_kinds.contains(&task.kind) {
        return false;
    }
    if let Some(pool) = request.agent_pool.as_deref() {
        if let Some(task_pool) = task.agent_pool.as_deref() {
            if task_pool != pool {
                return false;
            }
        }
    }
    if task.risk > request.maximum_risk {
        return false;
    }
    if task.risk == RiskLevel::ExternalAction && !allow_external_actions {
        return false;
    }
    for required in &task.required_capabilities {
        if !request
            .capabilities
            .iter()
            .any(|capability| capability == required)
        {
            return false;
        }
    }
    if task
        .dependency_statuses
        .iter()
        .any(|status| *status != TaskStatus::Done)
    {
        return false;
    }
    if let Some(max) = max_parallel_jobs {
        if active_in_project >= max {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> EligibilityTask {
        EligibilityTask {
            status: TaskStatus::Ready,
            kind: TaskKind::Implementation,
            risk: RiskLevel::Low,
            project: Some("profiler-core".into()),
            repo: Some("github.com/acme/profiler-core".into()),
            agent_pool: None,
            required_capabilities: vec![],
            dependency_statuses: vec![],
        }
    }

    fn request() -> ClaimRequest {
        ClaimRequest::new("agent-1")
    }

    #[test]
    fn inbox_is_never_eligible() {
        let mut candidate = task();
        candidate.status = TaskStatus::Inbox;
        assert!(!is_eligible(&candidate, &request(), 0, None, false));
    }

    #[test]
    fn default_claim_denies_high_and_external_action() {
        let mut candidate = task();
        candidate.risk = RiskLevel::High;
        assert!(!is_eligible(&candidate, &request(), 0, None, false));

        candidate.risk = RiskLevel::ExternalAction;
        let mut explicit = request();
        explicit.maximum_risk = RiskLevel::ExternalAction;
        assert!(!is_eligible(&candidate, &explicit, 0, None, false));
        assert!(is_eligible(&candidate, &explicit, 0, None, true));

        candidate.risk = RiskLevel::High;
        explicit.maximum_risk = RiskLevel::High;
        assert!(is_eligible(&candidate, &explicit, 0, None, false));
    }

    #[test]
    fn capabilities_must_be_a_subset_and_dependencies_must_be_done() {
        let mut candidate = task();
        candidate.required_capabilities = vec!["rust".into(), "benchmarking".into()];
        assert!(!is_eligible(&candidate, &request(), 0, None, false));
        let mut worker = request();
        worker.capabilities = vec!["rust".into(), "benchmarking".into(), "github-pr".into()];
        assert!(is_eligible(&candidate, &worker, 0, None, false));

        candidate.dependency_statuses = vec![TaskStatus::Done, TaskStatus::Ready];
        assert!(!is_eligible(&candidate, &worker, 0, None, false));
        candidate.dependency_statuses = vec![TaskStatus::Done];
        assert!(is_eligible(&candidate, &worker, 0, None, false));
    }

    #[test]
    fn filters_and_project_cap() {
        let candidate = task();
        let mut req = request();
        req.allowed_repos = vec!["git@github.com:acme/profiler-core.git".into()];
        req.allowed_projects = vec!["profiler-core".into()];
        req.allowed_kinds = vec![TaskKind::Research];
        assert!(!is_eligible(&candidate, &req, 0, None, false));
        req.allowed_kinds = vec![TaskKind::Implementation];
        assert!(is_eligible(&candidate, &req, 0, Some(1), false));
        assert!(!is_eligible(&candidate, &req, 1, Some(1), false));

        let mut pooled = candidate.clone();
        pooled.agent_pool = Some("frontend".into());
        req.agent_pool = Some("rust".into());
        assert!(!is_eligible(&pooled, &req, 0, None, false));
        req.agent_pool = Some("frontend".into());
        assert!(is_eligible(&pooled, &req, 0, None, false));
    }

    #[test]
    fn empty_kind_and_repo_filters_mean_unrestricted() {
        let mut candidate = task();
        candidate.repo = None;
        candidate.project = None;
        assert!(is_eligible(&candidate, &request(), 0, None, false));
    }
}
