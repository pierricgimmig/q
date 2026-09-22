/// Normalize common Git remote URL forms to `host/owner/repo`.
///
/// `git@github.com:acme/profiler-core.git`,
/// `https://github.com/acme/profiler-core.git`, and
/// `github.com/acme/profiler-core` all become
/// `github.com/acme/profiler-core`.
pub fn normalize_repo_url(input: &str) -> String {
    let original = input.trim();
    if original.is_empty() {
        return String::new();
    }

    let mut rest = original;
    let had_scheme = rest.contains("://");
    if had_scheme {
        if let Some((_, after)) = rest.split_once("://") {
            rest = after;
        }
    }

    if !had_scheme {
        if let Some((hostpart, path)) = rest.split_once(':') {
            if path.contains('/') && !hostpart.contains('/') {
                return join_host_path(hostpart, path);
            }
        }
    }

    if let Some((host, path)) = rest.split_once('/') {
        return join_host_path(host, path);
    }

    strip_dot_git(rest.trim()).trim_matches('/').to_string()
}

fn join_host_path(host: &str, path: &str) -> String {
    let host = host.trim();
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.strip_prefix("www.").unwrap_or(host);
    let host = host.trim_matches('/').to_ascii_lowercase();
    let path = strip_dot_git(path.trim().trim_matches('/'));
    if host.is_empty() {
        return path.to_string();
    }
    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

fn strip_dot_git(value: &str) -> &str {
    value.strip_suffix(".git").unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_github_forms() {
        let expected = "github.com/acme/profiler-core";
        for input in [
            "git@github.com:acme/profiler-core.git",
            "https://github.com/acme/profiler-core.git",
            "http://github.com/acme/profiler-core",
            "github.com/acme/profiler-core",
            "ssh://git@github.com/acme/profiler-core.git",
            "https://github.com/acme/profiler-core/",
            "https://www.github.com/acme/profiler-core.git",
            "git://github.com/acme/profiler-core.git",
            "ssh://git@github.com:22/acme/profiler-core.git",
            "org-123@github.com:acme/profiler-core.git",
        ] {
            assert_eq!(normalize_repo_url(input), expected, "input {input}");
        }
    }

    #[test]
    fn preserves_repo_path_case_and_lowercases_host() {
        assert_eq!(
            normalize_repo_url("https://GitHub.com/Acme/Profiler-Core.git"),
            "github.com/Acme/Profiler-Core"
        );
    }
}
