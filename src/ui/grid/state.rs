use ratatui::widgets::TableState;

use crate::ui::Load;
use crate::ui::grid::message::{GridRequest, GridResponse};
use crate::ui::grid::page::{self, Page};

pub enum GridCommand {
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    First,
    Last,
    ScrollLeft,
    ScrollRight,
    NextPage,
    PrevPage,
    Refresh,
}

struct Target {
    schema: String,
    table: String,
}

pub struct DataGridState {
    target: Option<Target>,
    offset: u64,
    page: Load<Page>,
    table: TableState,
    col_offset: usize,
    next_request_id: u64,
    viewport_height: u16,
}

impl DataGridState {
    pub fn new() -> Self {
        Self {
            target: None,
            offset: 0,
            page: Load::NotLoaded,
            table: TableState::default(),
            col_offset: 0,
            next_request_id: 0,
            viewport_height: 10,
        }
    }

    pub fn is_open(&self) -> bool {
        self.target.is_some()
    }

    pub fn target(&self) -> Option<(&str, &str)> {
        self.target
            .as_ref()
            .map(|t| (t.schema.as_str(), t.table.as_str()))
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn page(&self) -> &Load<Page> {
        &self.page
    }

    pub fn open(&mut self, schema: String, table: String) -> GridRequest {
        self.target = Some(Target {
            schema: schema.clone(),
            table: table.clone(),
        });
        self.offset = 0;
        self.col_offset = 0;
        self.table = TableState::default().with_selected(Some(0));
        let id = self.next_id();
        self.page = Load::Loading { id };
        GridRequest::Page {
            id,
            schema,
            table,
            offset: 0,
        }
    }

    pub fn command(&mut self, cmd: GridCommand) -> Option<GridRequest> {
        let target = self.target.as_ref()?;

        match cmd {
            GridCommand::MoveUp => {
                self.move_selection(-1);
                None
            }
            GridCommand::MoveDown => {
                self.move_selection(1);
                None
            }
            GridCommand::PageUp => {
                self.move_selection(-(self.viewport_height as isize));
                None
            }
            GridCommand::PageDown => {
                self.move_selection(self.viewport_height as isize);
                None
            }
            GridCommand::First => {
                if let Load::Loaded(page) = &self.page
                    && !page.rows.is_empty()
                {
                    self.table.select(Some(0));
                }
                None
            }
            GridCommand::Last => {
                if let Load::Loaded(page) = &self.page
                    && !page.rows.is_empty()
                {
                    self.table.select(Some(page.rows.len() - 1));
                }
                None
            }
            GridCommand::ScrollLeft => {
                self.col_offset = self.col_offset.saturating_sub(1);
                None
            }
            GridCommand::ScrollRight => {
                if let Load::Loaded(page) = &self.page
                    && !page.columns.is_empty()
                {
                    self.col_offset = (self.col_offset + 1).min(page.columns.len() - 1);
                }
                None
            }
            GridCommand::NextPage => {
                let has_next = matches!(&self.page, Load::Loaded(p) if p.has_next);
                if !has_next {
                    return None;
                }
                let schema = target.schema.clone();
                let table = target.table.clone();
                let offset = page::next_offset(self.offset);
                self.offset = offset;
                // `col_offset` deliberately carries over across pages: the
                // user's horizontal scroll position reflects which columns
                // they care about, and that's independent of which page of
                // rows is showing. Row selection has no equivalent
                // carry-over, since row N on the old page and row N on the
                // new page are unrelated data.
                self.table = TableState::default().with_selected(Some(0));
                let id = self.next_id();
                self.page = Load::Loading { id };
                Some(GridRequest::Page {
                    id,
                    schema,
                    table,
                    offset,
                })
            }
            GridCommand::PrevPage => {
                if self.offset == 0 || matches!(self.page, Load::Loading { .. }) {
                    return None;
                }
                let schema = target.schema.clone();
                let table = target.table.clone();
                let offset = page::prev_offset(self.offset);
                self.offset = offset;
                self.table = TableState::default().with_selected(Some(0));
                let id = self.next_id();
                self.page = Load::Loading { id };
                Some(GridRequest::Page {
                    id,
                    schema,
                    table,
                    offset,
                })
            }
            GridCommand::Refresh => {
                if matches!(self.page, Load::Loading { .. }) {
                    return None;
                }
                let schema = target.schema.clone();
                let table = target.table.clone();
                let offset = self.offset;
                let id = self.next_id();
                self.page = Load::Loading { id };
                Some(GridRequest::Page {
                    id,
                    schema,
                    table,
                    offset,
                })
            }
        }
    }

    pub fn apply(&mut self, response: GridResponse) {
        let GridResponse::Page {
            id,
            schema,
            table,
            result,
            ..
        } = response;

        if !matches!(&self.page, Load::Loading { id: current } if *current == id) {
            return;
        }
        let target_matches = self
            .target
            .as_ref()
            .is_some_and(|t| t.schema == schema && t.table == table);
        if !target_matches {
            return;
        }

        match result {
            Ok(page) => {
                self.table
                    .select(if page.rows.is_empty() { None } else { Some(0) });
                self.page = Load::Loaded(page);
            }
            Err(e) => {
                self.page = Load::Failed {
                    message: crate::ui::error_chain(&e),
                };
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Load::Loaded(page) = &self.page else {
            return;
        };
        if page.rows.is_empty() {
            return;
        }
        let len = page.rows.len();
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        self.table.select(Some(next as usize));
    }

    pub(crate) fn set_viewport_height(&mut self, h: u16) {
        self.viewport_height = h;
    }

    /// Splits the borrow at the field level so rendering code can hold an
    /// immutable reference to the loaded page alongside a mutable reference
    /// to the table state (plus the current column scroll offset) without
    /// having to clone the page every frame.
    pub(crate) fn parts(&mut self) -> (&Load<Page>, &mut TableState, usize) {
        (&self.page, &mut self.table, self.col_offset)
    }

    // Only test code inspects selection/scroll state directly; production
    // rendering goes through `parts()` above.
    #[cfg(test)]
    pub(crate) fn table_state_mut(&mut self) -> &mut TableState {
        &mut self.table
    }

    #[cfg(test)]
    pub(crate) fn col_offset(&self) -> usize {
        self.col_offset
    }

    fn next_id(&mut self) -> crate::ui::RequestId {
        let id = crate::ui::RequestId(self.next_request_id);
        self.next_request_id += 1;
        id
    }
}

impl Default for DataGridState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::DataSourceError;

    fn page_with_rows(n: usize) -> Page {
        Page {
            columns: vec!["a".into(), "b".into(), "c".into()],
            rows: (0..n)
                .map(|i| vec![Some(i.to_string()), Some("x".into()), None])
                .collect(),
            has_next: false,
        }
    }

    fn open_and_load(state: &mut DataGridState, page: Page) {
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "t".into(),
            offset: 0,
            result: Ok(page),
        });
    }

    #[test]
    fn commands_are_noop_when_grid_not_open() {
        let mut state = DataGridState::new();
        assert!(state.command(GridCommand::MoveDown).is_none());
        assert!(state.command(GridCommand::NextPage).is_none());
        assert!(matches!(state.page(), Load::NotLoaded));
    }

    #[test]
    fn move_down_clamps_at_last_row() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(3));
        for _ in 0..10 {
            state.command(GridCommand::MoveDown);
        }
        assert_eq!(state.table_state_mut().selected(), Some(2));
    }

    #[test]
    fn move_up_clamps_at_zero() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(3));
        state.command(GridCommand::MoveUp);
        assert_eq!(state.table_state_mut().selected(), Some(0));
    }

    #[test]
    fn move_on_empty_page_is_noop() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(0));
        assert!(state.command(GridCommand::MoveDown).is_none());
        assert_eq!(state.table_state_mut().selected(), None);
    }

    #[test]
    fn scroll_right_clamps_to_last_column() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(1));
        for _ in 0..10 {
            state.command(GridCommand::ScrollRight);
        }
        assert_eq!(state.col_offset(), 2);
    }

    #[test]
    fn scroll_left_saturates_at_zero() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(1));
        assert!(state.command(GridCommand::ScrollLeft).is_none());
        assert_eq!(state.col_offset(), 0);
    }

    #[test]
    fn next_page_is_noop_without_has_next() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(3));
        assert!(state.command(GridCommand::NextPage).is_none());
        assert_eq!(state.offset(), 0);
    }

    #[test]
    fn next_page_issues_request_and_advances_offset_when_has_next() {
        let mut state = DataGridState::new();
        let mut page = page_with_rows(50);
        page.has_next = true;
        open_and_load(&mut state, page);

        let req = state.command(GridCommand::NextPage);
        assert!(req.is_some());
        assert_eq!(state.offset(), 50);
        assert!(matches!(state.page(), Load::Loading { .. }));
    }

    #[test]
    fn prev_page_is_noop_at_offset_zero() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(3));
        assert!(state.command(GridCommand::PrevPage).is_none());
    }

    #[test]
    fn prev_page_is_noop_while_a_fetch_is_already_in_flight() {
        let mut state = DataGridState::new();
        let mut page = page_with_rows(50);
        page.has_next = true;
        open_and_load(&mut state, page);

        assert!(state.command(GridCommand::NextPage).is_some());
        assert!(matches!(state.page(), Load::Loading { .. }));
        assert_eq!(state.offset(), 50);

        assert!(
            state.command(GridCommand::PrevPage).is_none(),
            "PrevPage must not issue a new request while one is already in flight"
        );
        assert_eq!(
            state.offset(),
            50,
            "PrevPage must not advance offset when it's a no-op"
        );
    }

    #[test]
    fn refresh_is_noop_while_a_fetch_is_already_in_flight() {
        let mut state = DataGridState::new();
        let mut page = page_with_rows(50);
        page.has_next = true;
        open_and_load(&mut state, page);

        assert!(state.command(GridCommand::NextPage).is_some());
        assert!(matches!(state.page(), Load::Loading { .. }));

        assert!(
            state.command(GridCommand::Refresh).is_none(),
            "Refresh must not issue a new request while one is already in flight"
        );
    }

    #[test]
    fn refresh_issues_request_when_not_loading() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(3));
        assert!(state.command(GridCommand::Refresh).is_some());
        assert!(matches!(state.page(), Load::Loading { .. }));
    }

    #[test]
    fn apply_ignores_response_with_stale_id() {
        let mut state = DataGridState::new();
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        // Simulate a second open (new id) before the first response arrives.
        let _ = state.open("public".into(), "t2".into());
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "t2".into(),
            offset: 0,
            result: Ok(page_with_rows(1)),
        });
        assert!(matches!(state.page(), Load::Loading { .. }));
    }

    #[test]
    fn apply_ignores_response_for_a_different_target_even_with_matching_id() {
        let mut state = DataGridState::new();
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        // Craft a response with the current (matching) request id but a
        // mismatched table name. In practice id and target always change
        // together, so this exact combination isn't reachable from real
        // `open`/`apply` traffic today — this only exercises the defensive
        // target-match check directly, in case that invariant ever changes.
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "other_table".into(),
            offset: 0,
            result: Ok(page_with_rows(1)),
        });
        assert!(matches!(state.page(), Load::Loading { .. }));
    }

    #[test]
    fn apply_accepts_matching_id_and_target() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(2));
        assert!(matches!(state.page(), Load::Loaded(_)));
        assert_eq!(state.table_state_mut().selected(), Some(0));
    }

    #[test]
    fn apply_error_sets_failed_state() {
        let mut state = DataGridState::new();
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "t".into(),
            offset: 0,
            result: Err(DataSourceError::Cancelled),
        });
        assert!(matches!(state.page(), Load::Failed { .. }));
    }
}
