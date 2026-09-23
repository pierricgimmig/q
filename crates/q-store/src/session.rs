//! Shared parameter and query surface for local SQLite and remote libsql.
//!
//! Migrations and the sync engine both talk through [`Db`], so the remote
//! database gets the same schema as the local file.

use rusqlite::types::{ToSql, ToSqlOutput, Value as RusqlValue};
use rusqlite::{params_from_iter, Connection};

use q_core::QueueError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SqlVal {
    Null,
    Int(i64),
    Text(String),
}

impl SqlVal {
    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub(crate) fn opt_text(value: Option<String>) -> Self {
        match value {
            Some(value) => Self::Text(value),
            None => Self::Null,
        }
    }
}

impl ToSql for SqlVal {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match self {
            Self::Null => Ok(ToSqlOutput::Owned(RusqlValue::Null)),
            Self::Int(value) => Ok(ToSqlOutput::Owned(RusqlValue::Integer(*value))),
            Self::Text(value) => Ok(ToSqlOutput::Owned(RusqlValue::Text(value.clone()))),
        }
    }
}

pub(crate) trait Db {
    fn execute_batch(&mut self, sql: &str) -> Result<(), QueueError>;
    fn execute(&mut self, sql: &str, params: &[SqlVal]) -> Result<u64, QueueError>;
    fn query(&mut self, sql: &str, params: &[SqlVal]) -> Result<Vec<Vec<SqlVal>>, QueueError>;
    fn last_insert_rowid(&self) -> i64;
    fn begin_immediate(&mut self) -> Result<(), QueueError>;
    fn commit(&mut self) -> Result<(), QueueError>;
    fn rollback(&mut self) -> Result<(), QueueError>;
}

impl Db for Connection {
    fn execute_batch(&mut self, sql: &str) -> Result<(), QueueError> {
        Connection::execute_batch(self, sql).map_err(|err| QueueError::Database(err.to_string()))
    }

    fn execute(&mut self, sql: &str, params: &[SqlVal]) -> Result<u64, QueueError> {
        Connection::execute(self, sql, params_from_iter(params.iter()))
            .map(|count| count as u64)
            .map_err(|err| QueueError::Database(err.to_string()))
    }

    fn query(&mut self, sql: &str, params: &[SqlVal]) -> Result<Vec<Vec<SqlVal>>, QueueError> {
        let mut stmt = self
            .prepare(sql)
            .map_err(|err| QueueError::Database(err.to_string()))?;
        let count = stmt.column_count();
        let mut rows = stmt
            .query(params_from_iter(params.iter()))
            .map_err(|err| QueueError::Database(err.to_string()))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| QueueError::Database(err.to_string()))?
        {
            let mut cols = Vec::with_capacity(count);
            for idx in 0..count {
                let value = row
                    .get_ref(idx)
                    .map_err(|err| QueueError::Database(err.to_string()))?;
                cols.push(match value {
                    rusqlite::types::ValueRef::Null => SqlVal::Null,
                    rusqlite::types::ValueRef::Integer(value) => SqlVal::Int(value),
                    rusqlite::types::ValueRef::Text(value) => SqlVal::Text(
                        std::str::from_utf8(value)
                            .map_err(|err| QueueError::Database(err.to_string()))?
                            .to_string(),
                    ),
                    rusqlite::types::ValueRef::Real(value) => SqlVal::Int(value as i64),
                    rusqlite::types::ValueRef::Blob(_) => SqlVal::Null,
                });
            }
            out.push(cols);
        }
        Ok(out)
    }

    fn last_insert_rowid(&self) -> i64 {
        Connection::last_insert_rowid(self)
    }

    fn begin_immediate(&mut self) -> Result<(), QueueError> {
        self.execute_batch("BEGIN IMMEDIATE")
    }

    fn commit(&mut self) -> Result<(), QueueError> {
        self.execute_batch("COMMIT")
    }

    fn rollback(&mut self) -> Result<(), QueueError> {
        self.execute_batch("ROLLBACK")
    }
}

pub(crate) fn query_i64(db: &mut dyn Db, sql: &str) -> Result<i64, QueueError> {
    let rows = db.query(sql, &[])?;
    match rows.first().and_then(|row| row.first()) {
        Some(SqlVal::Int(value)) => Ok(*value),
        _ => Err(QueueError::Database(format!(
            "expected one integer from `{sql}`"
        ))),
    }
}

pub(crate) fn query_i64s(db: &mut dyn Db, sql: &str) -> Result<Vec<i64>, QueueError> {
    let rows = db.query(sql, &[])?;
    let mut ids = Vec::new();
    for row in rows {
        match row.first() {
            Some(SqlVal::Int(value)) => ids.push(*value),
            _ => {
                return Err(QueueError::Database(format!(
                    "expected integers from `{sql}`"
                )))
            }
        }
    }
    Ok(ids)
}
