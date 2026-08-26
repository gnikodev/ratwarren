use crate::datasource::{Column, Schema, Table};

pub use crate::ui::Load;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKey {
    Schema {
        schema: String,
    },
    Table {
        schema: String,
        table: String,
    },
    Column {
        schema: String,
        table: String,
        column: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaNode {
    pub schema: Schema,
    pub expanded: bool,
    pub tables: Load<Vec<TableNode>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableNode {
    pub table: Table,
    pub expanded: bool,
    pub columns: Load<Vec<Column>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectTree {
    pub schemas: Load<Vec<SchemaNode>>,
}
