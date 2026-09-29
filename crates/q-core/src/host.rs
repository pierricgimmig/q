//! Hostname of the machine a worker is running on.
//!
//! Detection uses the platform call (`gethostname` / `GetComputerNameW`) so it
//! works without an extra crate. Call it on the worker. `q serve` must not
//! fill a missing host with its own name; a remote claim records the host the
//! client sent.

/// This machine's hostname, or `None` when the platform call fails.
pub fn local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        unix_hostname()
    }
    #[cfg(windows)]
    {
        windows_hostname()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Model and host a local worker should put on a claim.
///
/// Model comes from the explicit value, then `Q_AGENT_MODEL`. Host comes from
/// the explicit value, then `Q_AGENT_HOST`, then [`local_hostname`]. Blank
/// strings count as unset. Do not call this on the server side of `q serve`.
pub fn local_worker_identity(
    model: Option<String>,
    host: Option<String>,
) -> (Option<String>, Option<String>) {
    let model = nonempty(model).or_else(|| nonempty(std::env::var("Q_AGENT_MODEL").ok()));
    let host = nonempty(host)
        .or_else(|| nonempty(std::env::var("Q_AGENT_HOST").ok()))
        .or_else(local_hostname);
    (model, host)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Trim, split commas, drop empties, and dedupe case-insensitively.
/// A tag longer than 64 characters is rejected.
pub fn normalize_tags(tags: &[String]) -> Result<Vec<String>, crate::QueueError> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tag in tags {
        for part in tag.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.chars().count() > 64 {
                return Err(crate::QueueError::InvalidInput(format!(
                    "tag '{trimmed}' is longer than 64 characters"
                )));
            }
            let key = trimmed.to_ascii_lowercase();
            if seen.insert(key) {
                out.push(trimmed.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn unix_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes, and
    // `gethostname` writes a NUL-terminated name into it.
    let rc = unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(unix)]
unsafe extern "C" {
    fn gethostname(buf: *mut std::ffi::c_char, len: usize) -> i32;
}

#[cfg(windows)]
fn windows_hostname() -> Option<String> {
    use std::os::windows::ffi::OsStringExt;

    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 code units and `len` is a valid pointer.
    let rc = unsafe { GetComputerNameW(buf.as_mut_ptr(), &mut len) };
    if rc == 0 {
        return None;
    }
    let name = std::ffi::OsString::from_wide(&buf[..len as usize])
        .to_string_lossy()
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetComputerNameW(buffer: *mut u16, size: *mut u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_is_detected() {
        let name = local_hostname().expect("hostname");
        assert!(!name.is_empty());
        assert!(!name.contains('\0'));
    }

    #[test]
    fn explicit_identity_wins_over_detection() {
        let (model, host) = local_worker_identity(Some("opus".into()), Some("worker-a".into()));
        assert_eq!(model.as_deref(), Some("opus"));
        assert_eq!(host.as_deref(), Some("worker-a"));
        let (model, host) = local_worker_identity(Some("  ".into()), Some("".into()));
        assert!(model.is_none());
        assert!(host.is_some());
    }

    #[test]
    fn tags_are_trimmed_split_and_deduped() {
        let tags = normalize_tags(&[" Rust ".into(), "rust,db".into()]).unwrap();
        assert_eq!(tags, vec!["Rust".to_string(), "db".to_string()]);
        assert!(normalize_tags(&["x".repeat(65)]).is_err());
    }
}
