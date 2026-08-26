use crate::app::message::{WorkerRequest, WorkerResponse};
use crate::datasource::{self, DataSource};
use crate::ui::grid::message::{GridRequest, GridResponse};
use crate::ui::grid::page;
use crate::ui::tree::message::{TreeRequest, TreeResponse};

// A single serial worker task: strictly one request in flight against the
// DataSource at a time. This is what keeps PostgresDataSource's one-permit
// semaphore's `Busy` error unreachable in normal operation — do not spawn a
// task per request. Tree and grid requests interleave FIFO through this same
// worker.
pub fn spawn(
    source: std::sync::Arc<dyn DataSource>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<WorkerRequest>,
    responses: tokio::sync::mpsc::UnboundedSender<WorkerResponse>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            if responses.send(handle(&*source, req).await).is_err() {
                break;
            }
        }
    })
}

async fn handle(source: &dyn DataSource, request: WorkerRequest) -> WorkerResponse {
    match request {
        WorkerRequest::Tree(req) => WorkerResponse::Tree(handle_tree(source, req).await),
        WorkerRequest::Grid(req) => WorkerResponse::Grid(handle_grid(source, req).await),
    }
}

async fn handle_tree(source: &dyn DataSource, request: TreeRequest) -> TreeResponse {
    match request {
        TreeRequest::Schemas { id } => TreeResponse::Schemas {
            id,
            result: source.list_schemas().await,
        },
        TreeRequest::Tables { id, schema } => TreeResponse::Tables {
            result: source.list_tables(&schema).await,
            id,
            schema,
        },
        TreeRequest::Columns { id, schema, table } => TreeResponse::Columns {
            result: source.list_columns(&schema, &table).await,
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
) -> Result<crate::ui::grid::Page, crate::datasource::DataSourceError> {
    let mut stream = source.execute(sql).await?;
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
    stream.finish().await;
    let fetched = take_result?;
    let columns = stream.columns().map(|c| c.to_vec()).unwrap_or_default();
    Ok(crate::ui::grid::Page::from_fetched(columns, fetched))
}
