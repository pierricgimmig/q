//! Token file, roles, and the live-reloading store `q serve` reads from.
//!
//! ```toml
//! [[tokens]]
//! name = "pierric"
//! role = "human"
//! secret = "..."
//!
//! [[tokens]]
//! name = "codex-vps"
//! role = "agent"
//! secret = "..."
//! ```
//!
//! `q token create` appends to this file. The server checks the file's
//! modification time on each request and reloads it when it changes, so a
//! new or revoked token takes effect without a restart.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::crypto::{constant_time_eq, random_token};

pub const MIN_SECRET_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Human,
    Agent,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            other => Err(format!("unknown role {other}; use human or agent")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    pub name: String,
    pub role: Role,
    pub secret: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub tokens: Vec<AuthToken>,
}

impl AuthConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read token file {}: {err}", path.display()))?;
        Self::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: AuthConfig = toml::from_str(text).map_err(|err| err.to_string())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        for token in &self.tokens {
            let name = token.name.trim();
            if name.is_empty() {
                return Err("every token needs a name".into());
            }
            if !names.insert(name.to_string()) {
                return Err(format!("duplicate token name {name}"));
            }
            if token.secret.len() < MIN_SECRET_LEN {
                return Err(format!(
                    "token {name}: secret must be at least {MIN_SECRET_LEN} characters"
                ));
            }
        }
        Ok(())
    }

    /// Write the file with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let text = toml::to_string(self).map_err(|err| err.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        std::fs::write(path, text)
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Add a token with a fresh random secret to `path`, creating the file if
    /// needed. Returns the secret; it is not stored anywhere else.
    pub fn create_token(path: &Path, name: &str, role: Role) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("token name is required".into());
        }
        let mut config = if path.exists() {
            Self::load(path)?
        } else {
            Self::default()
        };
        if config.tokens.iter().any(|token| token.name == name) {
            return Err(format!(
                "token {name} already exists in {}; revoke it first",
                path.display()
            ));
        }
        let secret = random_token();
        config.tokens.push(AuthToken {
            name: name.to_string(),
            role,
            secret: secret.clone(),
        });
        config.save(path)?;
        Ok(secret)
    }

    /// Remove a token by name. Sessions it signed in are invalid afterwards.
    pub fn revoke_token(path: &Path, name: &str) -> Result<(), String> {
        let mut config = Self::load(path)?;
        let before = config.tokens.len();
        config.tokens.retain(|token| token.name != name.trim());
        if config.tokens.len() == before {
            return Err(format!("no token named {name} in {}", path.display()));
        }
        config.save(path)
    }

    /// Resolve a bearer secret to its token.
    pub fn find_by_secret(&self, secret: &str) -> Option<&AuthToken> {
        let secret = secret.trim();
        self.tokens
            .iter()
            .find(|token| constant_time_eq(token.secret.as_bytes(), secret.as_bytes()))
    }

    pub fn find_by_name(&self, name: &str) -> Option<&AuthToken> {
        self.tokens.iter().find(|token| token.name == name)
    }
}

/// Who is calling. Anonymous loopback callers are a human named `anonymous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            name: "anonymous".into(),
            role: Role::Human,
        }
    }

    pub fn from_token(token: &AuthToken) -> Self {
        Self {
            name: token.name.clone(),
            role: token.role,
        }
    }
}

struct Loaded {
    config: AuthConfig,
    modified: Option<SystemTime>,
}

/// The token file as the server sees it, reloaded when the file changes.
pub struct TokenStore {
    path: Option<PathBuf>,
    loaded: RwLock<Loaded>,
}

impl TokenStore {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let config = AuthConfig::load(path)?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            loaded: RwLock::new(Loaded {
                config,
                modified: modified_at(path),
            }),
        })
    }

    /// A fixed configuration that never reloads. Used by tests.
    pub fn fixed(config: AuthConfig) -> Self {
        Self {
            path: None,
            loaded: RwLock::new(Loaded {
                config,
                modified: None,
            }),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Current configuration, re-read if the file changed. Missing, unreadable,
    /// or invalid files deny all access until a valid configuration is restored.
    pub fn snapshot(&self) -> AuthConfig {
        // Serialize reloads so an older snapshot cannot overwrite a revocation.
        let Ok(mut loaded) = self.loaded.write() else {
            return AuthConfig::default();
        };
        if let Some(path) = &self.path {
            let modified = modified_at(path);
            if modified.is_none() || loaded.modified != modified {
                match AuthConfig::load(path) {
                    Ok(config) => {
                        loaded.config = config;
                        loaded.modified = modified;
                    }
                    Err(err) => {
                        tracing::warn!("token file unavailable; denying access: {err}");
                        loaded.config = AuthConfig::default();
                        // Retry even if a repaired file has the same timestamp.
                        loaded.modified = None;
                    }
                }
            }
        }
        loaded.config.clone()
    }

    pub fn len(&self) -> usize {
        self.snapshot().tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn modified_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("q-tokens-{}.toml", uuid::Uuid::new_v4()))
    }

    #[test]
    fn token_file_parses_and_validates() {
        let config = AuthConfig::parse(
            r#"
            [[tokens]]
            name = "me"
            role = "human"
            secret = "0123456789abcdef0123"

            [[tokens]]
            name = "bot"
            role = "agent"
            secret = "fedcba9876543210fedc"
            "#,
        )
        .unwrap();
        assert_eq!(config.tokens.len(), 2);
        let me = config.find_by_secret("0123456789abcdef0123").unwrap();
        assert_eq!(me.role, Role::Human);
        assert_eq!(me.name, "me");
        assert_eq!(
            config.find_by_secret("fedcba9876543210fedc").unwrap().role,
            Role::Agent
        );
        assert!(config.find_by_secret("nope").is_none());
    }

    #[test]
    fn token_file_rejects_short_secrets_and_duplicates() {
        let short = AuthConfig::parse("[[tokens]]\nname=\"a\"\nrole=\"human\"\nsecret=\"short\"\n");
        assert!(short.unwrap_err().contains("at least"));
        let dup = AuthConfig::parse(
            "[[tokens]]\nname=\"a\"\nrole=\"human\"\nsecret=\"0123456789abcdef\"\n[[tokens]]\nname=\"a\"\nrole=\"agent\"\nsecret=\"0123456789abcdefg\"\n",
        );
        assert!(dup.unwrap_err().contains("duplicate"));
        assert!(AuthConfig::parse("tokens = []").unwrap().tokens.is_empty());
    }

    #[test]
    fn token_store_denies_access_after_deletion_or_invalid_reload() {
        let path = temp_path();
        let secret = AuthConfig::create_token(&path, "me", Role::Human).unwrap();
        let store = TokenStore::from_file(&path).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(store.snapshot().find_by_secret(&secret).is_none());
        std::fs::write(&path, "not valid toml").unwrap();
        assert!(store.snapshot().tokens.is_empty());
        std::fs::write(&path, original).unwrap();
        assert!(store.snapshot().find_by_secret(&secret).is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn create_and_revoke_round_trip_through_the_file_and_the_store() {
        let path = temp_path();
        let secret = AuthConfig::create_token(&path, "pierric", Role::Human).unwrap();
        assert!(secret.len() >= MIN_SECRET_LEN);
        let store = TokenStore::from_file(&path).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.snapshot().find_by_secret(&secret).is_some());

        // The store notices a token added after it was opened.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let agent = AuthConfig::create_token(&path, "bot", Role::Agent).unwrap();
        // Coarse mtime file systems may not advance within one test; force it.
        // Windows needs a writable handle to set the time.
        let bumped = before + std::time::Duration::from_secs(2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        let snapshot = store.snapshot();
        assert_eq!(snapshot.tokens.len(), 2);
        assert_eq!(snapshot.find_by_secret(&agent).unwrap().role, Role::Agent);

        assert!(AuthConfig::create_token(&path, "bot", Role::Agent)
            .unwrap_err()
            .contains("already exists"));
        AuthConfig::revoke_token(&path, "bot").unwrap();
        assert!(AuthConfig::load(&path)
            .unwrap()
            .find_by_name("bot")
            .is_none());
        AuthConfig::revoke_token(&path, "pierric").unwrap();
        assert!(path.exists());
        assert!(AuthConfig::load(&path).unwrap().tokens.is_empty());
        assert!(store.snapshot().tokens.is_empty());
        std::fs::remove_file(path).unwrap();
        assert!(Role::parse("HUMAN").is_ok());
        assert!(Role::parse("root").is_err());
    }
}
