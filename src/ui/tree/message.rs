use crate::datasource::{Column, DataSourceError, Schema, Table};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug)]
pub enum TreeRequest {
    Schemas {
        id: RequestId,
    },
    Tables {
        id: RequestId,
        schema: String,
    },
    Columns {
        id: RequestId,
        schema: String,
        table: String,
    },
}

#[derive(Debug)]
pub enum TreeResponse {
    Schemas {
        id: RequestId,
        result: Result<Vec<Schema>, DataSourceError>,
    },
    Tables {
        id: RequestId,
        schema: String,
        result: Result<Vec<Table>, DataSourceError>,
    },
    Columns {
        id: RequestId,
        schema: String,
        table: String,
        result: Result<Vec<Column>, DataSourceError>,
    },
}
