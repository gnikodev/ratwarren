//! Integration tests for `PostgresDataSource` against a *real* Postgres
//! server. These are skipped by default (`cargo test` stays green without a
//! DB), and only run when `RATWARREN_TEST_PG=1` is set.
//!
//! Spin up a throwaway instance:
//!
//!   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres --name ratwarren-pg postgres:17
//!
//! Then run:
//!
//!   RATWARREN_TEST_PG=1 cargo test --test postgres
//!
//! Connection parameters (all optional, shown with their defaults):
//!   RATWARREN_TEST_PG_HOST=127.0.0.1
//!   RATWARREN_TEST_PG_PORT=5432
//!   RATWARREN_TEST_PG_USER=postgres
//!   RATWARREN_TEST_PG_PASSWORD=postgres
//!   RATWARREN_TEST_PG_DB=postgres
//!
//! Each test creates its own uniquely-named schema (`rwt_<tag>_<pid>_<ts>_<n>`)
//! and drops it with `CASCADE` before returning, so tests don't collide when
//! run concurrently and don't leave residue on a long-lived server. That
//! cleanup runs at the end of the happy path, not from a panic-safe guard —
//! an assertion failure mid-test can leave its schema behind. That's an
//! accepted tradeoff here: the documented setup is a `--rm` throwaway
//! container anyway, so leftover schemas don't outlive the container.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ratwarren::app::message::{WorkerRequest, WorkerResponse};
use ratwarren::app::run::{
    CancelOutcome, CancelRequest, QueryOutcome, QueryRequest, QueryResponse, RunOutcome, RunState,
    RunSummary,
};
use ratwarren::app::worker;
use ratwarren::config::Connection;
use ratwarren::datasource::{
    ConnectOptions, DataSource, DataSourceError, PostgresDataSource, Row, TableKind, quote_ident,
    select_page_sql,
};
use ratwarren::editor::{Motion, RunTarget, RunUnit, TextBuffer, plan_run};
use ratwarren::ui::RequestId;
use ratwarren::ui::grid::page::FETCH_LIMIT;
use ratwarren::ui::tree::message::{TreeRequest, TreeResponse};

fn pg_test_enabled() -> bool {
    std::env::var("RATWARREN_TEST_PG").as_deref() == Ok("1")
}

macro_rules! require_pg {
    () => {
        if !pg_test_enabled() {
            eprintln!(
                "skipping: set RATWARREN_TEST_PG=1 to run this test against a real Postgres \
                 instance (see the doc comment at the top of tests/postgres.rs for a throwaway \
                 docker one-liner and the RATWARREN_TEST_PG_* connection env vars)."
            );
            return;
        }
    };
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn test_connection() -> Connection {
    Connection {
        name: "ratwarren-test".to_string(),
        host: env_or("RATWARREN_TEST_PG_HOST", "127.0.0.1"),
        port: env_or("RATWARREN_TEST_PG_PORT", "5432")
            .parse()
            .expect("RATWARREN_TEST_PG_PORT must be a valid u16"),
        database: env_or("RATWARREN_TEST_PG_DB", "postgres"),
        user: env_or("RATWARREN_TEST_PG_USER", "postgres"),
        password: None,
        tunnel: None,
    }
}

fn test_password() -> String {
    env_or("RATWARREN_TEST_PG_PASSWORD", "postgres")
}

async fn connect() -> PostgresDataSource {
    let conn = test_connection();
    PostgresDataSource::connect(&conn, Some(&test_password()))
        .await
        .expect(
            "connect to the test postgres instance -- see the doc comment at the top of \
             tests/postgres.rs for how to start one",
        )
}

fn unique_schema(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after the unix epoch")
        .as_nanos();
    format!("rwt_{tag}_{}_{}_{n}", std::process::id(), nanos % 1_000_000)
}

async fn exec_to_completion(
    ds: &PostgresDataSource,
    sql: &str,
) -> Result<Vec<Row>, DataSourceError> {
    let mut stream = ds.execute(sql).await?;
    let mut rows = Vec::new();
    while let Some(item) = stream.next().await {
        rows.push(item?);
    }
    Ok(rows)
}

async fn create_schema(ds: &PostgresDataSource, schema: &str) {
    // Defensive: clears out anything left behind by a prior crashed run
    // using the same schema name (only possible on pid/timestamp reuse, but
    // cheap insurance).
    drop_schema(ds, schema).await;
    exec_to_completion(ds, &format!("CREATE SCHEMA {}", quote_ident(schema)))
        .await
        .expect("CREATE SCHEMA should succeed");
}

async fn drop_schema(ds: &PostgresDataSource, schema: &str) {
    exec_to_completion(
        ds,
        &format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(schema)),
    )
    .await
    .expect("DROP SCHEMA IF EXISTS ... CASCADE should succeed");
}

// --- 1. list_schemas ---

#[tokio::test]
async fn list_schemas_contains_public_and_marks_system_schemas() {
    require_pg!();
    let ds = connect().await;

    let schemas = ds
        .list_schemas()
        .await
        .expect("list_schemas should succeed");

    let public = schemas
        .iter()
        .find(|s| s.name == "public")
        .expect("public schema should be present");
    assert!(!public.is_system, "public should not be marked system");

    let pg_catalog = schemas
        .iter()
        .find(|s| s.name == "pg_catalog")
        .expect("pg_catalog schema should be present");
    assert!(pg_catalog.is_system, "pg_catalog should be marked system");

    let information_schema = schemas
        .iter()
        .find(|s| s.name == "information_schema")
        .expect("information_schema should be present");
    assert!(
        information_schema.is_system,
        "information_schema should be marked system"
    );
}

// --- 2. list_tables ---

#[tokio::test]
async fn list_tables_reports_correct_kind_for_each_relation_type() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("tables");
    create_schema(&ds, &schema).await;
    let q = quote_ident(&schema);

    exec_to_completion(&ds, &format!("CREATE TABLE {q}.tbl (id int PRIMARY KEY)"))
        .await
        .expect("create table");
    exec_to_completion(&ds, &format!("CREATE VIEW {q}.vw AS SELECT 1 AS x"))
        .await
        .expect("create view");
    exec_to_completion(
        &ds,
        &format!("CREATE MATERIALIZED VIEW {q}.mvw AS SELECT 1 AS x"),
    )
    .await
    .expect("create materialized view");
    exec_to_completion(
        &ds,
        &format!("CREATE TABLE {q}.ptbl (id int, val int) PARTITION BY RANGE (id)"),
    )
    .await
    .expect("create partitioned table");

    let tables = ds
        .list_tables(&schema)
        .await
        .expect("list_tables should succeed");

    let find = |name: &str| {
        tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("expected a table named {name:?} in {tables:?}"))
    };
    assert_eq!(find("tbl").kind, TableKind::Table);
    assert_eq!(find("vw").kind, TableKind::View);
    assert_eq!(find("mvw").kind, TableKind::MaterializedView);
    assert_eq!(find("ptbl").kind, TableKind::PartitionedTable);

    drop_schema(&ds, &schema).await;
}

// --- 3. list_columns ---

#[tokio::test]
async fn list_columns_reports_ordinal_type_nullability_default_and_primary_key() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("columns");
    create_schema(&ds, &schema).await;
    let q = quote_ident(&schema);

    exec_to_completion(
        &ds,
        &format!(
            "CREATE TABLE {q}.cols (
                a integer NOT NULL DEFAULT 5,
                b varchar(10),
                c text[],
                pk1 integer NOT NULL,
                pk2 integer NOT NULL,
                PRIMARY KEY (pk1, pk2)
            )"
        ),
    )
    .await
    .expect("create table");

    let columns = ds
        .list_columns(&schema, "cols")
        .await
        .expect("list_columns should succeed");

    assert_eq!(columns.len(), 5);
    // ordinal ordering must match attnum ordering (1-indexed, in declaration order).
    for (i, col) in columns.iter().enumerate() {
        assert_eq!(
            col.ordinal,
            (i + 1) as i16,
            "column {} at position {i} should have ordinal {}",
            col.name,
            i + 1
        );
    }

    let find = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("expected column {name:?} in {columns:?}"))
    };

    let a = find("a");
    assert_eq!(a.data_type, "integer");
    assert!(!a.is_nullable, "NOT NULL column should be non-nullable");
    assert_eq!(a.default_expr.as_deref(), Some("5"));
    assert!(!a.is_primary_key);

    let b = find("b");
    assert_eq!(b.data_type, "character varying(10)");
    assert!(b.is_nullable);
    assert_eq!(b.default_expr, None);
    assert!(!b.is_primary_key);

    let c = find("c");
    assert_eq!(c.data_type, "text[]");
    assert!(c.is_nullable);
    assert_eq!(c.default_expr, None);
    assert!(!c.is_primary_key);

    assert!(find("pk1").is_primary_key);
    assert!(find("pk2").is_primary_key);

    drop_schema(&ds, &schema).await;
}

// --- 4. execute() on a SELECT: columns, row order, NULL vs empty string ---

#[tokio::test]
async fn execute_select_preserves_row_order_and_distinguishes_null_from_empty_string() {
    require_pg!();
    let ds = connect().await;

    let mut stream = ds
        .execute("SELECT * FROM (VALUES (1, NULL::text), (2, ''::text)) AS t(n, s) ORDER BY n")
        .await
        .expect("execute should succeed");

    let first = stream
        .next()
        .await
        .expect("expected a first row")
        .expect("no error on first row");
    // Columns are populated by the RowDescription message, which the stream
    // processes before ever yielding a row.
    let columns = stream
        .columns()
        .expect("columns should be populated once a row has been observed")
        .clone();
    let n_idx = first.index_of("n").expect("column n should exist");
    let s_idx = first.index_of("s").expect("column s should exist");
    assert_eq!(first.get(n_idx), Some("1"));
    assert_eq!(
        first.get(s_idx),
        None,
        "SQL NULL should surface as None, not Some(\"\")"
    );

    let second = stream
        .next()
        .await
        .expect("expected a second row")
        .expect("no error on second row");
    assert_eq!(second.get(n_idx), Some("2"));
    assert_eq!(
        second.get(s_idx),
        Some(""),
        "an empty string literal should surface as Some(\"\"), not None"
    );

    assert!(stream.next().await.is_none(), "only two rows were selected");
    assert_eq!(&*columns, &["n".to_string(), "s".to_string()]);
}

// --- 5. execute() on INSERT: bare INSERT vs INSERT ... RETURNING ---

#[tokio::test]
async fn execute_insert_reports_rows_affected_and_returning_reports_columns() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("insert");
    create_schema(&ds, &schema).await;
    let table = format!("{}.ins", quote_ident(&schema));

    exec_to_completion(
        &ds,
        &format!("CREATE TABLE {table} (id serial PRIMARY KEY, val text)"),
    )
    .await
    .expect("create table");

    // Bare INSERT: no RowDescription is ever sent, so columns() stays None.
    let mut bare = ds
        .execute(&format!("INSERT INTO {table} (val) VALUES ('a'), ('b')"))
        .await
        .expect("execute should succeed");
    assert!(bare.next().await.is_none(), "a bare INSERT yields no rows");
    assert_eq!(
        bare.columns(),
        None,
        "a bare INSERT should report no columns"
    );
    assert_eq!(bare.rows_affected(), Some(2));
    // The datasource only allows one in-flight stream at a time; drop this
    // one (already fully drained, so this just releases the permit) before
    // starting the next query below.
    drop(bare);

    // INSERT ... RETURNING: a RowDescription/DataRow pair precedes CommandComplete.
    let mut returning = ds
        .execute(&format!(
            "INSERT INTO {table} (val) VALUES ('c') RETURNING id"
        ))
        .await
        .expect("execute should succeed");
    let row = returning
        .next()
        .await
        .expect("RETURNING should yield a row")
        .expect("no error");
    assert_eq!(row.columns(), &["id".to_string()]);
    assert!(
        row.get(0)
            .expect("id should not be null")
            .parse::<i64>()
            .is_ok(),
        "returned id should parse as an integer"
    );
    assert!(returning.next().await.is_none());
    assert_eq!(returning.rows_affected(), Some(1));
    drop(returning);

    drop_schema(&ds, &schema).await;
}

// --- 6. syntax error surfaces from next(), not execute() ---

#[tokio::test]
async fn syntax_error_surfaces_from_first_next_call_with_an_error_position() {
    require_pg!();
    let ds = connect().await;

    let mut stream = ds
        .execute("SELEKT 1")
        .await
        .expect("execute() itself should return Ok(stream) even for invalid SQL");

    let item = stream
        .next()
        .await
        .expect("the first next() call should yield an item (the error)");
    let err = item.expect_err("the item should be an error for invalid SQL");

    assert!(
        matches!(err, DataSourceError::Query { .. }),
        "expected DataSourceError::Query, got {err:?}"
    );
    assert!(
        err.error_position().is_some(),
        "a syntax error should carry an error position"
    );
}

// --- 7. streaming proof: a 100M-row query must not be buffered ---

#[tokio::test]
async fn execute_streams_rows_without_buffering_and_recovers_after_a_dropped_stream() {
    require_pg!();
    let ds = connect().await;

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = ds
            .execute("SELECT generate_series(1, 100000000)")
            .await
            .expect("execute should succeed");
        let first = stream
            .next()
            .await
            .expect("expected at least one row")
            .expect("no error on first row");
        assert_eq!(first.get(0), Some("1"));
        drop(stream);
    })
    .await;

    assert!(
        outcome.is_ok(),
        "fetching one row of a 100M-row result should complete well within 10s if rows are \
         streamed rather than buffered in full"
    );

    // The abandoned stream must not leave the connection stuck: a follow-up
    // query on the same datasource should still work. Immediately after
    // `drop(stream)` returns, the RowStream's background drain/cancel task
    // has only just been spawned and may not have run yet (no `.await` in
    // this test yields to the scheduler in between), so the very next
    // `execute()` can legitimately observe a transient `Busy` while that
    // task finishes releasing the slot -- that's the fix working as
    // intended (fail fast) rather than the old bug (hang forever). Retry
    // past that transient window instead of asserting success on the first
    // attempt.
    let follow_up = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ds.execute("SELECT 1").await {
                Ok(mut stream) => {
                    break stream
                        .next()
                        .await
                        .expect("expected a row")
                        .expect("no error");
                }
                Err(DataSourceError::Busy { .. }) => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(other) => panic!("unexpected error from follow-up execute: {other:?}"),
            }
        }
    })
    .await
    .expect("follow-up query should complete promptly, proving the connection recovered");

    assert_eq!(follow_up.get(0), Some("1"));
}

// --- 8. cancel races an in-flight query ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_an_in_flight_query() {
    require_pg!();
    let ds = Arc::new(connect().await);

    let mut stream = ds
        .execute("SELECT pg_sleep(30)")
        .await
        .expect("execute should succeed");
    let query_id = stream.query_id();

    let canceler = {
        let ds = Arc::clone(&ds);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ds.cancel(query_id).await
        })
    };

    let result = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
    let cancel_result = canceler.await.expect("the canceling task should not panic");
    cancel_result.expect("cancel() should succeed");

    let item = result.expect("stream.next() should resolve well before pg_sleep(30) finishes");
    match item {
        Some(Err(DataSourceError::Cancelled)) => {}
        other => panic!("expected Some(Err(Cancelled)), got {other:?}"),
    }
}

// --- 8b. mid-query connection death must not hang fetch_page forever ---
//
// Regression test for the finish()-Aborted-branch infinite loop: once
// Postgres kills the backend mid-query, tokio_postgres's underlying response
// stream is not fused and yields Err on every subsequent poll forever (never
// Pending, never None). `worker::fetch_page` -> `RowStream::finish()` must
// still return promptly instead of spinning at 100% CPU. `pg_terminate_backend`
// targeting a backend obtained from a separate connection is reliable enough
// in practice for this to be a permanent regression test rather than a
// manual-only check, but it's still wrapped in a generous timeout so any
// regression fails the test suite loudly instead of hanging it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_page_returns_promptly_when_the_connection_is_killed_mid_query() {
    require_pg!();
    let ds = connect().await;
    let killer = connect().await;

    let pid_row = exec_to_completion(&ds, "SELECT pg_backend_pid()")
        .await
        .expect("pg_backend_pid should succeed")
        .into_iter()
        .next()
        .expect("expected exactly one row");
    let pid: i32 = pid_row
        .get(0)
        .expect("pg_backend_pid column should be present")
        .parse()
        .expect("pg_backend_pid should return an integer");

    let fetch = tokio::spawn(async move { worker::fetch_page(&ds, "SELECT pg_sleep(30)").await });

    // Give fetch_page's execute() time to actually issue the query before
    // terminating the backend, so the kill lands mid-query rather than
    // racing the connection setup.
    tokio::time::sleep(Duration::from_millis(200)).await;
    exec_to_completion(&killer, &format!("SELECT pg_terminate_backend({pid})"))
        .await
        .expect("pg_terminate_backend should succeed");

    let result = tokio::time::timeout(Duration::from_secs(5), fetch)
        .await
        .expect(
            "fetch_page must return within 5s of the backend being terminated mid-query, not \
             hang forever spinning on a not-fused, always-Err stream",
        )
        .expect("the fetch_page task should not panic");

    assert!(
        result.is_err(),
        "fetch_page against a killed backend should surface a connection error, got {result:?}"
    );
}

// --- 9. cancel with a stale query id is a no-op ---

#[tokio::test]
async fn cancel_with_a_stale_query_id_is_a_no_op_and_does_not_affect_later_queries() {
    require_pg!();
    let ds = connect().await;

    let mut stream = ds
        .execute("SELECT 1")
        .await
        .expect("execute should succeed");
    let stale_id = stream.query_id();
    let row = stream
        .next()
        .await
        .expect("expected a row")
        .expect("no error");
    assert_eq!(row.get(0), Some("1"));
    assert!(stream.next().await.is_none(), "fully drain the stream");
    drop(stream);

    // active_id has moved on (back to 0) by the time this fires, so this
    // must be a no-op rather than an error.
    let result = ds.cancel(stale_id).await;
    assert!(
        result.is_ok(),
        "cancelling a query id that is no longer active should be a no-op, got {result:?}"
    );

    // An unrelated, subsequent query on the same datasource must be unaffected.
    let mut stream2 = ds
        .execute("SELECT 2")
        .await
        .expect("execute should succeed");
    let row2 = stream2
        .next()
        .await
        .expect("expected a row")
        .expect("no error");
    assert_eq!(row2.get(0), Some("2"));
}

// --- 10. Busy while a stream from a concurrent execute() is held open ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tables_reports_busy_while_a_concurrent_stream_is_held_open() {
    require_pg!();
    let ds = Arc::new(connect().await);

    let holder = {
        let ds = Arc::clone(&ds);
        tokio::spawn(async move {
            let mut stream = ds
                .execute("SELECT pg_sleep(1)")
                .await
                .expect("execute should succeed");
            // Hold the stream open (don't drain it) long enough for the
            // concurrent list_tables() call below to race against it, then
            // drain it so the datasource is usable again afterward.
            tokio::time::sleep(Duration::from_secs(1)).await;
            stream
                .next()
                .await
                .expect("expected a row from pg_sleep")
                .expect("no error");
            assert!(stream.next().await.is_none());
        })
    };

    // Give the holder task a moment to acquire the datasource's single slot
    // before racing against it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = ds.list_tables("public").await;
    assert!(
        matches!(result, Err(DataSourceError::Busy { .. })),
        "list_tables should observe Busy while another query's stream is held open, got {result:?}"
    );

    holder.await.expect("holder task should not panic");

    // Once the holder has drained and dropped its stream, the datasource
    // should be usable again.
    let tables = ds.list_tables("public").await;
    assert!(
        tables.is_ok(),
        "the datasource should recover once the concurrent stream is released, got {tables:?}"
    );
}

// --- 11. explain() ---

#[tokio::test]
async fn explain_returns_a_plan_containing_a_cost() {
    require_pg!();
    let ds = connect().await;

    let plan = ds
        .explain("SELECT 1")
        .await
        .expect("explain should succeed");
    assert!(
        plan.contains("cost="),
        "expected the EXPLAIN output to contain a cost estimate, got: {plan}"
    );
}

// --- 12. explain() must not allow statement injection ---

#[tokio::test]
async fn explain_rejects_multi_statement_injection_and_the_injected_statement_never_runs() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("explaininj");
    create_schema(&ds, &schema).await;

    let injection = format!(
        "SELECT 1; CREATE TABLE {}.t_should_not_exist(i int)",
        quote_ident(&schema)
    );
    let result = ds.explain(&injection).await;
    assert!(
        result.is_err(),
        "explain() must reject a multi-statement payload, got {result:?}"
    );

    // schema is generated by this test's own naming scheme (alnum/underscore
    // only), so interpolating it unquoted into a literal here is safe.
    let check_sql = format!("SELECT to_regclass('{schema}.t_should_not_exist') IS NULL AS missing");
    let rows = exec_to_completion(&ds, &check_sql)
        .await
        .expect("the sanity-check query itself should succeed");
    assert_eq!(
        rows[0].get(0),
        Some("t"),
        "the injected CREATE TABLE must not have run"
    );

    drop_schema(&ds, &schema).await;
}

// --- 13. connect() surfaces a Connect error for an unreachable port ---

#[tokio::test]
async fn connect_to_an_unreachable_port_returns_a_connect_error() {
    require_pg!();
    let mut conn = test_connection();
    conn.port = 1; // privileged port; nothing should be listening here.

    let options = ConnectOptions {
        connect_timeout: Duration::from_secs(2),
        ..ConnectOptions::default()
    };
    let result = PostgresDataSource::connect_with(&conn, Some(&test_password()), &options).await;

    match result {
        Err(DataSourceError::Connect { .. }) => {}
        Err(other) => panic!("expected DataSourceError::Connect, got a different error: {other:?}"),
        Ok(_) => panic!("expected DataSourceError::Connect, got Ok(_)"),
    }
}

// --- 14+. worker::fetch_page: pagination boundaries and the drain-before-drop
// invariant against a real server's simple-query wire protocol ---

async fn create_numbered_table(ds: &PostgresDataSource, schema: &str, n: u64) {
    let q = quote_ident(schema);
    exec_to_completion(ds, &format!("CREATE TABLE {q}.t (n int)"))
        .await
        .expect("create table");
    if n > 0 {
        exec_to_completion(
            ds,
            &format!("INSERT INTO {q}.t SELECT * FROM generate_series(1, {n})"),
        )
        .await
        .expect("insert rows");
    }
}

#[tokio::test]
async fn fetch_page_drains_before_returning_so_the_connection_is_immediately_reusable() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetchdrain");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 60).await;

    let sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let page = worker::fetch_page(&ds, &sql)
        .await
        .expect("fetch_page should succeed");
    assert_eq!(page.rows.len(), 50);
    assert!(page.has_next);

    // Deliberately no sleep/yield between fetch_page returning and the next
    // request: the property under test is that fetch_page's drain loop
    // releases the connection's single permit synchronously, before it
    // returns control to the caller -- not eventually, via a background
    // drain task racing this very call. If the drain-before-drop fix
    // regressed (e.g. the stream were dropped before being fully drained),
    // this call would observe DataSourceError::Busy instead.
    let result = ds.list_tables(&schema).await;
    assert!(
        result.is_ok(),
        "list_tables should succeed immediately after fetch_page returns, proving the \
         connection's permit was released synchronously rather than deferred to a background \
         drain task -- got {result:?}"
    );

    drop_schema(&ds, &schema).await;
}

#[tokio::test]
async fn fetch_page_at_exactly_fifty_rows_returns_all_rows_with_no_next_page() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetch50");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 50).await;

    let sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let page = worker::fetch_page(&ds, &sql)
        .await
        .expect("fetch_page should succeed");
    assert_eq!(page.rows.len(), 50);
    assert!(!page.has_next);

    drop_schema(&ds, &schema).await;
}

#[tokio::test]
async fn fetch_page_at_exactly_fifty_one_rows_truncates_the_fifty_first_row_and_reports_has_next() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetch51");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 51).await;

    let sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let page = worker::fetch_page(&ds, &sql)
        .await
        .expect("fetch_page should succeed");
    assert_eq!(
        page.rows.len(),
        50,
        "the 51st row must be consumed only to decide has_next, never rendered/returned"
    );
    assert!(page.has_next);

    drop_schema(&ds, &schema).await;
}

#[tokio::test]
async fn fetch_page_on_an_empty_table_returns_no_rows_but_populated_columns() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetch0");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 0).await;

    let sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let page = worker::fetch_page(&ds, &sql)
        .await
        .expect("fetch_page should succeed");
    assert!(page.rows.is_empty());
    assert!(
        !page.columns.is_empty(),
        "RowDescription arrives even for a zero-row result, so columns should still be populated"
    );
    assert_eq!(page.columns, vec!["n".to_string()]);
    assert!(!page.has_next);

    drop_schema(&ds, &schema).await;
}

#[tokio::test]
async fn fetch_page_second_page_returns_the_remaining_rows_with_no_further_next_page() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetchpage2");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 60).await;

    let first_sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let first = worker::fetch_page(&ds, &first_sql)
        .await
        .expect("fetch_page should succeed for the first page");
    assert!(first.has_next);

    let second_sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 50);
    let second = worker::fetch_page(&ds, &second_sql)
        .await
        .expect("fetch_page should succeed for the second page");
    assert_eq!(second.rows.len(), 10);
    assert!(!second.has_next);

    drop_schema(&ds, &schema).await;
}

#[tokio::test]
async fn fetch_page_distinguishes_null_from_empty_string_end_to_end() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("fetchnull");
    create_schema(&ds, &schema).await;
    let q = quote_ident(&schema);

    exec_to_completion(&ds, &format!("CREATE TABLE {q}.t (id int, s text)"))
        .await
        .expect("create table");
    exec_to_completion(
        &ds,
        &format!("INSERT INTO {q}.t (id, s) VALUES (1, NULL), (2, '')"),
    )
    .await
    .expect("insert rows");

    let sql = select_page_sql(&schema, "t", FETCH_LIMIT as u64, 0);
    let page = worker::fetch_page(&ds, &sql)
        .await
        .expect("fetch_page should succeed");
    assert_eq!(page.rows.len(), 2);

    let id_idx = page
        .columns
        .iter()
        .position(|c| c == "id")
        .expect("id column should be present");
    let s_idx = page
        .columns
        .iter()
        .position(|c| c == "s")
        .expect("s column should be present");

    let row_for = |id: &str| {
        page.rows
            .iter()
            .find(|r| r[id_idx].as_deref() == Some(id))
            .unwrap_or_else(|| panic!("expected a row with id={id} in {:?}", page.rows))
    };

    assert_eq!(
        row_for("1")[s_idx],
        None,
        "SQL NULL should round-trip through fetch_page/Page::from_fetched as None"
    );
    assert_eq!(
        row_for("2")[s_idx],
        Some(String::new()),
        "an empty string literal should round-trip through fetch_page/Page::from_fetched as \
         Some(\"\"), not None"
    );

    drop_schema(&ds, &schema).await;
}

// Regression test for a bug where fetch_page skipped draining the RowStream
// when the query itself errored server-side (e.g. division by zero), which
// left the connection permit stuck in a deferred-release (background drain
// task) state and caused the very next request on the same connection to
// spuriously fail with Busy -- destroying the real error message the user
// should have seen.
//
// Note on coverage: a syntax error (e.g. "SELEKT 1", see
// `syntax_error_surfaces_from_first_next_call_with_an_error_position` above)
// exercises the *same* fetch_page/RowStream code path as a runtime error
// like `SELECT 1/0` -- `execute()` returns `Ok(stream)` in both cases (simple
// query protocol reports errors as a message on the stream, not as a
// synchronous failure from `simple_query_raw`), and the failure only
// surfaces once `stream.take()` polls the stream, aborting it. There isn't a
// reachable case where `source.execute(sql).await?` itself fails with a SQL
// problem before a stream exists (the only way `execute()` fails
// synchronously is `Busy`, from a permit already held by a concurrent
// caller, which is a different scenario already covered by
// `list_tables_reports_busy_while_a_concurrent_stream_is_held_open` above).
// So a single runtime-error case here is representative; duplicating it with
// a syntax-error variant would just restate the same code path.
#[tokio::test]
async fn fetch_page_error_does_not_leave_the_connection_busy_for_the_next_request() {
    require_pg!();
    let ds = connect().await;

    let result = worker::fetch_page(&ds, "SELECT 1/0").await;
    match &result {
        Err(DataSourceError::Query { source, .. }) => {
            // tokio_postgres::Error's own Display is just "db error" for a
            // server-reported failure; the real message lives on the
            // wrapped DbError (same accessor `error_position()` uses).
            let db_error = source
                .as_db_error()
                .expect("a division-by-zero failure should be a DbError");
            assert!(
                db_error.message().contains("division by zero"),
                "expected the real division-by-zero error to surface, got: {}",
                db_error.message()
            );
        }
        other => panic!(
            "expected DataSourceError::Query wrapping the division-by-zero error, got {other:?}"
        ),
    }

    // Deliberately no sleep/yield between fetch_page returning and this next
    // call: the property under test is that fetch_page's `stream.finish()`
    // call drains and releases the connection's single permit synchronously,
    // within fetch_page's own return -- not eventually, via a background
    // drain task racing this very call.
    let schemas = ds.list_schemas().await;
    assert!(
        !matches!(schemas, Err(DataSourceError::Busy { .. })),
        "list_schemas should not observe Busy immediately after a failed fetch_page call -- \
         that would mean the connection permit was not released synchronously, got {schemas:?}"
    );
    assert!(
        schemas.is_ok(),
        "list_schemas should succeed immediately after a failed fetch_page call, got {schemas:?}"
    );
}

// --- 15+. worker::spawn's WorkerRequest::Query path (Phase 7 execution
// wiring): the finish()-vs-drop() branch in `handle_query` for genuinely
// unbounded editor SQL, and retry_on_busy recovering from the abandoned-
// stream drain that this branch hands off to.

fn spawn_worker(
    source: Arc<dyn DataSource>,
) -> (
    tokio::sync::mpsc::UnboundedSender<WorkerRequest>,
    tokio::sync::mpsc::UnboundedReceiver<WorkerResponse>,
    tokio::task::JoinHandle<()>,
) {
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = worker::spawn(source, request_rx, response_tx);
    (request_tx, response_rx, handle)
}

async fn run_query(
    request_tx: &tokio::sync::mpsc::UnboundedSender<WorkerRequest>,
    response_rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerResponse>,
    id: RequestId,
    sql: &str,
    timeout: Duration,
) -> Result<QueryOutcome, DataSourceError> {
    request_tx
        .send(WorkerRequest::Query(QueryRequest {
            id,
            sql: sql.to_string(),
        }))
        .expect("worker task should still be alive to receive the request");

    loop {
        let response = tokio::time::timeout(timeout, response_rx.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("worker did not respond to query {id:?} ({sql:?}) within {timeout:?}")
            })
            .expect("worker response channel should not close mid-test");
        if let WorkerResponse::Query(QueryResponse::Finished { id: got_id, result }) = response
            && got_id == id
        {
            return result;
        }
        // Ignore Started{..} for this id and any stray response for an
        // unrelated id (there shouldn't be one in these single-request-at-a-
        // time tests, but ignoring rather than asserting keeps this helper
        // reusable).
    }
}

// The core Phase 7 correctness rule under adversarial conditions: a query
// with NO LIMIT that returns far more than FETCH_LIMIT rows must not freeze
// the worker task. If `handle_query` mistakenly called `stream.finish()`
// after `take(FETCH_LIMIT)` returned exactly FETCH_LIMIT rows, this test
// would observe the first response taking as long as it takes Postgres to
// transmit and tokio-postgres to decode all `BIG_N` rows. Verified by hand
// against this exact test, temporarily replacing the `drop(stream)` branch
// in `handle_query` with `stream.finish().await`: against a local Docker
// Postgres, `BIG_N = 200_000` was NOT enough to distinguish the two (both
// finished in well under a second -- decoding that many one-column rows is
// simply too fast locally to notice), but `BIG_N = 5_000_000` reliably took
// ~3.5s with `finish()` and reliably failed the `elapsed < 2s` assertion
// below, versus comfortably under the threshold with the real `drop()` fix.
const BIG_N: u64 = 5_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_worker_does_not_block_on_a_result_set_far_larger_than_fetch_limit() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("bigquery");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, BIG_N).await;
    let q = quote_ident(&schema);

    let source: Arc<dyn DataSource> = Arc::new(ds);
    let (request_tx, mut response_rx, worker_handle) = spawn_worker(Arc::clone(&source));

    let start = std::time::Instant::now();
    let outcome = run_query(
        &request_tx,
        &mut response_rx,
        RequestId(1),
        &format!("SELECT * FROM {q}.t"),
        Duration::from_secs(5),
    )
    .await
    .expect("the unbounded SELECT should succeed");
    let elapsed = start.elapsed();

    match outcome {
        QueryOutcome::Rows(page) => {
            assert_eq!(page.rows.len(), 50);
            assert!(page.has_next);
        }
        other => panic!("expected QueryOutcome::Rows with has_next, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "handle_query took {elapsed:?} to return the first page of a {BIG_N}-row unbounded \
         query -- it must return almost immediately after FETCH_LIMIT rows, handing the \
         remainder off to the background drain path, not block draining everything \
         synchronously via finish()"
    );

    // The abandoned stream's background drain now holds the connection's
    // single permit while it discards the remaining ~BIG_N-50 rows. A second,
    // unrelated query issued immediately afterward must still eventually
    // succeed via `retry_on_busy` rather than getting stuck as a permanent
    // Busy error.
    let second = run_query(
        &request_tx,
        &mut response_rx,
        RequestId(2),
        "SELECT 1",
        Duration::from_secs(10),
    )
    .await
    .expect(
        "the follow-up query must eventually succeed once the background drain releases \
             the connection's permit, via retry_on_busy",
    );
    match second {
        QueryOutcome::Rows(page) => assert_eq!(page.rows.len(), 1),
        other => panic!("expected the follow-up SELECT 1 to return one row, got {other:?}"),
    }

    drop(request_tx);
    worker_handle.abort();
    let _ = worker_handle.await;
    drop(source);

    let cleanup = connect().await;
    drop_schema(&cleanup, &schema).await;
}

// The `rows.len() < FETCH_LIMIT` branch ("provably Ended"): a plain,
// unbounded SELECT whose real result is smaller than FETCH_LIMIT must still
// go through `stream.finish()` and leave the connection immediately usable
// -- same property `fetch_page_drains_before_returning_...` proves for the
// LIMIT-bounded grid path, now proven for the unbounded editor-SQL path.
#[tokio::test]
async fn query_worker_at_fewer_than_fetch_limit_rows_drains_and_frees_the_connection() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("smallquery");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 10).await;
    let q = quote_ident(&schema);

    let source: Arc<dyn DataSource> = Arc::new(ds);
    let (request_tx, mut response_rx, worker_handle) = spawn_worker(Arc::clone(&source));

    let outcome = run_query(
        &request_tx,
        &mut response_rx,
        RequestId(1),
        &format!("SELECT * FROM {q}.t"),
        Duration::from_secs(5),
    )
    .await
    .expect("the unbounded SELECT should succeed");
    match outcome {
        QueryOutcome::Rows(page) => {
            assert_eq!(page.rows.len(), 10);
            assert!(!page.has_next);
        }
        other => panic!("expected QueryOutcome::Rows with no next page, got {other:?}"),
    }

    // Immediately follow up (no sleep/yield): a second query must succeed
    // right away, not merely "eventually" via retry_on_busy, proving
    // `finish()` released the permit synchronously in this branch.
    let second = run_query(
        &request_tx,
        &mut response_rx,
        RequestId(2),
        "SELECT 1",
        Duration::from_millis(500),
    )
    .await
    .expect("a query issued immediately after a <FETCH_LIMIT result should not need retrying");
    assert!(matches!(second, QueryOutcome::Rows(page) if page.rows.len() == 1));

    drop(request_tx);
    worker_handle.abort();
    let _ = worker_handle.await;
    drop(source);

    let cleanup = connect().await;
    drop_schema(&cleanup, &schema).await;
}

// DML with no RowDescription (no SELECT list) must surface as
// `QueryOutcome::NoResultSet`, distinct from a `Rows` outcome with an empty
// page -- the two are visually and semantically different (e.g. "0 rows
// affected" vs. "an empty table").
#[tokio::test]
async fn query_worker_reports_no_result_set_for_a_zero_row_update() {
    require_pg!();
    let ds = connect().await;
    let schema = unique_schema("dmlquery");
    create_schema(&ds, &schema).await;
    create_numbered_table(&ds, &schema, 5).await;
    let q = quote_ident(&schema);

    let source: Arc<dyn DataSource> = Arc::new(ds);
    let (request_tx, mut response_rx, worker_handle) = spawn_worker(Arc::clone(&source));

    let outcome = run_query(
        &request_tx,
        &mut response_rx,
        RequestId(1),
        &format!("UPDATE {q}.t SET n = n WHERE false"),
        Duration::from_secs(5),
    )
    .await
    .expect("the UPDATE should succeed");
    assert_eq!(outcome, QueryOutcome::NoResultSet { rows_affected: 0 });

    drop(request_tx);
    worker_handle.abort();
    let _ = worker_handle.await;
    drop(source);

    let cleanup = connect().await;
    drop_schema(&cleanup, &schema).await;
}

// --- 18+. Full run-statement path (Phase 7's explicit test criterion):
// cursor statement, selection, whole buffer -- driven through the REAL
// `editor::plan_run` split, the REAL `RunState` sequencer, and the REAL
// `worker::spawn`/`spawn_canceller` + channels, not a hand-rolled
// reimplementation of any of those three.

type RequestSender = tokio::sync::mpsc::UnboundedSender<WorkerRequest>;
type ResponseReceiver = tokio::sync::mpsc::UnboundedReceiver<WorkerResponse>;
type CancelSender = tokio::sync::mpsc::UnboundedSender<CancelRequest>;

struct FullWorker {
    request_tx: RequestSender,
    response_rx: ResponseReceiver,
    cancel_tx: CancelSender,
    worker_handle: tokio::task::JoinHandle<()>,
    canceller_handle: tokio::task::JoinHandle<()>,
}

fn spawn_full_worker(source: Arc<dyn DataSource>) -> FullWorker {
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker_handle = worker::spawn(Arc::clone(&source), request_rx, response_tx.clone());
    let canceller_handle = worker::spawn_canceller(source, cancel_rx, response_tx);
    FullWorker {
        request_tx,
        response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    }
}

async fn shutdown_full_worker(
    request_tx: RequestSender,
    cancel_tx: CancelSender,
    worker_handle: tokio::task::JoinHandle<()>,
    canceller_handle: tokio::task::JoinHandle<()>,
) {
    drop(request_tx);
    drop(cancel_tx);
    worker_handle.abort();
    let _ = worker_handle.await;
    canceller_handle.abort();
    let _ = canceller_handle.await;
}

/// Drives one full run (as `App::start_run`/`App::apply_query_response`
/// would) to completion via the real `RunState` state machine and the real
/// worker channels: sends the first `QueryRequest`, then for every response
/// either fires a deferred cancel (`on_started`), advances to the next
/// statement, or returns once the run is `Done`. Returns every statement's
/// result in order plus the final summary (`None` only if `plan` was empty,
/// mirroring `RunState::start`).
async fn drive_run(
    request_tx: &tokio::sync::mpsc::UnboundedSender<WorkerRequest>,
    response_rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerResponse>,
    cancel_tx: &tokio::sync::mpsc::UnboundedSender<CancelRequest>,
    plan: Vec<RunUnit>,
    per_response_timeout: Duration,
) -> (
    Vec<Result<QueryOutcome, DataSourceError>>,
    Option<RunSummary>,
) {
    let mut state = RunState::new();
    let mut results = Vec::new();
    let Some(mut req) = state.start(plan) else {
        return (results, None);
    };
    loop {
        request_tx
            .send(WorkerRequest::Query(req))
            .expect("worker task should still be alive to receive the request");
        loop {
            let response = tokio::time::timeout(per_response_timeout, response_rx.recv())
                .await
                .expect("worker should respond within the timeout")
                .expect("worker response channel should not close mid-test");
            let WorkerResponse::Query(q) = response else {
                continue;
            };
            match q {
                QueryResponse::Started { id, query_id } => {
                    if let Some(cancel_req) = state.on_started(id, query_id) {
                        let _ = cancel_tx.send(cancel_req);
                    }
                }
                QueryResponse::Finished { id, result } => {
                    if !state.owns(id) {
                        continue;
                    }
                    let outcome = state
                        .on_finished(id, &result)
                        .expect("owns() was true, so on_finished must not be a no-op");
                    results.push(result);
                    match outcome {
                        RunOutcome::Next(next_req) => {
                            req = next_req;
                            break;
                        }
                        RunOutcome::Done(summary) => return (results, Some(summary)),
                    }
                }
                QueryResponse::CancelFailed { message, .. } => {
                    panic!("unexpected CancelFailed: {message}");
                }
            }
        }
    }
}

async fn assert_no_further_response_arrives(
    response_rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerResponse>,
) {
    // Bounded wait rather than an instantaneous `try_recv`: the worker task
    // needs at least one scheduler tick (and, in these tests, sometimes an
    // actual DB round-trip) to notice there's nothing left to send, so an
    // immediate `try_recv` could spuriously pass even if a bug caused a
    // stray extra response.
    let result = tokio::time::timeout(Duration::from_millis(500), response_rx.recv()).await;
    assert!(
        result.is_err(),
        "no further response should have arrived, but one did"
    );
}

#[tokio::test]
async fn full_run_path_cursor_statement_runs_only_the_statement_under_the_cursor() {
    require_pg!();
    let ds = connect().await;
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
    let mut buf = TextBuffer::from_text(text);
    // Position the cursor inside the middle statement, "SELECT 2".
    buf.move_to(
        ratwarren::editor::Position { line: 1, col: 3 },
        Motion::Move,
    );
    let units = plan_run(&buf, RunTarget::Cursor).expect("cursor sits on a clean statement");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].sql, "SELECT 2");

    let (results, summary) = drive_run(
        &request_tx,
        &mut response_rx,
        &cancel_tx,
        units,
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(results.len(), 1, "only the cursor statement should run");
    match &results[0] {
        Ok(QueryOutcome::Rows(page)) => {
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.rows[0][0].as_deref(), Some("2"));
        }
        other => panic!("expected QueryOutcome::Rows for SELECT 2, got {other:?}"),
    }
    let summary = summary.expect("plan was non-empty");
    assert_eq!(summary.ran, 1);
    assert_eq!(summary.total, 1);
    assert_eq!(summary.cancelled, None);
    assert_eq!(summary.failed, None);

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
}

#[tokio::test]
async fn full_run_path_selection_spanning_two_statements_runs_both_in_order() {
    require_pg!();
    let ds = connect().await;
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
    let mut buf = TextBuffer::from_text(text);
    buf.move_to(
        ratwarren::editor::Position { line: 0, col: 0 },
        Motion::Move,
    );
    // Select through the START of line 2 ("SELECT 3;"): this fully covers
    // statements 1 and 2 while ending exactly at statement 3's sql_span
    // start, which `statements_in`'s exclusive overlap check excludes.
    buf.move_to(
        ratwarren::editor::Position { line: 2, col: 0 },
        Motion::Extend,
    );
    let units = plan_run(&buf, RunTarget::Selection).expect("selection has no tokenizer error");
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].sql, "SELECT 1");
    assert_eq!(units[1].sql, "SELECT 2");

    let (results, summary) = drive_run(
        &request_tx,
        &mut response_rx,
        &cancel_tx,
        units,
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(results.len(), 2, "statement 3 must never run");
    for (expected, result) in ["1", "2"].into_iter().zip(&results) {
        match result {
            Ok(QueryOutcome::Rows(page)) => {
                assert_eq!(page.rows[0][0].as_deref(), Some(expected));
            }
            other => panic!("expected Rows({expected}), got {other:?}"),
        }
    }
    let summary = summary.expect("plan was non-empty");
    assert_eq!(summary.ran, 2);
    assert_eq!(summary.total, 2);
    assert_eq!(summary.cancelled, None);
    assert_eq!(summary.failed, None);
    assert_no_further_response_arrives(&mut response_rx).await;

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
}

#[tokio::test]
async fn full_run_path_whole_buffer_runs_mixed_ddl_dml_select_in_order() {
    require_pg!();
    let ds = connect().await;
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let schema = unique_schema("fullrunbuf");
    {
        let setup = connect().await;
        create_schema(&setup, &schema).await;
    }
    let q = quote_ident(&schema);
    let text = format!(
        "CREATE TABLE {q}.t (i int); INSERT INTO {q}.t VALUES (1),(2),(3); SELECT * FROM {q}.t;"
    );
    let buf = TextBuffer::from_text(&text);
    let units = plan_run(&buf, RunTarget::Buffer).expect("no tokenizer error");
    assert_eq!(units.len(), 3);

    let (results, summary) = drive_run(
        &request_tx,
        &mut response_rx,
        &cancel_tx,
        units,
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(results.len(), 3);
    assert!(
        matches!(
            &results[0],
            Ok(QueryOutcome::NoResultSet { rows_affected: 0 })
        ),
        "CREATE TABLE has no affected-row count, got {:?}",
        results[0]
    );
    assert!(
        matches!(
            &results[1],
            Ok(QueryOutcome::NoResultSet { rows_affected: 3 })
        ),
        "INSERT of 3 rows should report 3 affected, got {:?}",
        results[1]
    );
    match &results[2] {
        Ok(QueryOutcome::Rows(page)) => assert_eq!(page.rows.len(), 3),
        other => panic!("expected the trailing SELECT to return Rows, got {other:?}"),
    }
    let summary = summary.expect("plan was non-empty");
    assert_eq!(summary.ran, 3);
    assert_eq!(summary.total, 3);
    assert_eq!(summary.cancelled, None);
    assert_eq!(summary.failed, None);

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
    let cleanup = connect().await;
    drop_schema(&cleanup, &schema).await;
}

#[tokio::test]
async fn full_run_path_stops_on_the_first_error_and_never_issues_the_third_statement() {
    require_pg!();
    let ds = connect().await;
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let text = "SELECT 1; SELEKT 2; SELECT 3;";
    let buf = TextBuffer::from_text(text);
    // The splitter has no idea "SELEKT" isn't a real keyword -- it tokenizes
    // fine as an ordinary identifier, so all three statements are planned;
    // the syntax error only surfaces once Postgres actually executes it.
    let units = plan_run(&buf, RunTarget::Buffer).expect("splitting doesn't validate SQL syntax");
    assert_eq!(units.len(), 3);

    let (results, summary) = drive_run(
        &request_tx,
        &mut response_rx,
        &cancel_tx,
        units,
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(
        results.len(),
        2,
        "exactly two statements should have been issued: the first (ok) and the second (error)"
    );
    match &results[0] {
        Ok(QueryOutcome::Rows(page)) => assert_eq!(page.rows[0][0].as_deref(), Some("1")),
        other => panic!("expected statement 1 to succeed, got {other:?}"),
    }
    assert!(
        matches!(&results[1], Err(DataSourceError::Query { .. })),
        "expected statement 2 to fail with a query error, got {:?}",
        results[1]
    );
    let summary = summary.expect("plan was non-empty");
    assert_eq!(summary.ran, 2);
    assert_eq!(summary.total, 3);
    assert_eq!(summary.cancelled, None);
    assert!(summary.failed.is_some());

    // The third statement was never sent, so nothing further should ever
    // arrive on the response channel.
    assert_no_further_response_arrives(&mut response_rx).await;

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_run_path_cancel_mid_run_stops_the_run_and_never_issues_the_next_statement() {
    require_pg!();
    let ds = connect().await;
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let text = "SELECT pg_sleep(10); SELECT 1;";
    let buf = TextBuffer::from_text(text);
    let units = plan_run(&buf, RunTarget::Buffer).expect("no tokenizer error");
    assert_eq!(units.len(), 2);

    let mut state = RunState::new();
    let req = state.start(units).expect("plan is non-empty");
    request_tx
        .send(WorkerRequest::Query(req))
        .expect("worker alive");

    // Wait for `Started`, then simulate the user pressing the cancel key:
    // `RunState::request_cancel` returns the `QueryId` immediately since
    // it's already known by this point, exactly like `App::on_key`'s
    // `CancelOrQuit` handling.
    let started = tokio::time::timeout(Duration::from_secs(5), response_rx.recv())
        .await
        .expect("worker should report Started promptly")
        .expect("channel open");
    let WorkerResponse::Query(QueryResponse::Started { id, query_id }) = started else {
        panic!("expected the first response to be Started");
    };
    assert!(
        state.on_started(id, query_id).is_none(),
        "no cancel was requested yet, so on_started must not fire one"
    );
    let cancel_req = state
        .request_cancel()
        .expect("the QueryId is already known, so the cancel must fire immediately");
    cancel_tx.send(cancel_req).expect("canceller task alive");

    let finished = tokio::time::timeout(Duration::from_secs(10), response_rx.recv())
        .await
        .expect("worker should report Finished (cancelled) well before pg_sleep(10) elapses")
        .expect("channel open");
    let WorkerResponse::Query(QueryResponse::Finished {
        id: finished_id,
        result,
    }) = finished
    else {
        panic!("expected the second response to be Finished");
    };
    assert!(
        matches!(result, Err(DataSourceError::Cancelled)),
        "expected the cancelled statement to fail with DataSourceError::Cancelled, got {result:?}"
    );
    let outcome = state
        .on_finished(finished_id, &result)
        .expect("this id belongs to the active run");
    let summary = match outcome {
        RunOutcome::Done(summary) => summary,
        RunOutcome::Next(_) => panic!("a cancelled statement must stop the run, not continue"),
    };
    assert_eq!(summary.cancelled, Some(CancelOutcome::Interrupted));
    assert_eq!(summary.failed, None);
    assert_eq!(summary.ran, 1);
    assert_eq!(summary.total, 2);

    // Statement 2 was never sent, so nothing further should arrive.
    assert_no_further_response_arrives(&mut response_rx).await;

    // The connection must be immediately usable again: a `list_tables`
    // issued through the very same worker/channels must succeed (possibly
    // after `retry_on_busy`'s retry while the cancelled query's connection
    // slot settles), proving no stuck permit from the cancel path.
    let id = RequestId(12345);
    request_tx
        .send(WorkerRequest::Tree(TreeRequest::Tables {
            id,
            schema: "public".to_string(),
        }))
        .expect("worker alive");
    let response = tokio::time::timeout(Duration::from_secs(10), response_rx.recv())
        .await
        .expect("worker should recover and answer list_tables")
        .expect("channel open");
    match response {
        WorkerResponse::Tree(TreeResponse::Tables { result, .. }) => {
            assert!(
                result.is_ok(),
                "list_tables after a mid-run cancel should succeed, got {result:?}"
            );
        }
        WorkerResponse::Query(_) => panic!("expected a Tree(Tables) response, got a Query one"),
        WorkerResponse::Grid(_) => panic!("expected a Tree(Tables) response, got a Grid one"),
        WorkerResponse::Tree(TreeResponse::Schemas { .. } | TreeResponse::Columns { .. }) => {
            panic!("expected a Tree(Tables) response, got a different Tree response")
        }
    }

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
}

// The carried-over Phase 7 regression, looped for confidence: `handle_query`'s
// `finish()`-vs-`drop()` branch must reliably hand an unbounded, only-
// partially-consumed stream off to the background drain/abandon path instead
// of blocking the worker, AND the very next statement in the same run must
// reliably succeed once `retry_on_busy` gets past the transient `Busy` while
// that drain finishes -- at machine-speed back-to-back statement gaps, not
// human-speed.
//
// FIXED (was previously flaky, root-caused and fixed via the coop-yield +
// bounded-cancel-retry pass in `drain_abandoned`): root cause, reconstructed
// from Postgres server logs and confirmed by direct measurement, was that
// `drain_abandoned`'s drain loop (`while let Some(item) = inner.next().await
// { ... }`) never yielded to the scheduler -- tokio-postgres's stream has no
// tokio coop integration and can serve an entire buffered batch of rows per
// channel item, so the loop stayed inside a single `poll()` for up to ~7s on
// a large abandoned result set. That starved `tokio::time::timeout(ctx.grace,
// &mut drain)`'s `Sleep`, which never got polled, so the cancel-escalation
// fired anywhere from 100ms to 6.9s late (or not at all before the drain
// finished naturally) -- NOT a `cancel_query()` reliability problem itself
// (measured 0 failures/timeouts across 150 real `cancel_query()` calls,
// latency 105-220µs every time). The fix adds `tokio::task::coop::
// consume_budget().await` inside the drain loop so the timeout actually gets
// a chance to fire, plus a bounded cancel-retry loop (`AbandonStats` below)
// for observability.
//
// The first version of this test asserted `last_cancel_delay() < 500ms` and
// failed 4/10 runs against real Postgres -- NOT a regression of the coop-
// starvation bug above, but `last_cancel_delay_ms` being overwritten by
// EVERY escalation attempt, so a run that needed a second attempt (a
// legitimate ~1.13s = 100ms abandon_grace + 1s cancel_escalate + 25ms
// cancel_settle) was indistinguishable from a genuine regression of the
// first attempt firing late. Measured across 110+ iterations post-fix
// (this test plus independent review re-runs), `multi_attempt_abandons()`
// was 0 every time -- so a second attempt does not appear to be the actual
// explanation for the original 4/10 failures, and if this test ever shows
// `multi_attempt_abandons() > 0` alongside a large `first_cancel_delay()`,
// treat that as a live anomaly worth investigating, not an expected/benign
// case to relax the bound for. What's certain, independent of that
// explanation, is that mixing attempt-1 and attempt-2+ delays into one
// "last delay" metric made a real regression indistinguishable from normal
// operation -- which is why this test now pins
// `AbandonStats::first_cancel_delay()` (attempt-1 only, the number the
// coop-starvation defect actually corrupted) and reports
// `multi_attempt_abandons()` in failure messages as a diagnostic, without
// asserting on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_run_path_abandon_and_retry_is_reliable_across_repeated_back_to_back_runs() {
    require_pg!();
    let ds = connect().await;
    let stats = ds.abandon_stats();
    let source: Arc<dyn DataSource> = Arc::new(ds);
    let FullWorker {
        request_tx,
        mut response_rx,
        cancel_tx,
        worker_handle,
        canceller_handle,
    } = spawn_full_worker(Arc::clone(&source));

    let text = "SELECT generate_series(1, 100000000); SELECT 1;";
    let buf = TextBuffer::from_text(text);
    let units = plan_run(&buf, RunTarget::Buffer).expect("no tokenizer error");
    assert_eq!(units.len(), 2);

    let mut first_delays: Vec<Duration> = Vec::new();

    for i in 0..10u32 {
        let first_cancels_before = stats.first_cancels();
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            let mut state = RunState::new();
            let mut req = state
                .start(units.clone())
                .expect("plan is non-empty on every iteration");
            let mut received = 0usize;
            loop {
                request_tx
                    .send(WorkerRequest::Query(req))
                    .unwrap_or_else(|_| panic!("iteration {i}: worker task should still be alive"));
                loop {
                    let response = tokio::time::timeout(Duration::from_secs(15), response_rx.recv())
                        .await
                        .unwrap_or_else(|_| {
                            panic!("iteration {i}: worker did not respond within 15s")
                        })
                        .unwrap_or_else(|| panic!("iteration {i}: response channel closed"));
                    let WorkerResponse::Query(q) = response else {
                        continue;
                    };
                    match q {
                        QueryResponse::Started { id, query_id } => {
                            let _ = state.on_started(id, query_id);
                        }
                        QueryResponse::Finished { id, result } => {
                            if !state.owns(id) {
                                continue;
                            }
                            received += 1;
                            if received == 1 {
                                match &result {
                                    Ok(QueryOutcome::Rows(page)) => assert!(
                                        page.has_next,
                                        "iteration {i}: statement 1 (unbounded) should report has_next"
                                    ),
                                    other => panic!(
                                        "iteration {i}: expected Rows for statement 1, got {other:?}"
                                    ),
                                }
                            } else if received == 2 {
                                match &result {
                                    Ok(QueryOutcome::Rows(page)) => {
                                        assert_eq!(page.rows.len(), 1, "iteration {i}")
                                    }
                                    other => panic!(
                                        "iteration {i}: expected statement 2 to return 1 row, got {other:?}"
                                    ),
                                }
                                // Statement 2's `execute()` only succeeds once
                                // it acquires the connection permit, which
                                // `drain_abandoned` (spawned when statement
                                // 1's stream was dropped) only releases after
                                // it fully resolves -- so by this point the
                                // abandon/cancel-escalation for statement 1
                                // is guaranteed to have already run to
                                // completion, race-free.
                                let first_cancels_after = stats.first_cancels();
                                if first_cancels_after == first_cancels_before {
                                    // The drain finished inside abandon_grace,
                                    // so no cancel was issued this iteration
                                    // and first_cancel_delay() still holds an
                                    // earlier iteration's value. Skipping
                                    // beats asserting on a stale number
                                    // (that's the exact class of bug this
                                    // test is being fixed for). Guarded by
                                    // the !first_delays.is_empty() assertion
                                    // after the loop.
                                } else {
                                    assert_eq!(
                                        first_cancels_after,
                                        first_cancels_before + 1,
                                        "iteration {i}: expected exactly one abandon to have \
                                         issued a first cancel"
                                    );
                                    let delay = stats.first_cancel_delay();
                                    first_delays.push(delay);
                                    assert!(
                                        delay < Duration::from_millis(300),
                                        "iteration {i}: the FIRST cancel attempt fired {delay:?} \
                                         after abandon, expected ~100ms (abandon_grace) -- a \
                                         value in the hundreds of ms to seconds is the \
                                         coop-starvation regression this test exists for. \
                                         Escalation attempts 2+ are NOT measured here by design. \
                                         diagnostics: multi_attempt_abandons={} of {} abandons, \
                                         cancel_send_failures={}, cancel_timeouts={}",
                                        stats.multi_attempt_abandons(),
                                        stats.abandons(),
                                        stats.cancel_send_failures(),
                                        stats.cancel_timeouts(),
                                    );
                                }
                                assert_eq!(
                                    stats.cancel_gave_up(),
                                    0,
                                    "iteration {i}: cancel escalation exhausted all attempts and \
                                     fell back to an unbounded natural drain \
                                     (send_failures={}, timeouts={})",
                                    stats.cancel_send_failures(),
                                    stats.cancel_timeouts(),
                                );
                            }
                            let out = state
                                .on_finished(id, &result)
                                .expect("owns() was true, so this must not be a no-op");
                            match out {
                                RunOutcome::Next(next_req) => {
                                    req = next_req;
                                    break;
                                }
                                RunOutcome::Done(summary) => {
                                    assert_eq!(summary.ran, 2, "iteration {i}");
                                    assert_eq!(summary.total, 2, "iteration {i}");
                                    assert_eq!(summary.cancelled, None, "iteration {i}");
                                    assert_eq!(summary.failed, None, "iteration {i}");
                                    return;
                                }
                            }
                        }
                        QueryResponse::CancelFailed { message, .. } => {
                            panic!("iteration {i}: unexpected CancelFailed: {message}")
                        }
                    }
                }
            }
        })
        .await;
        outcome.unwrap_or_else(|_| panic!("iteration {i} timed out after 20s"));
    }

    assert!(
        !first_delays.is_empty(),
        "no iteration ever issued a first cancel -- the 100M-row abandon is supposed to outlive \
         abandon_grace every time, so this test is no longer exercising the escalation path it \
         was written for"
    );

    shutdown_full_worker(request_tx, cancel_tx, worker_handle, canceller_handle).await;
}
