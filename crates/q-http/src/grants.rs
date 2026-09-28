//! Durable refresh-token families. Only nonce hashes are stored, never bearer tokens.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::crypto::{sha256, unix_now};

pub struct GrantStore(Mutex<Connection>);

impl GrantStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::init(Connection::open(path).map_err(|e| e.to_string())?)
    }

    pub fn in_memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(connection: Connection) -> Result<Self, String> {
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS oauth_grants (
                id TEXT PRIMARY KEY,
                nonce_hash BLOB NOT NULL,
                expires INTEGER NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0
            );",
            )
            .map_err(|e| e.to_string())?;
        Ok(Self(Mutex::new(connection)))
    }

    pub(crate) fn create(&self, id: &str, nonce: &str, expires: u64) -> Result<(), String> {
        let mut connection = self.0.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM oauth_grants WHERE expires <= ?", [unix_now()])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO oauth_grants (id, nonce_hash, expires) VALUES (?, ?, ?)",
            params![id, sha256(nonce.as_bytes()).as_slice(), expires],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Atomically consume a nonce. Reusing any old nonce revokes the entire family.
    pub(crate) fn rotate(
        &self,
        id: &str,
        nonce: &str,
        next: &str,
        expires: u64,
    ) -> Result<bool, String> {
        let mut connection = self.0.lock().map_err(|e| e.to_string())?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let current: Option<Vec<u8>> = tx
            .query_row(
                "SELECT nonce_hash FROM oauth_grants WHERE id = ? AND revoked = 0 AND expires > ?",
                params![id, unix_now()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(current) = current else {
            return Ok(false);
        };
        let matches = crate::crypto::constant_time_eq(&current, &sha256(nonce.as_bytes()));
        if matches {
            tx.execute(
                "UPDATE oauth_grants SET nonce_hash = ?, expires = ? WHERE id = ?",
                params![sha256(next.as_bytes()).as_slice(), expires, id],
            )
            .map_err(|e| e.to_string())?;
        } else {
            tx.execute("UPDATE oauth_grants SET revoked = 1 WHERE id = ?", [id])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(matches)
    }

    pub(crate) fn active(&self, id: &str) -> bool {
        let Ok(connection) = self.0.lock() else {
            return false;
        };
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM oauth_grants WHERE id = ? AND revoked = 0 AND expires > ?)",
            params![id, unix_now()], |row| row.get(0),
        ).unwrap_or(false)
    }
}
