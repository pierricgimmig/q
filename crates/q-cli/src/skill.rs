//! Agent skill text and installer.
//!
//! `q skill` prints the embedded skill. `q skill install` writes that same
//! markdown to the user-level skill directories common coding agents scan.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

const SKILL_MARKDOWN: &str = include_str!("../skills/q/SKILL.md");

const INSTALL_HELP: &str = "\
## How to install

`q skill install` writes this skill to the user-level skill directories for common local coding agents:

- `~/.agents/skills/q/SKILL.md` — shared cross-agent location (Codex current user path; also used broadly)
- `~/.claude/skills/q/SKILL.md` — Claude Code personal skills
- `~/.cursor/skills/q/SKILL.md` — Cursor user skills
- `~/.codex/skills/q/SKILL.md` — Codex legacy path (still scanned)

Grok and similar agents that read Cursor skills or `~/.agents/skills` are covered by those two paths. There is no separate Grok directory.

You can also copy the skill printed above into `SKILL.md` yourself.

`--target agents|claude|cursor|codex|all` is repeatable and defaults to all four directories. Existing files are overwritten. A new file is reported as `created` and a replaced file as `updated`. `--force` is accepted and does not change that. Unwritable directories are skipped; the command fails only when every selected target was skipped.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SkillTarget {
    name: &'static str,
    relative: &'static str,
}

const TARGETS: &[SkillTarget] = &[
    SkillTarget {
        name: "agents",
        relative: ".agents/skills/q/SKILL.md",
    },
    SkillTarget {
        name: "claude",
        relative: ".claude/skills/q/SKILL.md",
    },
    SkillTarget {
        name: "cursor",
        relative: ".cursor/skills/q/SKILL.md",
    },
    SkillTarget {
        name: "codex",
        relative: ".codex/skills/q/SKILL.md",
    },
];

#[derive(Debug, Clone, Serialize)]
struct TargetPath {
    name: &'static str,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct SkillDocument {
    skill: &'static str,
    install_targets: Vec<TargetPath>,
    install_help: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallOutcome {
    pub target: String,
    pub status: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstallReport {
    /// Always true. `--force` is accepted and recorded; overwrite does not depend on it.
    overwrite: bool,
    force: bool,
    results: Vec<InstallOutcome>,
}

pub fn skill_markdown() -> &'static str {
    SKILL_MARKDOWN
}

pub fn install_help() -> &'static str {
    INSTALL_HELP
}

pub fn home_dir() -> Result<PathBuf, String> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| "HOME is not set".to_string())?;
    if raw.is_empty() {
        return Err("HOME is not set".into());
    }
    Ok(PathBuf::from(raw))
}

pub fn print_skill(json: bool) -> Result<(), String> {
    let home = home_dir().ok();
    let document = skill_document(home.as_deref());
    if json {
        write_json(&document)
    } else {
        let mut stdout = io::stdout().lock();
        write!(stdout, "{}", document.skill).map_err(|err| err.to_string())?;
        if !document.skill.ends_with('\n') {
            writeln!(stdout).map_err(|err| err.to_string())?;
        }
        writeln!(stdout).map_err(|err| err.to_string())?;
        write!(stdout, "{}", document.install_help).map_err(|err| err.to_string())?;
        Ok(())
    }
}

pub fn install_skill(
    home: &Path,
    requested: &[String],
    force: bool,
    json: bool,
) -> Result<(), String> {
    let results = write_skill(home, requested)?;
    let succeeded = results.iter().any(|result| result.status != "skipped");
    let report = InstallReport {
        overwrite: true,
        force,
        results,
    };
    if json {
        write_json(&report)?;
    } else {
        let mut stdout = io::stdout().lock();
        for result in &report.results {
            match result.message.as_deref() {
                Some(message) => writeln!(
                    stdout,
                    "{} {} ({message})",
                    result.status,
                    result.path.display()
                ),
                None => writeln!(stdout, "{} {}", result.status, result.path.display()),
            }
            .map_err(|err| err.to_string())?;
        }
    }
    if !succeeded {
        return Err("no skill files installed".into());
    }
    if force {
        tracing::debug!("skill install --force; existing files are overwritten either way");
    }
    Ok(())
}

pub fn write_skill(home: &Path, requested: &[String]) -> Result<Vec<InstallOutcome>, String> {
    let targets = select_targets(requested)?;
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let path = home.join(target.relative);
        results.push(write_one(target.name, &path));
    }
    Ok(results)
}

fn skill_document(home: Option<&Path>) -> SkillDocument {
    let install_targets = TARGETS
        .iter()
        .map(|target| TargetPath {
            name: target.name,
            path: match home {
                Some(home) => home.join(target.relative),
                None => PathBuf::from("~").join(target.relative),
            },
        })
        .collect();
    SkillDocument {
        skill: skill_markdown(),
        install_targets,
        install_help: install_help(),
    }
}

fn select_targets(requested: &[String]) -> Result<Vec<SkillTarget>, String> {
    if requested.is_empty() || requested.iter().any(|name| name == "all") {
        return Ok(TARGETS.to_vec());
    }
    let mut chosen = Vec::new();
    for name in requested {
        let Some(target) = TARGETS.iter().find(|target| target.name == name) else {
            return Err(format!(
                "unknown skill target '{name}' (expected agents, claude, cursor, codex, or all)"
            ));
        };
        if !chosen
            .iter()
            .any(|existing: &SkillTarget| existing.name == target.name)
        {
            chosen.push(*target);
        }
    }
    Ok(chosen)
}

fn write_one(name: &str, path: &Path) -> InstallOutcome {
    let existed = path.exists();
    let outcome = (|| {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, skill_markdown())?;
        Ok::<(), io::Error>(())
    })();
    match outcome {
        Ok(()) => InstallOutcome {
            target: name.to_string(),
            status: if existed {
                "updated".into()
            } else {
                "created".into()
            },
            path: path.to_path_buf(),
            message: None,
        },
        Err(err) => InstallOutcome {
            target: name.to_string(),
            status: "skipped".into(),
            path: path.to_path_buf(),
            message: Some(err.to_string()),
        },
    }
}

fn write_json(value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer_pretty(io::stdout(), value).map_err(|err| err.to_string())?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("q-skill-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn skill_markdown_has_frontmatter_and_install_pointer() {
        let skill = skill_markdown();
        assert!(skill.contains("name: q"));
        assert!(skill.contains("q feature create"));
        assert!(skill.contains("q skill install"));
        assert!(install_help().contains("q skill install"));
        assert!(install_help().contains("~/.agents/skills/q/SKILL.md"));
        assert!(install_help().contains("~/.cursor/skills/q/SKILL.md"));
    }

    #[test]
    fn install_agents_creates_then_updates() {
        let home = temp_home("agents");
        let created = write_skill(&home, &["agents".into()]).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].status, "created");
        let path = home.join(".agents/skills/q/SKILL.md");
        assert_eq!(fs::read_to_string(&path).unwrap(), skill_markdown());

        let updated = write_skill(&home, &["agents".into()]).unwrap();
        assert_eq!(updated[0].status, "updated");
        assert_eq!(updated[0].path, path);
    }

    #[test]
    fn unknown_target_is_rejected_before_writing() {
        let home = temp_home("bad");
        let err = write_skill(&home, &["grok".into()]).unwrap_err();
        assert!(err.contains("unknown skill target"));
        assert!(!home.join(".agents").exists());
    }

    #[test]
    fn all_targets_are_the_default_and_deduped() {
        let home = temp_home("all");
        let results = write_skill(&home, &["all".into(), "cursor".into()]).unwrap();
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|result| result.status == "created"));
        assert!(home.join(".codex/skills/q/SKILL.md").is_file());
        assert!(home.join(".claude/skills/q/SKILL.md").is_file());
    }
}
