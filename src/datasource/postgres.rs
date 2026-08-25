use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{StreamExt, TryStreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::tunnel::{Tunnel, TunnelSpec};

use super::introspect;
use super::stream::{BoxedResultStream, ResultMessage, StreamState};
use super::{Column, DataSource, DataSourceError, QueryId, RowStream, Schema, Table};

pub struct ConnectOptions {
    pub connect_timeout: Duration,
    pub application_name: String,
    pub tunnel: crate::tunnel::TunnelOptions,
    pub cancel_timeout: Duration,
    pub abandon_grace: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            application_name: "ratwarren".to_string(),
            tunnel: crate::tunnel::TunnelOptions::default(),
            cancel_timeout: Duration::from_secs(5),
            abandon_grace: Duration::from_millis(100),
        }
    }
}

pub struct PostgresDataSource {
    name: String,
    dialed_host: String,
    dialed_port: u16,
    client: tokio_postgres::Client,
    conn_task: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    conn_error: Arc<Mutex<Option<String>>>,
    cancel_token: tokio_postgres::CancelToken,
    tunnel: Option<Mutex<Tunnel>>,
    slot: Arc<Semaphore>,
    next_id: AtomicU64,
    active_id: Arc<AtomicU64>,
    // Invariant: no request may be enqueued onto the connection while a
    // cancel request for this connection is being written. Every method
    // that issues a request to Postgres holds this gate across the
    // issuance; `cancel` (and the abandoned-stream drain task) hold it
    // across their whole check-and-send. This closes the race where a
    // cancel meant for query A lands on the wire after query B has already
    // been sent and kills B instead.
    start_gate: Arc<tokio::sync::Mutex<()>>,
    cancel_timeout: Duration,
    abandon_grace: Duration,
}

impl PostgresDataSource {
    pub async fn connect(
        conn: &crate::config::Connection,
        password: Option<&str>,
    ) -> Result<Self, DataSourceError> {
        Self::connect_with(conn, password, &ConnectOptions::default()).await
    }

    pub async fn connect_with(
        conn: &crate::config::Connection,
        password: Option<&str>,
        options: &ConnectOptions,
    ) -> Result<Self, DataSourceError> {
        let spec = TunnelSpec::from_connection(conn).map_err(|source| DataSourceError::Tunnel {
            name: conn.name.clone(),
            source,
        })?;

        let (host, port, tunnel): (String, u16, Option<Tunnel>) = match spec {
            Some(spec) => {
                let tunnel_options = options.tunnel.clone();
                let join =
                    tokio::task::spawn_blocking(move || Tunnel::open_with(&spec, &tunnel_options));
                match join.await {
                    Err(_join_err) => {
                        return Err(DataSourceError::TunnelTaskPanicked {
                            name: conn.name.clone(),
                        });
                    }
                    Ok(Err(tunnel_err)) => {
                        return Err(DataSourceError::Tunnel {
                            name: conn.name.clone(),
                            source: tunnel_err,
                        });
                    }
                    Ok(Ok(tunnel)) => {
                        let port = tunnel.local_port();
                        ("127.0.0.1".to_string(), port, Some(tunnel))
                    }
                }
            }
            None => (conn.host.clone(), conn.port, None),
        };

        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&host)
            .port(port)
            .user(&conn.user)
            .dbname(&conn.database)
            .application_name(&options.application_name)
            .connect_timeout(options.connect_timeout);
        if let Some(pw) = password {
            cfg.password(pw);
        }

        let (client, connection) = match cfg.connect(tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(connect_err) => {
                if let Some(mut tunnel) = tunnel
                    && let Err(check_err) = tunnel.check_alive()
                {
                    return Err(DataSourceError::TunnelDown {
                        name: conn.name.clone(),
                        source: check_err,
                    });
                }
                return Err(DataSourceError::Connect {
                    name: conn.name.clone(),
                    addr: format!("{host}:{port}"),
                    source: connect_err,
                });
            }
        };

        let cancel_token = client.cancel_token();

        let conn_error = Arc::new(Mutex::new(None));
        let conn_error_task = Arc::clone(&conn_error);
        let conn_task = tokio::spawn(async move {
            if let Err(e) = connection.await {
                *conn_error_task
                    .lock()
                    .expect("conn_error mutex should not be poisoned") = Some(format!("{e}"));
            }
        });

        Ok(Self {
            name: conn.name.clone(),
            dialed_host: host,
            dialed_port: port,
            client,
            conn_task,
            conn_error,
            cancel_token,
            tunnel: tunnel.map(Mutex::new),
            slot: Arc::new(Semaphore::new(1)),
            next_id: AtomicU64::new(1),
            active_id: Arc::new(AtomicU64::new(0)),
            start_gate: Arc::new(tokio::sync::Mutex::new(())),
            cancel_timeout: options.cancel_timeout,
            abandon_grace: options.abandon_grace,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dialed_addr(&self) -> (&str, u16) {
        (&self.dialed_host, self.dialed_port)
    }

    pub fn tunnel_local_port(&self) -> Option<u16> {
        self.tunnel.as_ref().map(|t| {
            t.lock()
                .expect("tunnel mutex should not be poisoned")
                .local_port()
        })
    }

    pub async fn close(self) {
        self.conn_task.abort();
        if let Some(tunnel) = self.tunnel {
            let tunnel = tunnel
                .into_inner()
                .expect("tunnel mutex should not be poisoned");
            let _ = tokio::task::spawn_blocking(move || tunnel.shutdown()).await;
        }
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, DataSourceError> {
        self.slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| DataSourceError::Busy {
                name: self.name.clone(),
            })
    }
}

#[async_trait::async_trait]
impl DataSource for PostgresDataSource {
    async fn list_schemas(&self) -> Result<Vec<Schema>, DataSourceError> {
        let _permit = self.try_acquire()?;

        let rows = {
            let _gate = self.start_gate.lock().await;
            self.client
                .query_raw(
                    introspect::LIST_SCHEMAS,
                    std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>(),
                )
                .await
                .map_err(|source| DataSourceError::Query {
                    sql: introspect::LIST_SCHEMAS.to_string(),
                    source,
                })?
        };

        let mut rows = std::pin::pin!(rows);
        let mut schemas = Vec::new();
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|source| DataSourceError::Query {
                sql: introspect::LIST_SCHEMAS.to_string(),
                source,
            })?
        {
            schemas.push(introspect::row_to_schema(&row));
        }
        Ok(schemas)
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<Table>, DataSourceError> {
        let _permit = self.try_acquire()?;

        let rows = {
            let _gate = self.start_gate.lock().await;
            self.client
                .query_raw(introspect::LIST_TABLES, [&schema])
                .await
                .map_err(|source| DataSourceError::Query {
                    sql: introspect::LIST_TABLES.to_string(),
                    source,
                })?
        };

        let mut rows = std::pin::pin!(rows);
        let mut tables = Vec::new();
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|source| DataSourceError::Query {
                sql: introspect::LIST_TABLES.to_string(),
                source,
            })?
        {
            tables.push(introspect::row_to_table(&row)?);
        }
        Ok(tables)
    }

    async fn list_columns(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<Column>, DataSourceError> {
        let _permit = self.try_acquire()?;

        let rows = {
            let _gate = self.start_gate.lock().await;
            self.client
                .query_raw(introspect::LIST_COLUMNS, [&schema, &table])
                .await
                .map_err(|source| DataSourceError::Query {
                    sql: introspect::LIST_COLUMNS.to_string(),
                    source,
                })?
        };

        let mut rows = std::pin::pin!(rows);
        let mut columns = Vec::new();
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|source| DataSourceError::Query {
                sql: introspect::LIST_COLUMNS.to_string(),
                source,
            })?
        {
            columns.push(introspect::row_to_column(&row));
        }
        Ok(columns)
    }

    async fn execute(&self, sql: &str) -> Result<RowStream, DataSourceError> {
        let permit = self.try_acquire()?;
        let gate = self.start_gate.lock().await;

        let sql_owned = sql.to_string();
        let simple_stream = self
            .client
            .simple_query_raw(sql)
            .await
            .map_err(|source| map_query_error(&sql_owned, source))?;

        let map_sql = sql_owned.clone();
        let adapted = simple_stream.map(move |item| match item {
            Ok(tokio_postgres::SimpleQueryMessage::RowDescription(cols)) => {
                let names: Arc<[String]> = cols
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect::<Vec<_>>()
                    .into();
                Ok(ResultMessage::Columns(names))
            }
            Ok(tokio_postgres::SimpleQueryMessage::Row(row)) => {
                let values = (0..row.len())
                    .map(|i| row.get(i).map(str::to_string))
                    .collect();
                Ok(ResultMessage::Row(values))
            }
            Ok(tokio_postgres::SimpleQueryMessage::CommandComplete(rows_affected)) => {
                Ok(ResultMessage::Complete { rows_affected })
            }
            // SimpleQueryMessage is #[non_exhaustive]; a fabricated Complete
            // here would trigger a spurious MultipleStatements error on any
            // future upstream variant, so just ignore it instead.
            Ok(_) => Ok(ResultMessage::Ignored),
            Err(source) => Err(map_query_error(&map_sql, source)),
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active_id.store(id, Ordering::Release);
        drop(gate);

        Ok(RowStream {
            inner: Box::pin(adapted),
            query_id: QueryId(id),
            columns: None,
            rows_affected: None,
            active_id: Arc::clone(&self.active_id),
            permit: Some(permit),
            abandon: Some(AbandonCtx {
                cancel_token: self.cancel_token.clone(),
                start_gate: Arc::clone(&self.start_gate),
                grace: self.abandon_grace,
                cancel_timeout: self.cancel_timeout,
            }),
            state: StreamState::Streaming,
        })
    }

    async fn explain(&self, sql: &str) -> Result<String, DataSourceError> {
        let _permit = self.try_acquire()?;

        let stmt = format!("EXPLAIN (FORMAT TEXT) {sql}");
        let rows = {
            let _gate = self.start_gate.lock().await;
            self.client
                .query_raw(
                    &stmt,
                    std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>(),
                )
                .await
                .map_err(|source| DataSourceError::Query {
                    sql: sql.to_string(),
                    source,
                })?
        };

        let mut rows = std::pin::pin!(rows);
        let mut lines = Vec::new();
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|source| DataSourceError::Query {
                sql: sql.to_string(),
                source,
            })?
        {
            let line: String = row.get(0);
            lines.push(line);
        }
        Ok(lines.join("\n"))
    }

    async fn cancel(&self, query_id: QueryId) -> Result<(), DataSourceError> {
        let _gate = self.start_gate.lock().await;
        if self.active_id.load(Ordering::Acquire) != query_id.0 {
            return Ok(());
        }
        match tokio::time::timeout(
            self.cancel_timeout,
            self.cancel_token.cancel_query(tokio_postgres::NoTls),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(DataSourceError::CancelFailed {
                name: self.name.clone(),
                source,
            }),
            Err(_) => Err(DataSourceError::CancelTimedOut {
                name: self.name.clone(),
                timeout: self.cancel_timeout,
            }),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AbandonCtx {
    pub(crate) cancel_token: tokio_postgres::CancelToken,
    pub(crate) start_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) grace: Duration,
    pub(crate) cancel_timeout: Duration,
}

impl AbandonCtx {
    pub(crate) fn try_spawn_drain(
        self,
        inner: BoxedResultStream,
        query_id: QueryId,
        active_id: Arc<AtomicU64>,
        permit: OwnedSemaphorePermit,
        allow_cancel: bool,
    ) -> Result<(), (BoxedResultStream, OwnedSemaphorePermit)> {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Err((inner, permit));
        };
        handle.spawn(drain_abandoned(
            inner,
            self,
            query_id,
            active_id,
            permit,
            allow_cancel,
        ));
        Ok(())
    }
}

async fn drain_abandoned(
    mut inner: BoxedResultStream,
    ctx: AbandonCtx,
    query_id: QueryId,
    active_id: Arc<AtomicU64>,
    permit: OwnedSemaphorePermit,
    allow_cancel: bool,
) {
    let drain = async {
        while let Some(item) = inner.next().await {
            // Not fused: a closed connection keeps yielding Err forever, so
            // this break is load-bearing (without it, this task spins at
            // 100% CPU).
            if item.is_err() {
                break;
            }
        }
    };
    tokio::pin!(drain);
    if allow_cancel && tokio::time::timeout(ctx.grace, &mut drain).await.is_err() {
        {
            let _gate = ctx.start_gate.lock().await;
            if active_id.load(Ordering::Acquire) == query_id.0 {
                let _ = tokio::time::timeout(
                    ctx.cancel_timeout,
                    ctx.cancel_token.cancel_query(tokio_postgres::NoTls),
                )
                .await;
            }
        }
        drain.await;
    } else if !allow_cancel {
        drain.await;
    }
    let _ = active_id.compare_exchange(query_id.0, 0, Ordering::Release, Ordering::Relaxed);
    drop(permit);
}

fn map_query_error(sql: &str, source: tokio_postgres::Error) -> DataSourceError {
    if source.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) {
        DataSourceError::Cancelled
    } else {
        DataSourceError::Query {
            sql: sql.to_string(),
            source,
        }
    }
}

pub fn quote_ident(ident: &str) -> String {
    let mut quoted = String::with_capacity(ident.len() + 2);
    quoted.push('"');
    for c in ident.chars() {
        if c == '"' {
            quoted.push('"');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_in_double_quotes() {
        assert_eq!(quote_ident("foo"), "\"foo\"");
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("foo\"bar"), "\"foo\"\"bar\"");
    }

    #[test]
    fn quote_ident_handles_empty_string() {
        assert_eq!(quote_ident(""), "\"\"");
    }
}
