use ratatui::widgets::ListState;

use crate::datasource::TableKind;
use crate::ui::tree::message::{RequestId, TreeRequest, TreeResponse};
use crate::ui::tree::model::{Load, NodeKey, ObjectTree, SchemaNode, TableNode};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TreeRowKey {
    Node(NodeKey),
    // `None` = root-level (the schema list itself).
    Status(Option<NodeKey>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatusKind {
    Loading,
    Empty,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TreeRowKind {
    Schema {
        name: String,
        expanded: bool,
    },
    Table {
        name: String,
        kind: TableKind,
        expanded: bool,
    },
    Column {
        name: String,
        data_type: String,
        is_nullable: bool,
        is_primary_key: bool,
    },
    Status(StatusKind),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub key: TreeRowKey,
    pub depth: u16,
    pub kind: TreeRowKind,
}

pub enum TreeCommand {
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    First,
    Last,
    Expand,
    Collapse,
    Toggle,
    Refresh,
    ToggleSystemSchemas,
}

pub struct ObjectTreeState {
    tree: ObjectTree,
    rows: Vec<TreeRow>,
    list: ListState,
    next_request_id: u64,
    show_system_schemas: bool,
    viewport_height: u16,
}

impl Default for ObjectTreeState {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectTreeState {
    pub fn new() -> Self {
        Self {
            tree: ObjectTree::default(),
            rows: Vec::new(),
            list: ListState::default(),
            next_request_id: 0,
            show_system_schemas: false,
            viewport_height: 10,
        }
    }

    pub fn refresh_root(&mut self) -> TreeRequest {
        let id = self.next_id();
        self.tree.schemas = Load::Loading { id };
        self.rebuild_rows();
        TreeRequest::Schemas { id }
    }

    pub fn apply(&mut self, response: TreeResponse) {
        match response {
            TreeResponse::Schemas { id, result } => {
                if !matches!(&self.tree.schemas, Load::Loading { id: current } if *current == id) {
                    return;
                }
                self.tree.schemas = match result {
                    Ok(schemas) => Load::Loaded(
                        schemas
                            .into_iter()
                            .map(|schema| SchemaNode {
                                schema,
                                expanded: false,
                                tables: Load::NotLoaded,
                            })
                            .collect(),
                    ),
                    Err(e) => Load::Failed {
                        message: crate::ui::error_chain(&e),
                    },
                };
            }
            TreeResponse::Tables { id, schema, result } => {
                let Some(node) = self.schema_node_mut(&schema) else {
                    return;
                };
                if !matches!(&node.tables, Load::Loading { id: current } if *current == id) {
                    return;
                }
                node.tables = match result {
                    Ok(tables) => Load::Loaded(
                        tables
                            .into_iter()
                            .map(|table| TableNode {
                                table,
                                expanded: false,
                                columns: Load::NotLoaded,
                            })
                            .collect(),
                    ),
                    Err(e) => Load::Failed {
                        message: crate::ui::error_chain(&e),
                    },
                };
            }
            TreeResponse::Columns {
                id,
                schema,
                table,
                result,
            } => {
                let Some(node) = self.table_node_mut(&schema, &table) else {
                    return;
                };
                if !matches!(&node.columns, Load::Loading { id: current } if *current == id) {
                    return;
                }
                node.columns = match result {
                    Ok(columns) => Load::Loaded(columns),
                    Err(e) => Load::Failed {
                        message: crate::ui::error_chain(&e),
                    },
                };
            }
        }
        self.rebuild_rows();
    }

    pub fn command(&mut self, cmd: TreeCommand) -> Option<TreeRequest> {
        match cmd {
            TreeCommand::MoveUp => {
                self.move_selection(-1);
                None
            }
            TreeCommand::MoveDown => {
                self.move_selection(1);
                None
            }
            TreeCommand::PageUp => {
                self.move_selection(-(self.viewport_height as isize));
                None
            }
            TreeCommand::PageDown => {
                self.move_selection(self.viewport_height as isize);
                None
            }
            TreeCommand::First => {
                if !self.rows.is_empty() {
                    self.list.select(Some(0));
                }
                None
            }
            TreeCommand::Last => {
                if !self.rows.is_empty() {
                    self.list.select(Some(self.rows.len() - 1));
                }
                None
            }
            TreeCommand::Expand => self.handle_expand(),
            TreeCommand::Collapse => self.handle_collapse(),
            TreeCommand::Toggle => self.handle_toggle(),
            TreeCommand::Refresh => self.handle_refresh(),
            TreeCommand::ToggleSystemSchemas => {
                self.show_system_schemas = !self.show_system_schemas;
                self.rebuild_rows();
                None
            }
        }
    }

    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&TreeRow> {
        self.list.selected().and_then(|i| self.rows.get(i))
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.list.selected()
    }

    pub(crate) fn set_viewport_height(&mut self, h: u16) {
        self.viewport_height = h;
    }

    pub(crate) fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn next_id(&mut self) -> RequestId {
        let id = self.next_request_id;
        self.next_request_id += 1;
        RequestId(id)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.list.select(None);
            return;
        }
        let current = self.list.selected().unwrap_or(0) as isize;
        let max = self.rows.len() as isize - 1;
        let new = (current + delta).clamp(0, max);
        self.list.select(Some(new as usize));
    }

    fn schema_node_mut(&mut self, schema: &str) -> Option<&mut SchemaNode> {
        match &mut self.tree.schemas {
            Load::Loaded(schemas) => schemas.iter_mut().find(|s| s.schema.name == schema),
            _ => None,
        }
    }

    fn table_node_mut(&mut self, schema: &str, table: &str) -> Option<&mut TableNode> {
        let schema_node = self.schema_node_mut(schema)?;
        match &mut schema_node.tables {
            Load::Loaded(tables) => tables.iter_mut().find(|t| t.table.name == table),
            _ => None,
        }
    }

    fn is_expanded(&mut self, key: &NodeKey) -> bool {
        match key {
            NodeKey::Schema { schema } => self.schema_node_mut(schema).is_some_and(|n| n.expanded),
            NodeKey::Table { schema, table } => self
                .table_node_mut(schema, table)
                .is_some_and(|n| n.expanded),
            NodeKey::Column { .. } => false,
        }
    }

    // Sets `expanded = true` and, if the relevant `Load<_>` field is
    // `NotLoaded`/`Failed`, kicks off a fresh load. Already-`Loaded` fields
    // are left untouched — the data is already there.
    fn expand_key(&mut self, key: &NodeKey) -> Option<TreeRequest> {
        match key {
            NodeKey::Schema { schema } => {
                let schema = schema.clone();
                let node = self.schema_node_mut(&schema)?;
                if matches!(node.tables, Load::Loaded(_)) {
                    node.expanded = true;
                    None
                } else {
                    let id = self.next_id();
                    let node = self.schema_node_mut(&schema)?;
                    node.expanded = true;
                    node.tables = Load::Loading { id };
                    Some(TreeRequest::Tables { id, schema })
                }
            }
            NodeKey::Table { schema, table } => {
                let schema = schema.clone();
                let table = table.clone();
                let node = self.table_node_mut(&schema, &table)?;
                if matches!(node.columns, Load::Loaded(_)) {
                    node.expanded = true;
                    None
                } else {
                    let id = self.next_id();
                    let node = self.table_node_mut(&schema, &table)?;
                    node.expanded = true;
                    node.columns = Load::Loading { id };
                    Some(TreeRequest::Columns { id, schema, table })
                }
            }
            NodeKey::Column { .. } => None,
        }
    }

    // Collapsing never touches the `Load<_>` field: loaded children are
    // retained and in-flight requests are not cancelled, so re-expanding is
    // instant if the data (or an already-en-route response) is still there.
    fn collapse_key(&mut self, key: &NodeKey) {
        match key {
            NodeKey::Schema { schema } => {
                if let Some(node) = self.schema_node_mut(schema) {
                    node.expanded = false;
                }
            }
            NodeKey::Table { schema, table } => {
                if let Some(node) = self.table_node_mut(schema, table) {
                    node.expanded = false;
                }
            }
            NodeKey::Column { .. } => {}
        }
    }

    fn select_node_key(&mut self, key: &NodeKey) {
        if let Some(idx) = self
            .rows
            .iter()
            .position(|r| r.key == TreeRowKey::Node(key.clone()))
        {
            self.list.select(Some(idx));
        }
    }

    fn handle_expand(&mut self) -> Option<TreeRequest> {
        let key = match self.selected()?.key.clone() {
            TreeRowKey::Node(k) => k,
            TreeRowKey::Status(_) => return None,
        };
        if self.is_expanded(&key) {
            self.move_selection(1);
            None
        } else {
            let req = self.expand_key(&key);
            self.rebuild_rows();
            req
        }
    }

    fn handle_collapse(&mut self) -> Option<TreeRequest> {
        let key = match self.selected()?.key.clone() {
            TreeRowKey::Node(k) => k,
            TreeRowKey::Status(_) => return None,
        };
        match &key {
            NodeKey::Column { schema, table, .. } => {
                let parent = NodeKey::Table {
                    schema: schema.clone(),
                    table: table.clone(),
                };
                self.select_node_key(&parent);
            }
            NodeKey::Table { schema, .. } => {
                if self.is_expanded(&key) {
                    self.collapse_key(&key);
                    self.rebuild_rows();
                } else {
                    let parent = NodeKey::Schema {
                        schema: schema.clone(),
                    };
                    self.select_node_key(&parent);
                }
            }
            NodeKey::Schema { .. } => {
                if self.is_expanded(&key) {
                    self.collapse_key(&key);
                    self.rebuild_rows();
                }
                // Already collapsed at the root: no parent to move to.
            }
        }
        None
    }

    fn handle_toggle(&mut self) -> Option<TreeRequest> {
        let key = self.selected()?.key.clone();
        match key {
            TreeRowKey::Status(parent) => self.refresh_target(parent.as_ref()),
            TreeRowKey::Node(NodeKey::Column { .. }) => None,
            TreeRowKey::Node(key) => {
                if self.is_expanded(&key) {
                    self.collapse_key(&key);
                    self.rebuild_rows();
                    None
                } else {
                    let req = self.expand_key(&key);
                    self.rebuild_rows();
                    req
                }
            }
        }
    }

    fn handle_refresh(&mut self) -> Option<TreeRequest> {
        let target = match self.selected() {
            None => None,
            Some(row) => match &row.key {
                TreeRowKey::Node(NodeKey::Schema { schema }) => Some(NodeKey::Schema {
                    schema: schema.clone(),
                }),
                TreeRowKey::Node(NodeKey::Table { schema, table }) => Some(NodeKey::Table {
                    schema: schema.clone(),
                    table: table.clone(),
                }),
                TreeRowKey::Node(NodeKey::Column { .. }) => None,
                TreeRowKey::Status(parent) => parent.clone(),
            },
        };
        self.refresh_target(target.as_ref())
    }

    fn refresh_target(&mut self, target: Option<&NodeKey>) -> Option<TreeRequest> {
        let req = match target {
            None => {
                let id = self.next_id();
                self.tree.schemas = Load::Loading { id };
                Some(TreeRequest::Schemas { id })
            }
            Some(NodeKey::Schema { schema }) => {
                let id = self.next_id();
                let schema = schema.clone();
                if let Some(node) = self.schema_node_mut(&schema) {
                    node.expanded = true;
                    node.tables = Load::Loading { id };
                }
                Some(TreeRequest::Tables { id, schema })
            }
            Some(NodeKey::Table { schema, table }) => {
                let id = self.next_id();
                let schema = schema.clone();
                let table = table.clone();
                if let Some(node) = self.table_node_mut(&schema, &table) {
                    node.expanded = true;
                    node.columns = Load::Loading { id };
                }
                Some(TreeRequest::Columns { id, schema, table })
            }
            // Columns are leaves with no `Load<_>` field of their own; a
            // Status row can never be parented by one (see build_rows).
            Some(NodeKey::Column { .. }) => None,
        };
        self.rebuild_rows();
        req
    }

    fn rebuild_rows(&mut self) {
        let previous_key = self.selected().map(|r| r.key.clone());
        let previous_index = self.list.selected();
        let previous_len = self.rows.len();
        self.rows = build_rows(&self.tree, self.show_system_schemas);
        let new_index = restore_selection(&self.rows, previous_key.as_ref(), previous_index);

        // `ListState::select` only resets the scroll offset when clearing the
        // selection entirely; it leaves a stale offset in place otherwise. If
        // the row set shrank, or the selection didn't land on the exact same
        // key (i.e. `restore_selection` fell back to a parent/clamped index),
        // a leftover offset from before the rebuild can point past the end of
        // the new (shorter) list, hiding rows that are still in `self.rows`
        // until the next explicit scroll. Rebuilding the `ListState` from
        // scratch resets the offset to 0 so `List`'s own render-time scroll
        // logic re-derives a correct offset for the new selection.
        let exact_match = previous_key.as_ref().is_some_and(|key| {
            new_index
                .and_then(|idx| self.rows.get(idx))
                .is_some_and(|row| &row.key == key)
        });
        if self.rows.len() < previous_len || !exact_match {
            self.list = ListState::default().with_selected(new_index);
        } else {
            self.list.select(new_index);
        }
    }
}

fn build_rows(tree: &ObjectTree, show_system_schemas: bool) -> Vec<TreeRow> {
    let mut rows = Vec::new();

    match &tree.schemas {
        Load::NotLoaded => {}
        Load::Loading { .. } => {
            rows.push(status_row(TreeRowKey::Status(None), 0, StatusKind::Loading))
        }
        Load::Failed { message } => rows.push(status_row(
            TreeRowKey::Status(None),
            0,
            StatusKind::Error(message.clone()),
        )),
        Load::Loaded(schemas) => {
            let visible: Vec<&SchemaNode> = schemas
                .iter()
                .filter(|s| show_system_schemas || !s.schema.is_system)
                .collect();
            if visible.is_empty() {
                rows.push(status_row(TreeRowKey::Status(None), 0, StatusKind::Empty));
            } else {
                for node in visible {
                    push_schema_rows(&mut rows, node);
                }
            }
        }
    }

    rows
}

fn push_schema_rows(rows: &mut Vec<TreeRow>, node: &SchemaNode) {
    let schema_key = NodeKey::Schema {
        schema: node.schema.name.clone(),
    };
    rows.push(TreeRow {
        key: TreeRowKey::Node(schema_key.clone()),
        depth: 0,
        kind: TreeRowKind::Schema {
            name: node.schema.name.clone(),
            expanded: node.expanded,
        },
    });

    if !node.expanded {
        return;
    }

    match &node.tables {
        Load::NotLoaded => {}
        Load::Loading { .. } => rows.push(status_row(
            TreeRowKey::Status(Some(schema_key.clone())),
            0,
            StatusKind::Loading,
        )),
        Load::Failed { message } => rows.push(status_row(
            TreeRowKey::Status(Some(schema_key.clone())),
            0,
            StatusKind::Error(message.clone()),
        )),
        Load::Loaded(tables) => {
            if tables.is_empty() {
                rows.push(status_row(
                    TreeRowKey::Status(Some(schema_key.clone())),
                    0,
                    StatusKind::Empty,
                ));
            } else {
                for table_node in tables {
                    push_table_rows(rows, &node.schema.name, table_node);
                }
            }
        }
    }
}

fn push_table_rows(rows: &mut Vec<TreeRow>, schema_name: &str, node: &TableNode) {
    let table_key = NodeKey::Table {
        schema: schema_name.to_string(),
        table: node.table.name.clone(),
    };
    rows.push(TreeRow {
        key: TreeRowKey::Node(table_key.clone()),
        depth: 1,
        kind: TreeRowKind::Table {
            name: node.table.name.clone(),
            kind: node.table.kind,
            expanded: node.expanded,
        },
    });

    if !node.expanded {
        return;
    }

    match &node.columns {
        Load::NotLoaded => {}
        Load::Loading { .. } => rows.push(status_row(
            TreeRowKey::Status(Some(table_key.clone())),
            1,
            StatusKind::Loading,
        )),
        Load::Failed { message } => rows.push(status_row(
            TreeRowKey::Status(Some(table_key.clone())),
            1,
            StatusKind::Error(message.clone()),
        )),
        Load::Loaded(columns) => {
            if columns.is_empty() {
                rows.push(status_row(
                    TreeRowKey::Status(Some(table_key.clone())),
                    1,
                    StatusKind::Empty,
                ));
            } else {
                for column in columns {
                    let column_key = NodeKey::Column {
                        schema: schema_name.to_string(),
                        table: node.table.name.clone(),
                        column: column.name.clone(),
                    };
                    rows.push(TreeRow {
                        key: TreeRowKey::Node(column_key),
                        depth: 2,
                        kind: TreeRowKind::Column {
                            name: column.name.clone(),
                            data_type: column.data_type.clone(),
                            is_nullable: column.is_nullable,
                            is_primary_key: column.is_primary_key,
                        },
                    });
                }
            }
        }
    }
}

fn status_row(key: TreeRowKey, depth: u16, kind: StatusKind) -> TreeRow {
    TreeRow {
        key,
        depth,
        kind: TreeRowKind::Status(kind),
    }
}

fn restore_selection(
    rows: &[TreeRow],
    previous_key: Option<&TreeRowKey>,
    previous_index: Option<usize>,
) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }

    if let Some(key) = previous_key {
        if let Some(idx) = rows.iter().position(|r| &r.key == key) {
            return Some(idx);
        }

        let fallback_key = match key {
            TreeRowKey::Status(Some(parent)) => Some(TreeRowKey::Node(parent.clone())),
            TreeRowKey::Node(NodeKey::Column { schema, table, .. }) => {
                Some(TreeRowKey::Node(NodeKey::Table {
                    schema: schema.clone(),
                    table: table.clone(),
                }))
            }
            TreeRowKey::Node(NodeKey::Table { schema, .. }) => {
                Some(TreeRowKey::Node(NodeKey::Schema {
                    schema: schema.clone(),
                }))
            }
            _ => None,
        };
        if let Some(fallback_key) = fallback_key
            && let Some(idx) = rows.iter().position(|r| r.key == fallback_key)
        {
            return Some(idx);
        }
    }

    match previous_index {
        Some(i) => Some(i.min(rows.len() - 1)),
        None => Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::{Column, DataSourceError, Schema, Table};
    use crate::tunnel::TunnelError;

    fn row(key: TreeRowKey) -> TreeRow {
        TreeRow {
            key,
            depth: 0,
            kind: TreeRowKind::Status(StatusKind::Empty),
        }
    }

    fn schema(name: &str) -> Schema {
        Schema {
            name: name.into(),
            is_system: false,
        }
    }

    fn schema_node(name: &str) -> SchemaNode {
        SchemaNode {
            schema: schema(name),
            expanded: false,
            tables: Load::NotLoaded,
        }
    }

    fn table(name: &str) -> Table {
        Table {
            name: name.into(),
            kind: TableKind::Table,
        }
    }

    fn table_node(name: &str) -> TableNode {
        TableNode {
            table: table(name),
            expanded: false,
            columns: Load::NotLoaded,
        }
    }

    fn sample_column(name: &str) -> Column {
        Column {
            name: name.into(),
            data_type: "text".into(),
            is_nullable: true,
            default_expr: None,
            ordinal: 1,
            is_primary_key: false,
        }
    }

    fn row_label(row: &TreeRow) -> &str {
        match &row.kind {
            TreeRowKind::Schema { name, .. } => name,
            TreeRowKind::Table { name, .. } => name,
            TreeRowKind::Column { name, .. } => name,
            TreeRowKind::Status(_) => "<status>",
        }
    }

    #[test]
    fn restore_selection_on_empty_rows_is_none() {
        assert_eq!(restore_selection(&[], None, Some(3)), None);
    }

    #[test]
    fn restore_selection_prefers_exact_key_match() {
        let schema_a = TreeRowKey::Node(NodeKey::Schema { schema: "a".into() });
        let schema_b = TreeRowKey::Node(NodeKey::Schema { schema: "b".into() });
        let rows = vec![row(schema_a.clone()), row(schema_b.clone())];

        assert_eq!(restore_selection(&rows, Some(&schema_b), Some(0)), Some(1));
    }

    #[test]
    fn restore_selection_falls_back_from_status_to_parent_node() {
        let parent = NodeKey::Schema { schema: "a".into() };
        let status = TreeRowKey::Status(Some(parent.clone()));
        let rows = vec![row(TreeRowKey::Node(parent))];

        assert_eq!(restore_selection(&rows, Some(&status), Some(0)), Some(0));
    }

    #[test]
    fn restore_selection_falls_back_from_column_to_table() {
        let table_key = TreeRowKey::Node(NodeKey::Table {
            schema: "s".into(),
            table: "t".into(),
        });
        let column_key = TreeRowKey::Node(NodeKey::Column {
            schema: "s".into(),
            table: "t".into(),
            column: "c".into(),
        });
        let rows = vec![row(table_key.clone())];

        assert_eq!(
            restore_selection(&rows, Some(&column_key), Some(0)),
            Some(0)
        );
    }

    #[test]
    fn restore_selection_falls_back_from_table_to_schema() {
        let schema_key = TreeRowKey::Node(NodeKey::Schema { schema: "s".into() });
        let table_key = TreeRowKey::Node(NodeKey::Table {
            schema: "s".into(),
            table: "t".into(),
        });
        let rows = vec![row(schema_key.clone())];

        assert_eq!(restore_selection(&rows, Some(&table_key), Some(0)), Some(0));
    }

    #[test]
    fn restore_selection_clamps_to_previous_index_when_nothing_matches() {
        let gone = TreeRowKey::Node(NodeKey::Schema {
            schema: "gone".into(),
        });
        let rows = vec![
            row(TreeRowKey::Node(NodeKey::Schema { schema: "a".into() })),
            row(TreeRowKey::Node(NodeKey::Schema { schema: "b".into() })),
        ];

        assert_eq!(restore_selection(&rows, Some(&gone), Some(50)), Some(1));
    }

    #[test]
    fn restore_selection_defaults_to_zero_without_previous_index() {
        let gone = TreeRowKey::Node(NodeKey::Schema {
            schema: "gone".into(),
        });
        let rows = vec![row(TreeRowKey::Node(NodeKey::Schema {
            schema: "a".into(),
        }))];

        assert_eq!(restore_selection(&rows, Some(&gone), None), Some(0));
    }

    #[test]
    fn apply_ignores_response_with_stale_request_id() {
        let mut state = ObjectTreeState::new();
        let first = state.refresh_root();
        let TreeRequest::Schemas { id: first_id } = first else {
            unreachable!()
        };
        // A refresh supersedes the in-flight request with a new id.
        let second = state.refresh_root();
        let TreeRequest::Schemas { id: second_id } = second else {
            unreachable!()
        };
        assert_ne!(first_id, second_id);

        state.apply(TreeResponse::Schemas {
            id: first_id,
            result: Ok(vec![]),
        });
        assert!(matches!(state.tree.schemas, Load::Loading { id } if id == second_id));
    }

    #[test]
    fn apply_accepts_response_matching_current_request_id() {
        let mut state = ObjectTreeState::new();
        let TreeRequest::Schemas { id } = state.refresh_root() else {
            unreachable!()
        };

        state.apply(TreeResponse::Schemas {
            id,
            result: Ok(vec![]),
        });
        assert!(matches!(state.tree.schemas, Load::Loaded(ref v) if v.is_empty()));
    }

    // --- movement / clamping (TreeCommand) ---

    #[test]
    fn move_down_and_up_clamp_at_row_bounds() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas =
            Load::Loaded(vec![schema_node("a"), schema_node("b"), schema_node("c")]);
        state.rebuild_rows();
        assert_eq!(state.selected_index(), Some(0));

        assert!(state.command(TreeCommand::MoveUp).is_none());
        assert_eq!(
            state.selected_index(),
            Some(0),
            "moving up from the first row must not go out of bounds"
        );

        let _ = state.command(TreeCommand::MoveDown);
        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(state.selected_index(), Some(2));

        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(
            state.selected_index(),
            Some(2),
            "moving down from the last row must not go out of bounds or wrap"
        );
    }

    #[test]
    fn page_down_and_up_move_by_viewport_height_and_clamp() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded((0..10).map(|i| schema_node(&format!("s{i}"))).collect());
        state.rebuild_rows();
        state.set_viewport_height(4);
        assert_eq!(state.selected_index(), Some(0));

        let _ = state.command(TreeCommand::PageDown);
        assert_eq!(state.selected_index(), Some(4));
        let _ = state.command(TreeCommand::PageDown);
        assert_eq!(state.selected_index(), Some(8));
        // Only one row remains before the end; PageDown must clamp, not
        // overshoot or wrap.
        let _ = state.command(TreeCommand::PageDown);
        assert_eq!(state.selected_index(), Some(9));

        let _ = state.command(TreeCommand::PageUp);
        assert_eq!(state.selected_index(), Some(5));
        let _ = state.command(TreeCommand::PageUp);
        assert_eq!(state.selected_index(), Some(1));
        // Clamp at the start rather than going negative.
        let _ = state.command(TreeCommand::PageUp);
        assert_eq!(state.selected_index(), Some(0));
    }

    #[test]
    fn first_and_last_jump_to_bounds() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas =
            Load::Loaded(vec![schema_node("a"), schema_node("b"), schema_node("c")]);
        state.rebuild_rows();
        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(state.selected_index(), Some(1));

        assert!(state.command(TreeCommand::Last).is_none());
        assert_eq!(state.selected_index(), Some(2));

        assert!(state.command(TreeCommand::First).is_none());
        assert_eq!(state.selected_index(), Some(0));
    }

    #[test]
    fn movement_commands_on_empty_rows_do_not_panic() {
        let mut state = ObjectTreeState::new();
        assert!(state.rows().is_empty());

        for cmd in [
            TreeCommand::MoveUp,
            TreeCommand::MoveDown,
            TreeCommand::PageUp,
            TreeCommand::PageDown,
            TreeCommand::First,
            TreeCommand::Last,
        ] {
            assert!(state.command(cmd).is_none());
            assert_eq!(state.selected_index(), None);
        }
    }

    // --- expand / collapse / toggle semantics ---

    #[test]
    fn expand_not_loaded_schema_starts_a_request_and_shows_loading_row() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![schema_node("public")]);
        state.rebuild_rows();

        let req = state.command(TreeCommand::Expand);
        let Some(TreeRequest::Tables { id, schema }) = req else {
            panic!("expected a Tables request, got {req:?}");
        };
        assert_eq!(schema, "public");

        match &state.tree.schemas {
            Load::Loaded(schemas) => {
                assert!(schemas[0].expanded);
                assert!(matches!(schemas[0].tables, Load::Loading { id: got } if got == id));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }

        let rows = state.rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows[1].kind,
            TreeRowKind::Status(StatusKind::Loading)
        ));
        assert_eq!(
            rows[1].key,
            TreeRowKey::Status(Some(NodeKey::Schema {
                schema: "public".into()
            }))
        );
    }

    #[test]
    fn expand_schema_with_already_loaded_tables_shows_children_without_new_request() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: false,
            tables: Load::Loaded(vec![table_node("t1")]),
        }]);
        state.rebuild_rows();

        let req = state.command(TreeCommand::Expand);
        assert!(
            req.is_none(),
            "already-loaded children must not trigger a new request"
        );
        match &state.tree.schemas {
            Load::Loaded(schemas) => assert!(schemas[0].expanded),
            other => panic!("expected Loaded, got {other:?}"),
        }

        let rows = state.rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[1].kind, TreeRowKind::Table { .. }));
    }

    // Expanding an already-expanded node with loaded children moves selection
    // to the first child, since there's nothing left to load and a plain
    // no-op would leave the Expand key feeling unresponsive.
    #[test]
    fn expand_on_already_expanded_node_moves_selection_to_first_child() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Loaded(vec![table_node("t1"), table_node("t2")]),
        }]);
        state.rebuild_rows();
        assert_eq!(state.selected_index(), Some(0));

        let req = state.command(TreeCommand::Expand);
        assert!(req.is_none());
        assert_eq!(state.selected_index(), Some(1));
    }

    #[test]
    fn collapse_then_reexpand_does_not_refetch_loaded_children() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Loaded(vec![table_node("t1")]),
        }]);
        state.rebuild_rows();
        assert_eq!(state.rows().len(), 2);

        let req = state.command(TreeCommand::Collapse);
        assert!(req.is_none());
        match &state.tree.schemas {
            Load::Loaded(schemas) => {
                assert!(!schemas[0].expanded, "Collapse must clear `expanded`");
                assert!(
                    matches!(schemas[0].tables, Load::Loaded(_)),
                    "Collapse must not clear the Load field"
                );
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
        assert_eq!(state.rows().len(), 1);

        let req = state.command(TreeCommand::Expand);
        assert!(
            req.is_none(),
            "re-expanding retained, already-loaded children must not re-issue a request"
        );
        assert_eq!(state.rows().len(), 2);
    }

    // Collapse on an already-collapsed root schema is a no-op: a schema is a
    // top-level row, so there is no parent row to move selection to.
    #[test]
    fn collapse_on_collapsed_root_schema_is_a_noop() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![schema_node("a")]);
        state.rebuild_rows();

        let req = state.command(TreeCommand::Collapse);
        assert!(req.is_none());
        assert_eq!(state.selected_index(), Some(0));
    }

    // Collapse on an already-collapsed table moves selection up to its parent
    // schema, so repeated Collapse presses walk back up the tree.
    #[test]
    fn collapse_on_collapsed_table_moves_selection_to_parent_schema() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Loaded(vec![table_node("t1")]),
        }]);
        state.rebuild_rows();

        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(state.selected_index(), Some(1));

        let req = state.command(TreeCommand::Collapse);
        assert!(req.is_none());
        assert_eq!(state.selected_index(), Some(0));
    }

    // Collapse on a leaf column row moves selection up to its parent table,
    // consistent with Collapse walking up the tree one level at a time.
    #[test]
    fn collapse_on_column_leaf_moves_selection_to_parent_table() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Loaded(vec![TableNode {
                table: table("t1"),
                expanded: true,
                columns: Load::Loaded(vec![sample_column("c1")]),
            }]),
        }]);
        state.rebuild_rows();

        assert!(state.command(TreeCommand::Last).is_none());
        assert_eq!(state.selected_index(), Some(2));

        let req = state.command(TreeCommand::Collapse);
        assert!(req.is_none());
        assert_eq!(state.selected_index(), Some(1));
    }

    #[test]
    fn toggle_on_error_status_row_refreshes_parent_with_new_request() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Failed {
                message: "boom".into(),
            },
        }]);
        state.rebuild_rows();

        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(state.selected_index(), Some(1));

        let req = state.command(TreeCommand::Toggle);
        let Some(TreeRequest::Tables { id, schema }) = req else {
            panic!("expected a Tables request, got {req:?}");
        };
        assert_eq!(schema, "public");
        match &state.tree.schemas {
            Load::Loaded(schemas) => {
                assert!(matches!(schemas[0].tables, Load::Loading { id: got } if got == id))
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    // Regression test for a bug found in manual testing: refreshing a
    // collapsed schema updated its `Load` field to `Loading` but left
    // `expanded` false, so the loading/result state was invisible until the
    // user separately pressed Expand.
    #[test]
    fn refresh_on_collapsed_schema_expands_it() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![schema_node("public")]);
        state.rebuild_rows();
        assert_eq!(state.selected_index(), Some(0));

        let req = state.command(TreeCommand::Refresh);
        let Some(TreeRequest::Tables { id, schema }) = req else {
            panic!("expected a Tables request, got {req:?}");
        };
        assert_eq!(schema, "public");
        match &state.tree.schemas {
            Load::Loaded(schemas) => {
                assert!(schemas[0].expanded, "Refresh must set expanded = true");
                assert!(matches!(schemas[0].tables, Load::Loading { id: got } if got == id));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    // Same bug as above, for a collapsed table node nested under an
    // already-expanded schema.
    #[test]
    fn refresh_on_collapsed_table_expands_it() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![SchemaNode {
            schema: schema("public"),
            expanded: true,
            tables: Load::Loaded(vec![table_node("t1")]),
        }]);
        state.rebuild_rows();

        let _ = state.command(TreeCommand::MoveDown);
        assert_eq!(state.selected_index(), Some(1));

        let req = state.command(TreeCommand::Refresh);
        let Some(TreeRequest::Columns { id, schema, table }) = req else {
            panic!("expected a Columns request, got {req:?}");
        };
        assert_eq!(schema, "public");
        assert_eq!(table, "t1");
        match &state.tree.schemas {
            Load::Loaded(schemas) => match &schemas[0].tables {
                Load::Loaded(tables) => {
                    assert!(tables[0].expanded, "Refresh must set expanded = true");
                    assert!(matches!(tables[0].columns, Load::Loading { id: got } if got == id));
                }
                other => panic!("expected Loaded, got {other:?}"),
            },
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    // The "refresh mid-flight" shape from the design: a Loading{id=1}
    // request is superseded by a Refresh that mints id=2; a stale response
    // for id=1 must be dropped and the node must remain Loading{id=2}.
    #[test]
    fn refresh_mid_flight_makes_the_superseded_response_ignored() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![schema_node("public")]);
        state.rebuild_rows();

        let req1 = state.command(TreeCommand::Expand);
        let Some(TreeRequest::Tables { id: id1, .. }) = req1 else {
            panic!("expected a Tables request, got {req1:?}");
        };

        let req2 = state.command(TreeCommand::Refresh);
        let Some(TreeRequest::Tables { id: id2, .. }) = req2 else {
            panic!("expected a Tables request, got {req2:?}");
        };
        assert_ne!(id1, id2);

        state.apply(TreeResponse::Tables {
            id: id1,
            schema: "public".into(),
            result: Ok(vec![]),
        });
        match &state.tree.schemas {
            Load::Loaded(schemas) => assert!(
                matches!(schemas[0].tables, Load::Loading { id } if id == id2),
                "stale response for id1 must not overwrite the in-flight id2 load"
            ),
            other => panic!("expected Loaded, got {other:?}"),
        }

        state.apply(TreeResponse::Tables {
            id: id2,
            schema: "public".into(),
            result: Ok(vec![]),
        });
        match &state.tree.schemas {
            Load::Loaded(schemas) => {
                assert!(matches!(schemas[0].tables, Load::Loaded(ref t) if t.is_empty()))
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    // --- `apply` correctness beyond staleness ---

    #[test]
    fn apply_with_empty_result_shows_empty_status_row() {
        let mut state = ObjectTreeState::new();
        let TreeRequest::Schemas { id } = state.refresh_root() else {
            unreachable!()
        };
        state.apply(TreeResponse::Schemas {
            id,
            result: Ok(vec![]),
        });

        let rows = state.rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0].kind,
            TreeRowKind::Status(StatusKind::Empty)
        ));
        assert_eq!(rows[0].key, TreeRowKey::Status(None));
    }

    #[test]
    fn apply_with_error_result_builds_load_failed_via_full_error_chain() {
        let mut state = ObjectTreeState::new();
        let TreeRequest::Schemas { id } = state.refresh_root() else {
            unreachable!()
        };

        let make_err = || DataSourceError::Tunnel {
            name: "vps".into(),
            source: TunnelError::PortReservation {
                source: std::io::Error::other("no ports left"),
            },
        };
        let expected = crate::ui::error_chain(&make_err());

        state.apply(TreeResponse::Schemas {
            id,
            result: Err(make_err()),
        });

        match &state.tree.schemas {
            Load::Failed { message } => {
                assert_eq!(message, &expected);
                // Confirm the chain includes both the top-level message and
                // the #[source] level, not just the former.
                assert!(message.contains("failed to open the SSH tunnel"));
                assert!(message.contains("failed to reserve a local port"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn toggle_system_schemas_shows_and_hides_without_a_new_request() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![
            SchemaNode {
                schema: schema("public"),
                expanded: false,
                tables: Load::NotLoaded,
            },
            SchemaNode {
                schema: Schema {
                    name: "pg_catalog".into(),
                    is_system: true,
                },
                expanded: false,
                tables: Load::NotLoaded,
            },
        ]);
        state.rebuild_rows();
        assert_eq!(
            state.rows().len(),
            1,
            "system schemas are hidden by default"
        );

        let req = state.command(TreeCommand::ToggleSystemSchemas);
        assert!(req.is_none(), "toggling visibility is a pure rebuild");
        assert_eq!(state.rows().len(), 2);

        let req = state.command(TreeCommand::ToggleSystemSchemas);
        assert!(req.is_none());
        assert_eq!(state.rows().len(), 1);
    }

    // --- row rendering / rebuild ordering ---

    #[test]
    fn rows_flattens_nested_tree_depth_first_with_correct_depths() {
        let mut state = ObjectTreeState::new();
        state.tree.schemas = Load::Loaded(vec![
            SchemaNode {
                schema: schema("s1"),
                expanded: true,
                tables: Load::Loaded(vec![
                    TableNode {
                        table: table("t1"),
                        expanded: true,
                        columns: Load::Loaded(vec![sample_column("c1"), sample_column("c2")]),
                    },
                    TableNode {
                        table: table("t2"),
                        expanded: false,
                        columns: Load::NotLoaded,
                    },
                ]),
            },
            SchemaNode {
                schema: schema("s2"),
                expanded: false,
                tables: Load::NotLoaded,
            },
        ]);
        state.rebuild_rows();

        let summary: Vec<(u16, &str)> = state
            .rows()
            .iter()
            .map(|r| (r.depth, row_label(r)))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, "s1"),
                (1, "t1"),
                (2, "c1"),
                (2, "c2"),
                (1, "t2"),
                (0, "s2"),
            ]
        );
    }
}
