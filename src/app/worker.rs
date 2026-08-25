use crate::datasource::DataSource;
use crate::ui::tree::message::{TreeRequest, TreeResponse};

// A single serial worker task: strictly one request in flight against the
// DataSource at a time. This is what keeps PostgresDataSource's one-permit
// semaphore's `Busy` error unreachable in normal operation — do not spawn a
// task per request.
pub fn spawn(
    source: std::sync::Arc<dyn DataSource>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<TreeRequest>,
    responses: tokio::sync::mpsc::UnboundedSender<TreeResponse>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            if responses.send(handle(&*source, req).await).is_err() {
                break;
            }
        }
    })
}

async fn handle(source: &dyn DataSource, request: TreeRequest) -> TreeResponse {
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
