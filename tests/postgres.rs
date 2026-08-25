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

use ratwarren::config::Connection;
use ratwarren::datasource::{
    ConnectOptions, DataSource, DataSourceError, PostgresDataSource, Row, TableKind, quote_ident,
};

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
