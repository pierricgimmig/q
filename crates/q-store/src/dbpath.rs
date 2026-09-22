use std::path::{Path, PathBuf};

pub fn default_db_path() -> PathBuf {
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    resolve_db_path(
        xdg.as_deref(),
        home.as_deref(),
        local_app_data.as_deref(),
        std::env::consts::OS,
    )
}

pub fn resolve_db_path(
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    target_os: &str,
) -> PathBuf {
    if let Some(xdg) = xdg_data_home {
        if !xdg.as_os_str().is_empty() {
            return xdg.join("q").join("queue.db");
        }
    }
    if target_os == "macos" {
        if let Some(home) = home {
            return home
                .join("Library")
                .join("Application Support")
                .join("q")
                .join("queue.db");
        }
    } else if target_os == "windows" {
        if let Some(local_app_data) = local_app_data {
            return local_app_data.join("q").join("queue.db");
        }
        if let Some(home) = home {
            return home
                .join("AppData")
                .join("Local")
                .join("q")
                .join("queue.db");
        }
    } else if let Some(home) = home {
        return home.join(".local").join("share").join("q").join("queue.db");
    }
    PathBuf::from("q").join("queue.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_data_home_wins_on_every_platform() {
        let xdg = Path::new("/custom/data");
        let home = Path::new("/home/me");
        for os in ["linux", "macos", "windows"] {
            assert_eq!(
                resolve_db_path(Some(xdg), Some(home), None, os),
                PathBuf::from("/custom/data/q/queue.db")
            );
        }
    }

    #[test]
    fn platform_fallbacks() {
        let home = Path::new("/home/me");
        assert_eq!(
            resolve_db_path(None, Some(home), None, "linux"),
            PathBuf::from("/home/me/.local/share/q/queue.db")
        );
        assert_eq!(
            resolve_db_path(None, Some(home), None, "macos"),
            PathBuf::from("/home/me/Library/Application Support/q/queue.db")
        );
        let local = Path::new("C:/Users/me/AppData/Local");
        assert_eq!(
            resolve_db_path(None, Some(home), Some(local), "windows"),
            PathBuf::from("C:/Users/me/AppData/Local/q/queue.db")
        );
    }
}
