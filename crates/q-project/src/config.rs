use std::fs;
use std::path::Path;

use q_core::{QueueError, StaleDisposition, TaskKind};
use serde::Deserialize;

use crate::discover::ProjectContext;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    pub match_pattern: String,
    pub project: Option<String>,
    pub agent_pool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentQueueConfig {
    pub project: Option<String>,
    pub repo: Option<String>,
    pub default_kind: Option<TaskKind>,
    pub default_agent_pool: Option<String>,
    pub max_parallel_jobs: Option<i64>,
    pub require_pr: bool,
    pub allow_external_actions: bool,
    pub stale_disposition: StaleDisposition,
    pub paths: Vec<PathRule>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    project: Option<String>,
    repo: Option<String>,
    default_kind: Option<String>,
    default_agent_pool: Option<String>,
    max_parallel_jobs: Option<i64>,
    #[serde(default)]
    policy: Option<RawPolicy>,
    #[serde(default)]
    paths: Vec<RawPath>,
}

#[derive(Debug, Deserialize)]
struct RawPolicy {
    #[serde(default)]
    require_pr: bool,
    #[serde(default)]
    allow_external_actions: bool,
    stale_disposition: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPath {
    #[serde(rename = "match")]
    match_pattern: String,
    project: Option<String>,
    agent_pool: Option<String>,
}

pub fn load_config(path: &Path) -> Result<AgentQueueConfig, QueueError> {
    let text = fs::read_to_string(path).map_err(|err| {
        QueueError::InvalidInput(format!("failed to read {}: {err}", path.display()))
    })?;
    parse_config(&text)
        .map_err(|err| QueueError::InvalidInput(format!("invalid {}: {err}", path.display())))
}

pub fn parse_config(text: &str) -> Result<AgentQueueConfig, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|err| err.to_string())?;
    let default_kind = match raw.default_kind.as_deref() {
        Some(value) => Some(TaskKind::parse(value).map_err(|err| err.to_string())?),
        None => None,
    };
    if let Some(max) = raw.max_parallel_jobs {
        if max < 0 {
            return Err("max_parallel_jobs cannot be negative".into());
        }
    }
    let policy = raw.policy.unwrap_or(RawPolicy {
        require_pr: false,
        allow_external_actions: false,
        stale_disposition: None,
    });
    let stale_disposition = match policy.stale_disposition.as_deref() {
        Some(value) => StaleDisposition::parse(value).map_err(|err| err.to_string())?,
        None => StaleDisposition::Ready,
    };
    Ok(AgentQueueConfig {
        project: empty_to_none(raw.project),
        repo: empty_to_none(raw.repo),
        default_kind,
        default_agent_pool: empty_to_none(raw.default_agent_pool),
        max_parallel_jobs: raw.max_parallel_jobs,
        require_pr: policy.require_pr,
        allow_external_actions: policy.allow_external_actions,
        stale_disposition,
        paths: raw
            .paths
            .into_iter()
            .map(|rule| PathRule {
                match_pattern: rule.match_pattern,
                project: empty_to_none(rule.project),
                agent_pool: empty_to_none(rule.agent_pool),
            })
            .collect(),
    })
}

pub fn render_init_config(context: &ProjectContext) -> String {
    let project = context
        .project
        .clone()
        .or_else(|| {
            context.git_root.as_ref().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "project".into());
    let repo = context.repo.clone().unwrap_or_default();
    let kind = context
        .default_kind
        .unwrap_or(TaskKind::Implementation)
        .as_str();
    let mut text = String::from(
        "# q project configuration\n# Optional path rules override project and agent_pool.\n# [[paths]]\n# match = \"crates/collector/**\"\n# project = \"collector\"\n# agent_pool = \"rust\"\n\n",
    );
    text.push_str(&format!("project = {}\n", toml_string(&project)));
    if !repo.is_empty() {
        text.push_str(&format!("repo = {}\n", toml_string(&repo)));
    }
    text.push_str(&format!("default_kind = {}\n", toml_string(kind)));
    if let Some(pool) = &context.agent_pool {
        text.push_str(&format!("default_agent_pool = {}\n", toml_string(pool)));
    }
    text.push_str(
        "\n[policy]\nrequire_pr = false\nallow_external_actions = false\nstale_disposition = \"ready\"\n",
    );
    text
}

pub fn first_matching_rule<'a>(rules: &'a [PathRule], relative: &str) -> Option<&'a PathRule> {
    rules
        .iter()
        .find(|rule| glob_match(&rule.match_pattern, relative))
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    let pattern_segs: Vec<&str> = pattern.split('/').filter(|seg| !seg.is_empty()).collect();
    let path_segs: Vec<&str> = path.split('/').filter(|seg| !seg.is_empty()).collect();
    segment_glob(&pattern_segs, &path_segs)
}

fn segment_glob(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        if segment_glob(&pattern[1..], path) {
            return true;
        }
        if !path.is_empty() && segment_glob(pattern, &path[1..]) {
            return true;
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    segment_match(pattern[0], path[0]) && segment_glob(&pattern[1..], &path[1..])
}

fn segment_match(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();
    char_glob(&pattern, &segment)
}

fn char_glob(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    if pattern[0] == '*' {
        if char_glob(&pattern[1..], text) {
            return true;
        }
        if !text.is_empty() && char_glob(pattern, &text[1..]) {
            return true;
        }
        return false;
    }
    if text.is_empty() {
        return false;
    }
    pattern[0] == text[0] && char_glob(&pattern[1..], &text[1..])
}

fn empty_to_none(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../../../tests/fixtures/agentqueue.toml");

    #[test]
    fn parses_fixture_and_matches_path_rules() {
        let config = parse_config(SAMPLE).unwrap();
        assert_eq!(config.project.as_deref(), Some("profiler-core"));
        assert_eq!(
            config.repo.as_deref(),
            Some("github.com/acme/profiler-core")
        );
        assert_eq!(config.default_kind, Some(TaskKind::Implementation));
        assert_eq!(config.default_agent_pool.as_deref(), Some("coding"));
        assert_eq!(config.max_parallel_jobs, Some(2));
        assert!(config.require_pr);
        assert!(!config.allow_external_actions);
        assert_eq!(config.stale_disposition, StaleDisposition::Ready);

        let collector = first_matching_rule(&config.paths, "crates/collector/src/lib.rs").unwrap();
        assert_eq!(collector.project.as_deref(), Some("collector"));
        assert_eq!(collector.agent_pool.as_deref(), Some("rust"));
        assert!(glob_match("crates/collector/**", "crates/collector"));
        let viewer = first_matching_rule(&config.paths, "web/index.html").unwrap();
        assert_eq!(viewer.project.as_deref(), Some("viewer"));
        assert!(first_matching_rule(&config.paths, "docs/readme.md").is_none());
        assert!(glob_match("docs/*.md", "docs/readme.md"));
        assert!(!glob_match("docs/*.md", "docs/nested/readme.md"));
    }
}
