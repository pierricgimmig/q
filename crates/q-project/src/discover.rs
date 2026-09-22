use std::fs;
use std::path::{Path, PathBuf};

use q_core::{normalize_repo_url, ProjectPolicy, QueueError, TaskKind};
use serde::Deserialize;

use crate::config::{first_matching_rule, load_config};
use crate::git::{find_git_root_by_walking, git_head, git_remote_origin, git_toplevel};

pub const CONFIG_FILE_NAME: &str = ".agentqueue.toml";

pub fn config_file_name() -> &'static str {
    CONFIG_FILE_NAME
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    Explicit,
    PathRule,
    ConfigFile,
    Git,
    GlobalMapping,
    Unassigned,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectContext {
    pub project: Option<String>,
    pub repo: Option<String>,
    pub git_root: Option<PathBuf>,
    pub capture_path: PathBuf,
    pub repo_relative_path: Option<String>,
    pub git_head: Option<String>,
    pub source: ContextSource,
    pub agent_pool: Option<String>,
    pub default_kind: Option<TaskKind>,
    pub policy: Option<ProjectPolicy>,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    pub directory: PathBuf,
    pub explicit_repo: Option<String>,
    pub explicit_project: Option<String>,
    pub global_map: Option<PathBuf>,
    pub use_default_map: bool,
}

impl DiscoverOptions {
    pub fn from_directory(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            explicit_repo: None,
            explicit_project: None,
            global_map: None,
            use_default_map: false,
        }
    }
}

pub fn discover(options: DiscoverOptions) -> Result<ProjectContext, QueueError> {
    let capture_path = absolutize(&options.directory);
    let directory = if capture_path.is_file() {
        capture_path
            .parent()
            .unwrap_or(capture_path.as_path())
            .to_path_buf()
    } else {
        capture_path.clone()
    };

    let git_root = git_toplevel(&directory).or_else(|| find_git_root_by_walking(&directory));
    let (git_head, remote) = if git_root.is_some() {
        (git_head(&directory), git_remote_origin(&directory))
    } else {
        (None, None)
    };

    let mut project = None;
    let mut repo = remote
        .as_deref()
        .map(normalize_repo_url)
        .filter(|value| !value.is_empty());
    let mut agent_pool = None;
    let mut default_kind = None;
    let mut policy = None;
    let mut config_path = None;
    let mut source = if git_root.is_some() {
        ContextSource::Git
    } else {
        ContextSource::Unassigned
    };

    if project.is_none() {
        if let Some(name) = repo.as_ref().and_then(|value| value.rsplit('/').next()) {
            if !name.is_empty() && name != repo.as_deref().unwrap_or("") {
                project = Some(name.to_string());
            }
        }
    }
    if project.is_none() {
        if let Some(root) = &git_root {
            if let Some(name) = root.file_name() {
                project = Some(name.to_string_lossy().into_owned());
            }
        }
    }

    if let Some(root) = &git_root {
        let path = root.join(CONFIG_FILE_NAME);
        if path.is_file() {
            let config = load_config(&path)?;
            config_path = Some(path);
            if config.project.is_some() {
                project = config.project.clone();
            }
            if config.repo.is_some() {
                repo = config.repo.as_deref().map(normalize_repo_url);
            }
            default_kind = config.default_kind;
            agent_pool = config.default_agent_pool.clone();
            policy = Some(ProjectPolicy {
                max_parallel_jobs: config.max_parallel_jobs,
                require_pr: config.require_pr,
                allow_external_actions: config.allow_external_actions,
                stale_disposition: config.stale_disposition,
                config_path: config_path.as_ref().map(|path| path.display().to_string()),
            });
            source = ContextSource::ConfigFile;

            if let Some(relative) = repo_relative(root, &capture_path) {
                if let Some(rule) = first_matching_rule(&config.paths, &relative) {
                    if rule.project.is_some() {
                        project = rule.project.clone();
                    }
                    if rule.agent_pool.is_some() {
                        agent_pool = rule.agent_pool.clone();
                    }
                    source = ContextSource::PathRule;
                }
            }
        }
    }

    if git_root.is_none() {
        if let Some(mapping) = load_mapping(&directory, &options)? {
            if mapping.project.is_some() {
                project = mapping.project;
            }
            if mapping.repo.is_some() {
                repo = mapping.repo.as_deref().map(normalize_repo_url);
            }
            source = ContextSource::GlobalMapping;
        }
    }

    if let Some(explicit) = options.explicit_repo.as_deref() {
        let normalized = normalize_repo_url(explicit);
        if !normalized.is_empty() {
            repo = Some(normalized);
        }
        source = ContextSource::Explicit;
    }
    if let Some(explicit) = options.explicit_project.as_deref() {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            project = Some(trimmed.to_string());
        }
        source = ContextSource::Explicit;
    }

    let repo_relative_path = git_root
        .as_ref()
        .and_then(|root| repo_relative(root, &capture_path));

    Ok(ProjectContext {
        project,
        repo,
        git_root,
        capture_path,
        repo_relative_path,
        git_head,
        source,
        agent_pool,
        default_kind,
        policy,
        config_path,
    })
}

#[derive(Debug, Deserialize)]
struct MapFile {
    #[serde(default)]
    mappings: Vec<MapEntry>,
}

#[derive(Debug, Deserialize)]
struct MapEntry {
    prefix: String,
    project: Option<String>,
    repo: Option<String>,
}

struct AppliedMap {
    project: Option<String>,
    repo: Option<String>,
}

fn load_mapping(
    directory: &Path,
    options: &DiscoverOptions,
) -> Result<Option<AppliedMap>, QueueError> {
    let path = if let Some(path) = &options.global_map {
        path.clone()
    } else if options.use_default_map {
        match default_map_path() {
            Some(path) => path,
            None => return Ok(None),
        }
    } else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|err| {
        QueueError::InvalidInput(format!("failed to read mapping {}: {err}", path.display()))
    })?;
    let file: MapFile = toml::from_str(&text).map_err(|err| {
        QueueError::InvalidInput(format!("invalid mapping {}: {err}", path.display()))
    })?;
    let mut best: Option<(usize, &MapEntry)> = None;
    for entry in &file.mappings {
        let prefix = absolutize(Path::new(&entry.prefix));
        if directory.starts_with(&prefix) || directory == prefix {
            let len = prefix.components().count();
            let replace = match best {
                Some((best_len, _)) => len > best_len,
                None => true,
            };
            if replace {
                best = Some((len, entry));
            }
        }
    }
    Ok(best.map(|(_, entry)| AppliedMap {
        project: entry.project.clone(),
        repo: entry.repo.clone(),
    }))
}

fn default_map_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("q").join("path-map.toml"));
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("q")
            .join("path-map.toml"),
    )
}

fn absolutize(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn repo_relative(root: &Path, capture: &Path) -> Option<String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let capture = fs::canonicalize(capture).unwrap_or_else(|_| capture.to_path_buf());
    let relative = capture.strip_prefix(&root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let text = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("q-project-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "q-test@example.com"]);
        git(dir, &["config", "user.name", "q-test"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "init"]);
        git(
            dir,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:acme/profiler-core.git",
            ],
        );
    }

    #[test]
    fn nested_directory_discovers_repo_and_relative_path() {
        let root = temp_dir("nested");
        init_repo(&root);
        let nested = root.join("crates").join("trace");
        fs::create_dir_all(&nested).unwrap();
        let ctx = discover(DiscoverOptions::from_directory(&nested)).unwrap();
        assert_eq!(ctx.repo.as_deref(), Some("github.com/acme/profiler-core"));
        assert_eq!(ctx.project.as_deref(), Some("profiler-core"));
        assert_eq!(ctx.repo_relative_path.as_deref(), Some("crates/trace"));
        assert!(ctx.git_head.as_ref().unwrap().len() >= 7);
        assert_eq!(ctx.source, ContextSource::Git);
        assert!(ctx.capture_path.ends_with("crates/trace"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn linked_worktree_uses_worktree_root_and_shared_remote() {
        let root = temp_dir("main");
        init_repo(&root);
        let link_parent = temp_dir("linkparent");
        let linked = link_parent.join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "agent-branch",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let nested = linked.join("crates").join("trace");
        fs::create_dir_all(&nested).unwrap();
        let ctx = discover(DiscoverOptions::from_directory(&nested)).unwrap();
        let linked_canon = fs::canonicalize(&linked).unwrap();
        assert_eq!(ctx.git_root.as_ref(), Some(&linked_canon));
        assert_eq!(ctx.repo.as_deref(), Some("github.com/acme/profiler-core"));
        assert_eq!(ctx.repo_relative_path.as_deref(), Some("crates/trace"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&link_parent);
    }

    #[test]
    fn config_and_path_rule_override_inferred_names_unless_explicit() {
        let root = temp_dir("cfg");
        init_repo(&root);
        fs::write(
            root.join(".agentqueue.toml"),
            include_str!("../../../tests/fixtures/agentqueue.toml"),
        )
        .unwrap();
        let collector = root.join("crates").join("collector").join("src");
        fs::create_dir_all(&collector).unwrap();
        let ctx = discover(DiscoverOptions::from_directory(&collector)).unwrap();
        assert_eq!(ctx.project.as_deref(), Some("collector"));
        assert_eq!(ctx.agent_pool.as_deref(), Some("rust"));
        assert_eq!(ctx.repo.as_deref(), Some("github.com/acme/profiler-core"));
        assert_eq!(ctx.source, ContextSource::PathRule);
        assert_eq!(ctx.default_kind, Some(TaskKind::Implementation));
        assert!(ctx.policy.as_ref().unwrap().require_pr);
        assert!(!ctx.policy.as_ref().unwrap().allow_external_actions);

        let mut options = DiscoverOptions::from_directory(&collector);
        options.explicit_project = Some("override".into());
        let ctx = discover(options).unwrap();
        assert_eq!(ctx.project.as_deref(), Some("override"));
        assert_eq!(ctx.source, ContextSource::Explicit);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_finds_dot_git_when_git_metadata_is_not_a_repository() {
        let root = temp_dir("walk");
        fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let found = find_git_root_by_walking(&nested).unwrap();
        assert_eq!(found, root);
        let ctx = discover(DiscoverOptions::from_directory(&nested)).unwrap();
        assert_eq!(
            ctx.git_root
                .as_ref()
                .map(|path| fs::canonicalize(path).unwrap()),
            Some(fs::canonicalize(&root).unwrap())
        );
        assert!(ctx.repo.is_none());
        assert_eq!(ctx.repo_relative_path.as_deref(), Some("a/b"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_git_can_use_a_global_prefix_map_or_stay_unassigned() {
        let root = temp_dir("unmap");
        let ctx = discover(DiscoverOptions::from_directory(&root)).unwrap();
        assert!(ctx.git_root.is_none());
        assert_eq!(ctx.source, ContextSource::Unassigned);
        assert!(ctx.project.is_none());

        let map = root.join("map.toml");
        let child = root.join("src").join("tool");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            &map,
            format!(
                "[[mappings]]\nprefix = {}\nproject = \"tool\"\nrepo = \"github.com/acme/tool.git\"\n",
                toml_quote(&root.to_string_lossy())
            ),
        )
        .unwrap();
        let mut options = DiscoverOptions::from_directory(&child);
        options.global_map = Some(map);
        let ctx = discover(options).unwrap();
        assert_eq!(ctx.source, ContextSource::GlobalMapping);
        assert_eq!(ctx.project.as_deref(), Some("tool"));
        assert_eq!(ctx.repo.as_deref(), Some("github.com/acme/tool"));
        let _ = fs::remove_dir_all(&root);
    }

    fn toml_quote(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
