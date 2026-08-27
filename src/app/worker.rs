use crate::app::message::{WorkerRequest, WorkerResponse};
use crate::app::run::{QueryOutcome, QueryRequest, QueryResponse};
use crate::datasource::{self, DataSource, DataSourceError, QueryId};
use crate::ui::grid::message::{GridRequest, GridResponse};
use crate::ui::grid::page;
use crate::ui::tree::message::{TreeRequest, TreeResponse};

const BUSY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
const BUSY_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Retries only DataSourceError::Busy. Sound because the worker is the
/// single serial consumer of the connection: the only other permit holder
/// is a RowStream::drop background drain task, which always terminates (it
/// breaks on the first Err from a dead connection per Phase 3's fix), so
/// Busy here always means "a drain/cancel is finishing", never "another
/// user holding the connection". Termination alone isn't enough to bound
/// this loop's wait, though -- it also has to be *prompt*, which is a
/// property of the coop-yield fix in `drain_abandoned`'s drain loop
/// (`src/datasource/postgres.rs`), not of the break-on-error logic here.
///
/// BUSY_RETRY_BUDGET is not guaranteed to cover the worst case: cancel
/// escalation in `drain_abandoned` is bounded, but if every cancel attempt
/// fails, the abandoned stream falls back to an unbounded natural drain that
/// can outlast this budget, in which case the caller sees `BusyTimedOut`
/// even though the connection eventually recovers. Accepted for MVP0 -- there
/// is no reconnect path to escape to, and abandoning the drain early would
/// let a later query read the abandoned query's leftover rows as its own.
async fn retry_on_busy<T, F, Fut>(mut op: F) -> Result<T, DataSourceError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DataSourceError>>,
{
    let deadline = tokio::time::Instant::now() + BUSY_RETRY_BUDGET;
    loop {
        match op().await {
            Err(DataSourceError::Busy { .. }) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(BUSY_RETRY_DELAY).await;
            }
            Err(DataSourceError::Busy { name }) => {
                return Err(DataSourceError::BusyTimedOut {
                    name,
                    waited: BUSY_RETRY_BUDGET,
                });
            }
            other => return other,
        }
    }
}

// A single serial worker task: strictly one request in flight against the
// DataSource at a time. This is what keeps PostgresDataSource's one-permit
// semaphore's `Busy` error unreachable in normal operation — do not spawn a
// task per request. Tree and grid requests interleave FIFO through this same
// worker. The one exception is `cancel`, which is issued from a separate
// task (`spawn_canceller`) precisely so it can interrupt this worker while
// it's blocked awaiting a long-running query.
pub fn spawn(
    source: std::sync::Arc<dyn DataSource>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<WorkerRequest>,
    responses: tokio::sync::mpsc::UnboundedSender<WorkerResponse>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            let response = handle(&*source, req, &responses).await;
            if responses.send(response).is_err() {
                break;
            }
        }
    })
}

/// Runs on its own task so a `cancel` can reach the connection while the
/// main worker task above is blocked awaiting a long-running `execute`.
/// This task never touches the connection's permit (per `DataSource::cancel`'s
/// design — it only uses `start_gate`, no `try_acquire`), so it can never
/// itself deadlock against the worker holding the permit.
pub fn spawn_canceller(
    source: std::sync::Arc<dyn DataSource>,
    mut cancels: tokio::sync::mpsc::UnboundedReceiver<QueryId>,
    responses: tokio::sync::mpsc::UnboundedSender<WorkerResponse>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(qid) = cancels.recv().await {
            if let Err(e) = source.cancel(qid).await {
                let _ = responses.send(WorkerResponse::Query(QueryResponse::CancelFailed {
                    message: crate::ui::error_chain(&e),
                }));
            }
        }
    })
}

async fn handle(
    source: &dyn DataSource,
    request: WorkerRequest,
    responses: &tokio::sync::mpsc::UnboundedSender<WorkerResponse>,
) -> WorkerResponse {
    match request {
        WorkerRequest::Tree(req) => WorkerResponse::Tree(handle_tree(source, req).await),
        WorkerRequest::Grid(req) => WorkerResponse::Grid(handle_grid(source, req).await),
        WorkerRequest::Query(req) => {
            WorkerResponse::Query(handle_query(source, req, responses).await)
        }
    }
}

async fn handle_tree(source: &dyn DataSource, request: TreeRequest) -> TreeResponse {
    match request {
        TreeRequest::Schemas { id } => TreeResponse::Schemas {
            id,
            result: retry_on_busy(|| source.list_schemas()).await,
        },
        TreeRequest::Tables { id, schema } => TreeResponse::Tables {
            result: retry_on_busy(|| source.list_tables(&schema)).await,
            id,
            schema,
        },
        TreeRequest::Columns { id, schema, table } => TreeResponse::Columns {
            result: retry_on_busy(|| source.list_columns(&schema, &table)).await,
            id,
            schema,
            table,
        },
    }
}

async fn handle_grid(source: &dyn DataSource, request: GridRequest) -> GridResponse {
    match request {
        GridRequest::Page {
            id,
            schema,
            table,
            offset,
        } => {
            let sql =
                datasource::select_page_sql(&schema, &table, page::FETCH_LIMIT as u64, offset);
            let result = fetch_page(source, &sql).await;
            GridResponse::Page {
                id,
                schema,
                table,
                offset,
                result,
            }
        }
    }
}

pub async fn fetch_page(
    source: &dyn DataSource,
    sql: &str,
) -> Result<crate::ui::grid::Page, DataSourceError> {
    let mut stream = retry_on_busy(|| source.execute(sql)).await?;
    let take_result = stream.take(page::FETCH_LIMIT).await;
    // `stream.finish()` must run before `stream` is dropped, on BOTH the
    // success and error path (hence capturing `take_result` instead of
    // using `?` directly): dropping a not-yet-`Ended` stream — whether it's
    // still `Streaming`/`AfterComplete` or was `Aborted` by a query error —
    // defers the connection permit release to a background drain task
    // (spurious Busy on the next request) and, past abandon_grace, can fire
    // a cancel_query against a query that has already finished — which per
    // Phase 3's cancel_settle measurements has a small but real chance of
    // hitting a later, unrelated query instead.
    //
    // This is safe here (unlike `handle_query` below) only because every
    // caller of `fetch_page` passes a `LIMIT`-bounded query: `take()` always
    // reaches a terminal stream state well before FETCH_LIMIT rows, so
    // `finish()` never has an unbounded remainder to drain synchronously.
    stream.finish().await;
    let fetched = take_result?;
    let columns = stream.columns().map(|c| c.to_vec()).unwrap_or_default();
    Ok(crate::ui::grid::Page::from_fetched(columns, fetched))
}

async fn handle_query(
    source: &dyn DataSource,
    request: QueryRequest,
    responses: &tokio::sync::mpsc::UnboundedSender<WorkerResponse>,
) -> QueryResponse {
    let QueryRequest { id, sql } = request;

    let mut stream = match retry_on_busy(|| source.execute(&sql)).await {
        Ok(s) => s,
        Err(e) => return QueryResponse::Finished { id, result: Err(e) },
    };
    let _ = responses.send(WorkerResponse::Query(QueryResponse::Started {
        id,
        query_id: stream.query_id(),
    }));

    let taken = stream.take(page::FETCH_LIMIT).await;
    let result = match taken {
        Err(e) => {
            stream.finish().await;
            Err(e)
        }
        Ok(rows) => {
            let columns = stream.columns().map(|c| c.to_vec()).unwrap_or_default();
            if rows.len() < page::FETCH_LIMIT {
                // Provably `Ended`: `take()` only stops early via a `None`
                // from `next()`, which only happens in the `Ended` terminal
                // state -- `finish()` is a safe no-op here.
                let affected = stream.rows_affected();
                stream.finish().await;
                Ok(if columns.is_empty() {
                    QueryOutcome::NoResultSet {
                        rows_affected: affected.unwrap_or(0),
                    }
                } else {
                    QueryOutcome::Rows(crate::ui::grid::Page::from_fetched(columns, rows))
                })
            } else {
                // Exactly FETCH_LIMIT rows: unknown whether the real result
                // set has 51 rows or 100 million. Calling `finish()` here
                // would synchronously drain a potentially enormous remaining
                // result set on this worker task, freezing the whole UI --
                // hand off to `RowStream::drop`'s background-drain-and-
                // abandon path instead, which is the safe mechanism for an
                // unbounded, partially-consumed stream.
                drop(stream);
                Ok(QueryOutcome::Rows(crate::ui::grid::Page::from_fetched(
                    columns, rows,
                )))
            }
        }
    };
    QueryResponse::Finished { id, result }
}
