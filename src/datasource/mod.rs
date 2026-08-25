mod introspect;
mod postgres;
mod stream;

pub use postgres::{ConnectOptions, PostgresDataSource, quote_ident};
pub use stream::{Row, RowStream};

#[async_trait::async_trait]
pub trait DataSource: Send + Sync {
    async fn list_schemas(&self) -> Result<Vec<Schema>, DataSourceError>;
    async fn list_tables(&self, schema: &str) -> Result<Vec<Table>, DataSourceError>;
    async fn list_columns(&self, schema: &str, table: &str)
    -> Result<Vec<Column>, DataSourceError>;
    async fn execute(&self, sql: &str) -> Result<RowStream, DataSourceError>;
    async fn explain(&self, sql: &str) -> Result<String, DataSourceError>;
    async fn cancel(&self, query_id: QueryId) -> Result<(), DataSourceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub is_system: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub default_expr: Option<String>,
    pub ordinal: i16,
    pub is_primary_key: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DataSourceError {
    #[error("connection {name:?}: failed to open the SSH tunnel")]
    Tunnel {
        name: String,
        #[source]
        source: crate::tunnel::TunnelError,
    },
    #[error("connection {name:?}: the SSH tunnel is no longer running")]
    TunnelDown {
        name: String,
        #[source]
        source: crate::tunnel::TunnelError,
    },
    #[error("connection {name:?}: the SSH tunnel setup task panicked")]
    TunnelTaskPanicked { name: String },
    #[error("connection {name:?}: failed to connect to postgres at {addr}")]
    Connect {
        name: String,
        addr: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error(
        "connection {name:?}: the postgres connection is closed{}",
        reason_suffix(reason)
    )]
    Closed {
        name: String,
        reason: Option<String>,
    },
    #[error("query failed: {source}")]
    Query {
        sql: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("query was cancelled")]
    Cancelled,
    #[error("connection {name:?} is busy with another query")]
    Busy { name: String },
    #[error(
        "`{method}` requires a single SQL statement; the server executed more than one and the extra results were discarded"
    )]
    MultipleStatements { method: &'static str },
    #[error("connection {name:?}: failed to deliver the cancel request")]
    CancelFailed {
        name: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("connection {name:?}: the cancel request timed out after {}ms", timeout.as_millis())]
    CancelTimedOut {
        name: String,
        timeout: std::time::Duration,
    },
    #[error("failed to decode column {column:?} as UTF-8 text")]
    Decode {
        column: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("postgres returned an unexpected relkind {kind:?}")]
    UnexpectedRelkind { kind: String },
}

fn reason_suffix(reason: &Option<String>) -> String {
    match reason {
        Some(r) if !r.trim().is_empty() => format!(": {}", r.trim()),
        _ => String::new(),
    }
}

impl DataSourceError {
    pub fn error_position(&self) -> Option<u32> {
        match self {
            DataSourceError::Query { source, .. } => {
                source.as_db_error().and_then(|e| match e.position() {
                    Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                    _ => None,
                })
            }
            _ => None,
        }
    }
}
