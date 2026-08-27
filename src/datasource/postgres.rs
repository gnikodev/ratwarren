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
    pub cancel_settle: Duration,
    // Deliberately separate from `cancel_timeout` (5s, used by the
    // user-initiated `cancel()`): measured `cancel_query` latency is
    // 105-220µs, so reusing the 5s user-facing timeout in the abandon
    // retry loop below would let a handful of stuck attempts blow past
    // `retry_on_busy`'s 10s budget on its own.
    pub abandon_cancel_timeout: Duration,
    pub cancel_escalate: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            application_name: "ratwarren".to_string(),
            tunnel: crate::tunnel::TunnelOptions::default(),
            cancel_timeout: Duration::from_secs(5),
            abandon_grace: Duration::from_millis(100),
            // Postgres's cancel protocol is out-of-band and unacknowledged:
            // the server never confirms a cancel was received or applied,
            // so there is no principled way to know when it's safe to reuse
            // the connection for the next query. This delay is an
            // empirically-tuned heuristic, not a protocol guarantee. A
            // 60-iteration adversarial repro against a real Postgres
            // instance measured a 3/60 (5%) residual race with the
            // start_gate fix alone, and 0/150 with this 25ms settle delay
            // added on top. Treat 25ms as a best-effort mitigation, not as
            // sacred or safely deletable — re-measure against a real server
            // before changing it.
            cancel_settle: Duration::from_millis(25),
            abandon_cancel_timeout: Duration::from_secs(1),
            cancel_escalate: Duration::from_secs(1),
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
    cancel_settle: Duration,
    abandon_cancel_timeout: Duration,
    cancel_escalate: Duration,
    abandon_stats: Arc<AbandonStats>,
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
            cancel_settle: options.cancel_settle,
            abandon_cancel_timeout: options.abandon_cancel_timeout,
            cancel_escalate: options.cancel_escalate,
            abandon_stats: Arc::new(AbandonStats::default()),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Exists for test observability into the abandon/cancel-escalation
    /// path (`drain_abandoned`) so a regression test can assert on the
    /// actual defect -- cancel delay, give-up count -- instead of only an
    /// end-to-end timeout. Not surfaced in the UI.
    pub fn abandon_stats(&self) -> Arc<AbandonStats> {
        Arc::clone(&self.abandon_stats)
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

        let simple_stream = self
            .client
            .simple_query_raw(sql)
            .await
            .map_err(|source| map_query_error(sql, source))?;

        let map_sql = sql.to_string();
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
                cancel_settle: self.cancel_settle,
                abandon_cancel_timeout: self.abandon_cancel_timeout,
                cancel_escalate: self.cancel_escalate,
                stats: Arc::clone(&self.abandon_stats),
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
            Ok(Ok(())) => {
                tokio::time::sleep(self.cancel_settle).await;
                Ok(())
            }
            Ok(Err(source)) => Err(DataSourceError::CancelFailed {
                name: self.name.clone(),
                source,
            }),
            Err(_) => {
                // A timeout here means the cancel_query future was dropped
                // mid-flight, not that sending definitely failed — the
                // cancel packet may already be in transit. That makes this
                // arguably higher-risk for late delivery than the success
                // path above, so it gets the same settle delay.
                tokio::time::sleep(self.cancel_settle).await;
                Err(DataSourceError::CancelTimedOut {
                    name: self.name.clone(),
                    timeout: self.cancel_timeout,
                })
            }
        }
    }
}

const ABANDON_MAX_CANCEL_ATTEMPTS: u32 = 3;

/// Observability for the abandon/cancel-escalation path in
/// `drain_abandoned`: exists specifically so a test can assert on the actual
/// defect instead of only observing an end-to-end timeout. Not surfaced in
/// the UI.
///
/// The delay that matters is the FIRST attempt's. The original defect
/// (tokio-postgres's stream starving `timeout(grace, &mut drain)`'s `Sleep`)
/// manifests as attempt 1 firing hundreds of ms to seconds after
/// `abandon_grace`. Attempts 2+ legitimately fire ~`cancel_escalate` apart
/// by design, so mixing them into one "last delay" number makes a regression
/// of the original bug indistinguishable from the escalation loop working
/// correctly -- which is precisely what made the first regression test flaky.
#[derive(Default)]
pub struct AbandonStats {
    abandons: std::sync::atomic::AtomicU64,
    /// Abandons that got as far as actually issuing their first
    /// cancel_query -- i.e. the drain outlived abandon_grace AND the
    /// active_id recheck said a cancel was still warranted.
    first_cancels: std::sync::atomic::AtomicU64,
    /// Delay from abandon to the first cancel attempt of the most recent
    /// abandon that issued one. Never written by attempts 2+.
    first_cancel_delay_ms: std::sync::atomic::AtomicU64,
    /// Abandons whose drain outlived the first cancel + cancel_escalate.
    /// Expected to be non-zero for very large abandoned result sets; a
    /// diagnostic to watch, not a failure condition.
    multi_attempt_abandons: std::sync::atomic::AtomicU64,
    cancel_send_failures: std::sync::atomic::AtomicU64,
    cancel_timeouts: std::sync::atomic::AtomicU64,
    cancel_gave_up: std::sync::atomic::AtomicU64,
}

impl AbandonStats {
    pub fn abandons(&self) -> u64 {
        self.abandons.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn first_cancels(&self) -> u64 {
        self.first_cancels
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn first_cancel_delay(&self) -> Duration {
        Duration::from_millis(
            self.first_cancel_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    pub fn multi_attempt_abandons(&self) -> u64 {
        self.multi_attempt_abandons
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn cancel_send_failures(&self) -> u64 {
        self.cancel_send_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn cancel_timeouts(&self) -> u64 {
        self.cancel_timeouts
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn cancel_gave_up(&self) -> u64 {
        self.cancel_gave_up
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(crate) struct AbandonCtx {
    pub(crate) cancel_token: tokio_postgres::CancelToken,
    pub(crate) start_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) grace: Duration,
    pub(crate) cancel_settle: Duration,
    pub(crate) abandon_cancel_timeout: Duration,
    pub(crate) cancel_escalate: Duration,
    pub(crate) stats: Arc<AbandonStats>,
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
            // tokio-postgres's stream has no tokio coop integration and serves
            // thousands of rows out of one buffered BackendMessages batch, so a
            // tight drain loop stays inside a single poll() for seconds and
            // starves the timeout below. Measured firing up to 6.9s late
            // instead of the 100ms grace against a real Postgres -- that
            // starvation, not an unreliable cancel_query, is what made this
            // path look flaky. Re-measure before removing.
            tokio::task::coop::consume_budget().await;
        }
    };
    let abandoned_at = tokio::time::Instant::now();
    ctx.stats.abandons.fetch_add(1, Ordering::Relaxed);
    tokio::pin!(drain);

    // Cancel escalation is bounded (a few seconds worst case), but the DRAIN
    // ITSELF is never bounded or abandoned early: releasing the permit while
    // the server might still be streaming would let the next query silently
    // read the abandoned query's leftover rows as its own results, which is
    // worse than a slow permit. There is no MVP0 reconnect story, so giving up
    // on a stuck connection isn't a safe option -- only the cancel retry is
    // bounded, not the fallback natural drain.
    if !allow_cancel {
        drain.await;
    } else {
        let mut attempt = 0u32;
        let mut wait = ctx.grace;
        loop {
            if tokio::time::timeout(wait, &mut drain).await.is_ok() {
                break;
            }
            attempt += 1;
            if attempt == 2 {
                // The drain outlived attempt 1 + cancel_escalate. Counted
                // whether or not attempt 2 ends up sending anything: the
                // signal is "one cancel wasn't enough", independent of the
                // active_id recheck.
                ctx.stats
                    .multi_attempt_abandons
                    .fetch_add(1, Ordering::Relaxed);
            }

            let sent = {
                let _gate = ctx.start_gate.lock().await;
                if active_id.load(Ordering::Acquire) != query_id.0 {
                    None
                } else {
                    if attempt == 1 {
                        // Recorded here, not above: only for attempt 1
                        // (attempts 2+ are the escalation design, not the
                        // defect this guards), and only once we're actually
                        // committed to sending. Measured after the gate
                        // acquisition on purpose: gate wait is part of how
                        // late the cancel really goes out.
                        ctx.stats.first_cancels.fetch_add(1, Ordering::Relaxed);
                        ctx.stats
                            .first_cancel_delay_ms
                            .store(abandoned_at.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
                    Some(
                        tokio::time::timeout(
                            ctx.abandon_cancel_timeout,
                            ctx.cancel_token.cancel_query(tokio_postgres::NoTls),
                        )
                        .await,
                    )
                }
            };

            match sent {
                None => {
                    drain.await;
                    break;
                }
                Some(Ok(Ok(()))) => tokio::time::sleep(ctx.cancel_settle).await,
                // Same asymmetry as PostgresDataSource::cancel: a timeout
                // means the cancel packet may already be in transit, so it
                // gets the settle delay too — only an explicit send failure
                // (definitely nothing in flight) skips it.
                Some(Ok(Err(_))) => {
                    ctx.stats
                        .cancel_send_failures
                        .fetch_add(1, Ordering::Relaxed);
                }
                Some(Err(_)) => {
                    ctx.stats.cancel_timeouts.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(ctx.cancel_settle).await;
                }
            }

            if attempt >= ABANDON_MAX_CANCEL_ATTEMPTS {
                ctx.stats.cancel_gave_up.fetch_add(1, Ordering::Relaxed);
                drain.await;
                break;
            }
            wait = ctx.cancel_escalate;
        }
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

pub fn select_page_sql(schema: &str, table: &str, limit: u64, offset: u64) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {limit} OFFSET {offset}",
        quote_ident(schema),
        quote_ident(table)
    )
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

    #[test]
    fn select_page_sql_quotes_identifiers_needing_quoting_and_composes_limit_offset() {
        let sql = select_page_sql("my schema", "my.table\"name", 51, 100);
        assert_eq!(
            sql,
            "SELECT * FROM \"my schema\".\"my.table\"\"name\" LIMIT 51 OFFSET 100"
        );
    }

    #[test]
    fn select_page_sql_with_plain_identifiers() {
        let sql = select_page_sql("public", "users", 51, 0);
        assert_eq!(sql, "SELECT * FROM \"public\".\"users\" LIMIT 51 OFFSET 0");
    }

    // Synthetic (no real Postgres) concurrency regression test for the
    // start_gate + slot lock-ordering invariant documented on
    // PostgresDataSource::start_gate: every path that issues a request onto
    // the connection (fake_execute below) must hold start_gate across
    // issuance, and cancel (fake_cancel) must hold it across its whole
    // check-and-send, with real dummy async work (yields/sleeps) done while
    // holding the lock so the scheduler has room to interleave badly if the
    // lock ordering were wrong. This can't exercise the actual Postgres
    // cancel race (that requires real server timing, verified manually
    // against Docker — see the cancel_settle comment above), but it does
    // pin the "no deadlock under heavy real concurrent contention" property
    // that the gate design depends on: run on a genuine multi-threaded
    // runtime so tasks can actually run in parallel, not just interleave
    // cooperatively on one thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn start_gate_and_slot_do_not_deadlock_under_concurrent_contention() {
        let slot = Arc::new(Semaphore::new(1));
        let start_gate = Arc::new(tokio::sync::Mutex::new(()));
        let active_id = Arc::new(AtomicU64::new(0));
        let next_id = Arc::new(AtomicU64::new(1));

        async fn fake_execute(
            slot: Arc<Semaphore>,
            start_gate: Arc<tokio::sync::Mutex<()>>,
            active_id: Arc<AtomicU64>,
            next_id: Arc<AtomicU64>,
        ) {
            let Ok(permit) = slot.try_acquire_owned() else {
                return; // connection busy, same as DataSourceError::Busy
            };
            let id = {
                let gate = start_gate.lock().await;
                tokio::task::yield_now().await; // dummy work standing in for simple_query_raw().await
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                active_id.store(id, Ordering::Release);
                drop(gate);
                id
            };
            tokio::task::yield_now().await; // dummy work standing in for streaming rows
            let _ = active_id.compare_exchange(id, 0, Ordering::Release, Ordering::Relaxed);
            drop(permit);
        }

        async fn fake_cancel(
            start_gate: Arc<tokio::sync::Mutex<()>>,
            active_id: Arc<AtomicU64>,
            target_id: u64,
        ) {
            let _gate = start_gate.lock().await;
            if active_id.load(Ordering::Acquire) == target_id {
                tokio::task::yield_now().await; // dummy work standing in for cancel_query().await
            }
        }

        let mut handles = Vec::new();
        for i in 0..200u64 {
            let slot = Arc::clone(&slot);
            let start_gate = Arc::clone(&start_gate);
            let active_id = Arc::clone(&active_id);
            let next_id = Arc::clone(&next_id);
            if i % 2 == 0 {
                handles.push(tokio::spawn(fake_execute(
                    slot, start_gate, active_id, next_id,
                )));
            } else {
                handles.push(tokio::spawn(fake_cancel(start_gate, active_id, i)));
            }
        }

        let joined = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::future::join_all(handles),
        )
        .await
        .expect(
            "200 concurrent fake execute/cancel operations sharing start_gate+slot must \
                 finish well within 5s; a timeout here means the gate deadlocked",
        );
        for result in joined {
            result.expect("no fake_execute/fake_cancel task should panic");
        }
        assert_eq!(
            active_id.load(Ordering::Acquire),
            0,
            "every fake_execute clears active_id on completion, so nothing should be left active"
        );
    }
}
