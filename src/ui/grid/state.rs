use ratatui::widgets::TableState;

use crate::ui::Load;
use crate::ui::grid::message::{GridRequest, GridResponse};
use crate::ui::grid::page::{self, GridContent};

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

pub enum GridOrigin {
    Table { schema: String, table: String },
    Query { title: String },
}

pub struct DataGridState {
    origin: Option<GridOrigin>,
    offset: u64,
    content: Load<GridContent>,
    table: TableState,
    col_offset: usize,
    next_request_id: u64,
    viewport_height: u16,
}

impl DataGridState {
    pub fn new() -> Self {
        Self {
            origin: None,
            offset: 0,
            content: Load::NotLoaded,
            table: TableState::default(),
            col_offset: 0,
            next_request_id: 0,
            viewport_height: 10,
        }
    }

    pub fn is_open(&self) -> bool {
        self.origin.is_some()
    }

    pub fn origin(&self) -> Option<&GridOrigin> {
        self.origin.as_ref()
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn content(&self) -> &Load<GridContent> {
        &self.content
    }

    pub fn open(&mut self, schema: String, table: String) -> GridRequest {
        self.origin = Some(GridOrigin::Table {
            schema: schema.clone(),
            table: table.clone(),
        });
        self.offset = 0;
        self.col_offset = 0;
        self.table = TableState::default().with_selected(Some(0));
        let id = self.next_id();
        self.content = Load::Loading { id };
        GridRequest::Page {
            id,
            schema,
            table,
            offset: 0,
        }
    }

    pub fn begin_query(&mut self, id: crate::ui::RequestId, title: String) {
        self.origin = Some(GridOrigin::Query { title });
        self.offset = 0;
        self.col_offset = 0;
        self.table = TableState::default().with_selected(Some(0));
        self.content = Load::Loading { id };
    }

    /// Accepts only if BOTH: `content` is currently `Loading { id }` for this
    /// exact `id`, AND `origin` is currently `Query` (not `Table`) -- mirrors
    /// `apply`'s dual staleness check (id + origin-kind match) below.
    ///
    /// The origin-kind half of this check is load-bearing, not defensive:
    /// `DataGridState`'s own id counter (used by `open`'s `next_id`) and
    /// `RunState`'s `next_request_id` counter are independent and both start
    /// at 0, so the very first table-open of a session and the very first
    /// query of a session mint the SAME `RequestId(0)`. Without the origin
    /// check, a stale Table-origin `RequestId(0)` response could be accepted
    /// here for a Query-origin `content: Loading(0)`, or vice versa in
    /// `apply` below -- this collision is routine, not a hypothetical edge
    /// case.
    pub fn finish_query(
        &mut self,
        id: crate::ui::RequestId,
        result: Result<&crate::app::run::QueryOutcome, &crate::datasource::DataSourceError>,
    ) -> bool {
        if !matches!(&self.content, Load::Loading { id: current } if *current == id) {
            return false;
        }
        if !matches!(self.origin, Some(GridOrigin::Query { .. })) {
            return false;
        }

        match result {
            Ok(crate::app::run::QueryOutcome::Rows(page)) => {
                self.table
                    .select(if page.rows.is_empty() { None } else { Some(0) });
                self.content = Load::Loaded(GridContent::Rows(page.clone()));
            }
            Ok(crate::app::run::QueryOutcome::NoResultSet { rows_affected }) => {
                self.table.select(None);
                self.content = Load::Loaded(GridContent::NoResultSet {
                    rows_affected: *rows_affected,
                });
            }
            Err(e) => {
                self.content = Load::Failed {
                    message: crate::ui::error_chain(e),
                };
            }
        }
        true
    }

    pub fn command(&mut self, cmd: GridCommand) -> Option<GridRequest> {
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
                if let Load::Loaded(GridContent::Rows(page)) = &self.content
                    && !page.rows.is_empty()
                {
                    self.table.select(Some(0));
                }
                None
            }
            GridCommand::Last => {
                if let Load::Loaded(GridContent::Rows(page)) = &self.content
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
                if let Load::Loaded(GridContent::Rows(page)) = &self.content
                    && !page.columns.is_empty()
                {
                    self.col_offset = (self.col_offset + 1).min(page.columns.len() - 1);
                }
                None
            }
            // NextPage/PrevPage/Refresh only make sense against a `Table`
            // origin: an ad-hoc query result has no `LIMIT/OFFSET` handle to
            // page against, and re-running it would mean holding a stream
            // alive across keypresses, which is explicitly out of scope.
            GridCommand::NextPage => {
                let Some(GridOrigin::Table { schema, table }) = &self.origin else {
                    return None;
                };
                let has_next =
                    matches!(&self.content, Load::Loaded(GridContent::Rows(p)) if p.has_next);
                if !has_next {
                    return None;
                }
                let schema = schema.clone();
                let table = table.clone();
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
                self.content = Load::Loading { id };
                Some(GridRequest::Page {
                    id,
                    schema,
                    table,
                    offset,
                })
            }
            GridCommand::PrevPage => {
                let Some(GridOrigin::Table { schema, table }) = &self.origin else {
                    return None;
                };
                if self.offset == 0 || matches!(self.content, Load::Loading { .. }) {
                    return None;
                }
                let schema = schema.clone();
                let table = table.clone();
                let offset = page::prev_offset(self.offset);
                self.offset = offset;
                self.table = TableState::default().with_selected(Some(0));
                let id = self.next_id();
                self.content = Load::Loading { id };
                Some(GridRequest::Page {
                    id,
                    schema,
                    table,
                    offset,
                })
            }
            GridCommand::Refresh => {
                let Some(GridOrigin::Table { schema, table }) = &self.origin else {
                    return None;
                };
                if matches!(self.content, Load::Loading { .. }) {
                    return None;
                }
                let schema = schema.clone();
                let table = table.clone();
                let offset = self.offset;
                let id = self.next_id();
                self.content = Load::Loading { id };
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

        if !matches!(&self.content, Load::Loading { id: current } if *current == id) {
            return;
        }
        let origin_matches = matches!(
            &self.origin,
            Some(GridOrigin::Table { schema: s, table: t }) if *s == schema && *t == table
        );
        if !origin_matches {
            return;
        }

        match result {
            Ok(page) => {
                self.table
                    .select(if page.rows.is_empty() { None } else { Some(0) });
                self.content = Load::Loaded(GridContent::Rows(page));
            }
            Err(e) => {
                self.content = Load::Failed {
                    message: crate::ui::error_chain(&e),
                };
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Load::Loaded(GridContent::Rows(page)) = &self.content else {
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
    /// immutable reference to the loaded content alongside a mutable
    /// reference to the table state (plus the current column scroll offset)
    /// without having to clone the content every frame.
    pub(crate) fn parts(&mut self) -> (&Load<GridContent>, &mut TableState, usize) {
        (&self.content, &mut self.table, self.col_offset)
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
    use crate::app::run::QueryOutcome;
    use crate::datasource::DataSourceError;
    use crate::ui::grid::page::Page;

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
        assert!(matches!(state.content(), Load::NotLoaded));
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
        assert!(matches!(state.content(), Load::Loading { .. }));
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
        assert!(matches!(state.content(), Load::Loading { .. }));
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
        assert!(matches!(state.content(), Load::Loading { .. }));

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
        assert!(matches!(state.content(), Load::Loading { .. }));
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
        assert!(matches!(state.content(), Load::Loading { .. }));
    }

    #[test]
    fn apply_ignores_response_for_a_different_target_even_with_matching_id() {
        let mut state = DataGridState::new();
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        // Craft a response with the current (matching) request id but a
        // mismatched table name. Within `DataGridState`'s own id counter, id
        // and target always change together, so THIS exact combination isn't
        // reachable from that counter alone. But the same target/origin
        // check this exercises is what makes `apply` safe against a
        // different, routinely-occurring collision: `DataGridState`'s id
        // counter and `RunState`'s are independent and both start at 0 (see
        // `finish_query`'s doc comment), so a stale Table-origin response can
        // arrive with the same `RequestId` as a currently-loading Query
        // origin. This test pins the mechanism directly rather than trying
        // to reproduce that interleaving end-to-end.
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "other_table".into(),
            offset: 0,
            result: Ok(page_with_rows(1)),
        });
        assert!(matches!(state.content(), Load::Loading { .. }));
    }

    #[test]
    fn apply_accepts_matching_id_and_target() {
        let mut state = DataGridState::new();
        open_and_load(&mut state, page_with_rows(2));
        assert!(matches!(
            state.content(),
            Load::Loaded(GridContent::Rows(_))
        ));
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
        assert!(matches!(state.content(), Load::Failed { .. }));
    }

    // --- GridOrigin::Query path (begin_query / finish_query) ---

    #[test]
    fn begin_query_sets_query_origin_and_loading() {
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "SELECT 1".into());
        assert!(matches!(state.origin(), Some(GridOrigin::Query { .. })));
        assert!(matches!(state.content(), Load::Loading { id: got } if *got == id));
    }

    #[test]
    fn finish_query_accepts_matching_id_and_query_origin_with_rows() {
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "SELECT 1".into());
        let outcome = QueryOutcome::Rows(page_with_rows(2));
        let displayed = state.finish_query(id, Ok(&outcome));
        assert!(
            displayed,
            "a matching id and Query origin must report the result as displayed"
        );
        assert!(matches!(
            state.content(),
            Load::Loaded(GridContent::Rows(_))
        ));
        assert_eq!(state.table_state_mut().selected(), Some(0));
    }

    #[test]
    fn finish_query_accepts_no_result_set_and_clears_selection() {
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "UPDATE t SET x = 1".into());
        let outcome = QueryOutcome::NoResultSet { rows_affected: 5 };
        let displayed = state.finish_query(id, Ok(&outcome));
        assert!(displayed);
        assert!(matches!(
            state.content(),
            Load::Loaded(GridContent::NoResultSet { rows_affected: 5 })
        ));
        assert_eq!(state.table_state_mut().selected(), None);
    }

    #[test]
    fn finish_query_reports_an_error_result_as_displayed_too() {
        // An error is still "displayed" (as Load::Failed) rather than
        // discarded -- only a stale id / mismatched origin makes it not
        // displayed, not the Ok/Err split of the result itself.
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "SELECT 1 / 0".into());
        let displayed = state.finish_query(id, Err(&DataSourceError::Cancelled));
        assert!(
            displayed,
            "an error result for a matching id/origin must be reported as displayed"
        );
        assert!(matches!(state.content(), Load::Failed { .. }));
    }

    #[test]
    fn finish_query_ignores_stale_id() {
        let mut state = DataGridState::new();
        let id1 = crate::ui::RequestId(1);
        state.begin_query(id1, "SELECT 1".into());
        let id2 = crate::ui::RequestId(2);
        state.begin_query(id2, "SELECT 2".into());
        let outcome = QueryOutcome::NoResultSet { rows_affected: 0 };
        let displayed = state.finish_query(id1, Ok(&outcome));
        assert!(
            !displayed,
            "a stale request id must not be reported as displayed"
        );
        assert!(matches!(state.content(), Load::Loading { id } if *id == id2));
    }

    #[test]
    fn finish_query_ignores_response_when_origin_switched_to_table() {
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "SELECT 1".into());
        // `DataGridState`'s own id counter (Table opens) and `RunState`'s id
        // counter (queries) are independent and both start at 0, so a
        // Table-origin `RequestId(0)` and a Query-origin `RequestId(0)`
        // routinely collide -- e.g. the very first table-open and the very
        // first query of a session. The origin-kind check below is what
        // rejects that collision; it is load-bearing, not defensive. This
        // test pins the mechanism directly, standing in for the id 0
        // collision above with an explicit origin swap.
        let outcome = QueryOutcome::NoResultSet { rows_affected: 0 };
        state.origin = Some(GridOrigin::Table {
            schema: "public".into(),
            table: "t".into(),
        });
        let displayed = state.finish_query(id, Ok(&outcome));
        assert!(
            !displayed,
            "a Query result arriving after the origin switched to Table must not be reported as \
             displayed"
        );
        assert!(matches!(state.content(), Load::Loading { .. }));
    }

    #[test]
    fn finish_query_never_overwrites_already_visible_table_rows_with_a_stale_query_result() {
        // The concrete safety property behind the bool return: once the grid
        // is genuinely showing table data (not just mid-request), a late
        // query result must leave that data completely untouched, not just
        // report `false`.
        let mut state = DataGridState::new();
        let table_page = page_with_rows(3);
        open_and_load(&mut state, table_page.clone());
        assert!(matches!(
            state.content(),
            Load::Loaded(GridContent::Rows(p)) if *p == table_page
        ));

        let outcome = QueryOutcome::NoResultSet { rows_affected: 999 };
        let displayed = state.finish_query(crate::ui::RequestId(0), Ok(&outcome));
        assert!(!displayed);
        assert!(
            matches!(state.content(), Load::Loaded(GridContent::Rows(p)) if *p == table_page),
            "the visible table rows must be unchanged by the discarded query result, got {:?}",
            state.content()
        );
    }

    #[test]
    fn table_paging_commands_are_noop_for_query_origin() {
        let mut state = DataGridState::new();
        let id = crate::ui::RequestId(1);
        state.begin_query(id, "SELECT 1".into());
        let mut page = page_with_rows(50);
        page.has_next = true;
        state.finish_query(id, Ok(&QueryOutcome::Rows(page)));

        assert!(state.command(GridCommand::NextPage).is_none());
        assert!(state.command(GridCommand::PrevPage).is_none());
        assert!(state.command(GridCommand::Refresh).is_none());
    }
}
