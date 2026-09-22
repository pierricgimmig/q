//! Directory, Git, and `.agentqueue.toml` discovery.

mod config;
mod discover;
mod git;

pub use config::{load_config, render_init_config, AgentQueueConfig, PathRule};
pub use discover::{config_file_name, discover, DiscoverOptions, ProjectContext};
pub use git::find_git_root_by_walking;
pub use q_core::normalize_repo_url;

pub use discover::ContextSource;
