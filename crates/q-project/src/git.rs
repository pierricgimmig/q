use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    git_output(dir, &["rev-parse", "--show-toplevel"]).map(|value| {
        let path = PathBuf::from(value);
        fs::canonicalize(&path).unwrap_or(path)
    })
}

pub fn git_remote_origin(dir: &Path) -> Option<String> {
    git_output(dir, &["remote", "get-url", "origin"])
}

pub fn git_head(dir: &Path) -> Option<String> {
    git_output(dir, &["rev-parse", "HEAD"])
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

pub fn find_git_root_by_walking(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let marker = current.join(".git");
        if marker.is_dir() || marker.is_file() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}
