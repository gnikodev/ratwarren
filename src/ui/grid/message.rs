#[derive(Debug)]
pub enum GridRequest {
    Page {
        id: crate::ui::RequestId,
        schema: String,
        table: String,
        offset: u64,
    },
}

#[derive(Debug)]
pub enum GridResponse {
    Page {
        id: crate::ui::RequestId,
        schema: String,
        table: String,
        offset: u64,
        result: Result<crate::ui::grid::Page, crate::datasource::DataSourceError>,
    },
}
