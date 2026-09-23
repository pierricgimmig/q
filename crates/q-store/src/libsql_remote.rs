//! Blocking adapter around `libsql::Builder::new_remote`.
//!
//! The queue service is synchronous. libsql's remote client is async, so each
//! sync round-trip owns a current-thread runtime on a worker thread. That
//! avoids nesting runtimes inside the CLI's tokio main.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use q_core::QueueError;

use crate::session::{Db, SqlVal};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const OP_TIMEOUT: Duration = Duration::from_secs(8);

enum Cmd {
    Batch {
        sql: String,
        reply: Sender<Result<(), String>>,
    },
    Exec {
        sql: String,
        params: Vec<SqlVal>,
        reply: Sender<Result<u64, String>>,
    },
    Query {
        sql: String,
        params: Vec<SqlVal>,
        reply: Sender<Result<Vec<Vec<SqlVal>>, String>>,
    },
    LastId {
        reply: Sender<i64>,
    },
    Begin {
        reply: Sender<Result<(), String>>,
    },
    Commit {
        reply: Sender<Result<(), String>>,
    },
    Rollback {
        reply: Sender<Result<(), String>>,
    },
}

pub(crate) struct RemoteSession {
    tx: Sender<Cmd>,
}

impl RemoteSession {
    fn call<T>(&self, cmd: Cmd, rx: Receiver<T>) -> Result<T, QueueError> {
        self.tx
            .send(cmd)
            .map_err(|_| QueueError::Database("libsql worker stopped".into()))?;
        rx.recv()
            .map_err(|_| QueueError::Database("libsql worker stopped".into()))
    }
}

impl Db for RemoteSession {
    fn execute_batch(&mut self, sql: &str) -> Result<(), QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(
            Cmd::Batch {
                sql: sql.to_string(),
                reply,
            },
            rx,
        )?
        .map_err(QueueError::Database)
    }

    fn execute(&mut self, sql: &str, params: &[SqlVal]) -> Result<u64, QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(
            Cmd::Exec {
                sql: sql.to_string(),
                params: params.to_vec(),
                reply,
            },
            rx,
        )?
        .map_err(QueueError::Database)
    }

    fn query(&mut self, sql: &str, params: &[SqlVal]) -> Result<Vec<Vec<SqlVal>>, QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(
            Cmd::Query {
                sql: sql.to_string(),
                params: params.to_vec(),
                reply,
            },
            rx,
        )?
        .map_err(QueueError::Database)
    }

    fn last_insert_rowid(&self) -> i64 {
        let (reply, rx) = mpsc::channel();
        self.call(Cmd::LastId { reply }, rx).unwrap_or(0)
    }

    fn begin_immediate(&mut self) -> Result<(), QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(Cmd::Begin { reply }, rx)?
            .map_err(QueueError::Database)
    }

    fn commit(&mut self) -> Result<(), QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(Cmd::Commit { reply }, rx)?
            .map_err(QueueError::Database)
    }

    fn rollback(&mut self) -> Result<(), QueueError> {
        let (reply, rx) = mpsc::channel();
        self.call(Cmd::Rollback { reply }, rx)?
            .map_err(QueueError::Database)
    }
}

pub(crate) fn with_remote<T>(
    url: &str,
    auth_token: &str,
    f: impl FnOnce(&mut RemoteSession) -> Result<T, QueueError>,
) -> Result<T, QueueError> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let url = url.to_string();
    let auth_token = auth_token.to_string();
    let worker = std::thread::spawn(move || worker(url, auth_token, rx, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = worker.join();
            return Err(QueueError::Database(err));
        }
        Err(_) => {
            let _ = worker.join();
            return Err(QueueError::Database(
                "libsql worker exited before connect".into(),
            ));
        }
    }
    let mut session = RemoteSession { tx };
    let result = f(&mut session);
    drop(session);
    let _ = worker.join();
    result
}

fn worker(url: String, auth_token: String, rx: Receiver<Cmd>, ready: Sender<Result<(), String>>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    runtime.block_on(async move {
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, connect(&url, &auth_token)).await;
        let conn = match connected {
            Ok(Ok(conn)) => conn,
            Ok(Err(err)) => {
                let _ = ready.send(Err(err));
                return;
            }
            Err(_) => {
                let _ = ready.send(Err(format!("timed out connecting to libsql remote {url}")));
                return;
            }
        };
        if let Err(err) = conn.execute("PRAGMA foreign_keys = ON", ()).await {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
        if ready.send(Ok(())).is_err() {
            return;
        }
        let mut tx: Option<libsql::Transaction> = None;
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Batch { sql, reply } => {
                    let result = if let Some(transaction) = &tx {
                        timeout_op(transaction.execute_batch(&sql)).await
                    } else {
                        timeout_op(conn.execute_batch(&sql)).await
                    };
                    let _ = reply.send(result.map(|_| ()).map_err(|err| err.to_string()));
                }
                Cmd::Exec { sql, params, reply } => {
                    let values = to_libsql(&params);
                    let result = if let Some(transaction) = &tx {
                        timeout_op(transaction.execute(&sql, values)).await
                    } else {
                        timeout_op(conn.execute(&sql, values)).await
                    };
                    let _ = reply.send(result.map_err(|err| err.to_string()));
                }
                Cmd::Query { sql, params, reply } => {
                    let values = to_libsql(&params);
                    let result = async {
                        let mut rows = if let Some(transaction) = &tx {
                            transaction.query(&sql, values).await?
                        } else {
                            conn.query(&sql, values).await?
                        };
                        let width = rows.column_count();
                        let mut out = Vec::new();
                        while let Some(row) = rows.next().await? {
                            let mut cols = Vec::with_capacity(width as usize);
                            for idx in 0..width {
                                cols.push(from_libsql(row.get_value(idx)?));
                            }
                            out.push(cols);
                        }
                        Ok(out)
                    };
                    let result = timeout_op(result).await.map_err(|err| err.to_string());
                    let _ = reply.send(result);
                }
                Cmd::LastId { reply } => {
                    let id = if let Some(transaction) = &tx {
                        transaction.last_insert_rowid()
                    } else {
                        conn.last_insert_rowid()
                    };
                    let _ = reply.send(id);
                }
                Cmd::Begin { reply } => {
                    if tx.is_some() {
                        let _ = reply.send(Err("libsql transaction already open".into()));
                        continue;
                    }
                    match timeout_op(
                        conn.transaction_with_behavior(libsql::TransactionBehavior::Immediate),
                    )
                    .await
                    {
                        Ok(transaction) => {
                            tx = Some(transaction);
                            let _ = reply.send(Ok(()));
                        }
                        Err(err) => {
                            let _ = reply.send(Err(err.to_string()));
                        }
                    }
                }
                Cmd::Commit { reply } => match tx.take() {
                    Some(transaction) => {
                        let result = timeout_op(transaction.commit())
                            .await
                            .map_err(|err| err.to_string());
                        let _ = reply.send(result);
                    }
                    None => {
                        let _ = reply.send(Err("libsql commit without a transaction".into()));
                    }
                },
                Cmd::Rollback { reply } => match tx.take() {
                    Some(transaction) => {
                        let result = timeout_op(transaction.rollback())
                            .await
                            .map_err(|err| err.to_string());
                        let _ = reply.send(result);
                    }
                    None => {
                        let _ = reply.send(Ok(()));
                    }
                },
            }
        }
    });
}

async fn connect(url: &str, auth_token: &str) -> Result<libsql::Connection, String> {
    // `Builder::new_remote` is the supported HTTP client. Embedded replicas
    // (`new_remote_replica`) forward writes to the primary, so they cannot
    // accept offline local writes. Offline writes stay in the local SQLite
    // file; this connection is only the central authority.
    let db = libsql::Builder::new_remote(url.to_string(), auth_token.to_string())
        .build()
        .await
        .map_err(|err| err.to_string())?;
    db.connect().map_err(|err| err.to_string())
}

async fn timeout_op<T>(
    fut: impl std::future::Future<Output = Result<T, libsql::Error>>,
) -> Result<T, libsql::Error> {
    match tokio::time::timeout(OP_TIMEOUT, fut).await {
        Ok(result) => result,
        Err(_) => Err(libsql::Error::ConnectionFailed(
            "timed out talking to the libsql remote".into(),
        )),
    }
}

fn to_libsql(params: &[SqlVal]) -> Vec<libsql::Value> {
    params
        .iter()
        .map(|value| match value {
            SqlVal::Null => libsql::Value::Null,
            SqlVal::Int(value) => libsql::Value::Integer(*value),
            SqlVal::Text(value) => libsql::Value::Text(value.clone()),
        })
        .collect()
}

fn from_libsql(value: libsql::Value) -> SqlVal {
    match value {
        libsql::Value::Null => SqlVal::Null,
        libsql::Value::Integer(value) => SqlVal::Int(value),
        libsql::Value::Real(value) => SqlVal::Int(value as i64),
        libsql::Value::Text(value) => SqlVal::Text(value),
        libsql::Value::Blob(_) => SqlVal::Null,
    }
}
