pub mod keymap;
pub mod message;
pub mod run;
pub mod status;
pub mod worker;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::editor::{Motion, RunTarget};
use crate::ui::editor::EditorState;
use crate::ui::editor::widget::EditorWidget;
use crate::ui::grid::state::DataGridState;
use crate::ui::grid::widget::DataGridWidget;
use crate::ui::tree::model::NodeKey;
use crate::ui::tree::state::{ObjectTreeState, TreeCommand, TreeRowKey};
use crate::ui::tree::widget::ObjectTreeWidget;
use keymap::{AppCommand, RunKey, map_key};
use message::{WorkerRequest, WorkerResponse};
use run::{CancelOutcome, CancelRequest, RunOutcome, RunState, RunSummary};
use status::{Status, StatusKind};

const TREE_FOOTER: &str =
    "↑/↓ move  →/← expand/collapse  ⏎ open/toggle  r refresh  . system  Tab grid  q quit";
const GRID_FOOTER: &str = "↑/↓ move  ←/→ scroll cols  PgUp/PgDn page  n/p next/prev page  r refresh  Esc/Tab tree  q quit";
const EDITOR_FOOTER: &str = "type to edit  Ctrl+R run cursor/selection  Ctrl+E run buffer  \
                              Ctrl+A select all  Esc clear selection  Ctrl+C cancel/quit  Tab tree/grid";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Grid,
    Editor,
}

pub struct App {
    tree: ObjectTreeState,
    grid: DataGridState,
    editor: EditorState,
    run: RunState,
    status: Option<Status>,
    focus: Focus,
    requests: UnboundedSender<WorkerRequest>,
    responses: UnboundedReceiver<WorkerResponse>,
    cancels: UnboundedSender<CancelRequest>,
    connection_name: String,
    should_quit: bool,
}

impl App {
    pub fn new(
        connection_name: String,
        requests: UnboundedSender<WorkerRequest>,
        responses: UnboundedReceiver<WorkerResponse>,
        cancels: UnboundedSender<CancelRequest>,
    ) -> Self {
        Self {
            tree: ObjectTreeState::new(),
            grid: DataGridState::new(),
            editor: EditorState::new(),
            run: RunState::new(),
            status: None,
            focus: Focus::Tree,
            requests,
            responses,
            cancels,
            connection_name,
            should_quit: false,
        }
    }

    pub fn start(&mut self) {
        let req = self.tree.refresh_root();
        let _ = self.requests.send(WorkerRequest::Tree(req));
    }

    pub fn apply(&mut self, response: WorkerResponse) {
        match response {
            WorkerResponse::Tree(r) => self.tree.apply(r),
            WorkerResponse::Grid(r) => self.grid.apply(r),
            WorkerResponse::Query(r) => self.apply_query_response(r),
        }
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        let cmd = map_key(key, self.focus);
        // Cleared on every keypress except a run/cancel (those manage
        // `self.status` themselves -- e.g. "cancelling…" -- and would
        // otherwise have their own message wiped out by this blanket clear
        // one line after setting it) and except while a run is active (the
        // in-progress status set by `start_run`/`apply_query_response` must
        // survive keypresses that don't touch the run, e.g. scrolling the
        // grid while a query runs).
        if !matches!(
            cmd,
            Some(AppCommand::Run(_)) | Some(AppCommand::CancelOrQuit)
        ) && !self.run.is_active()
        {
            self.status = None;
        }

        match cmd {
            Some(AppCommand::Quit) => self.should_quit = true,
            Some(AppCommand::ToggleFocus) => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Editor,
                    Focus::Editor => {
                        if self.grid.is_open() {
                            Focus::Grid
                        } else {
                            Focus::Tree
                        }
                    }
                    Focus::Grid => Focus::Tree,
                };
            }
            Some(AppCommand::FocusTree) => self.focus = Focus::Tree,
            Some(AppCommand::Activate) => self.activate(),
            Some(AppCommand::Tree(cmd)) => {
                if let Some(req) = self.tree.command(cmd) {
                    let _ = self.requests.send(WorkerRequest::Tree(req));
                }
            }
            Some(AppCommand::Grid(cmd)) => {
                if let Some(req) = self.grid.command(cmd) {
                    let _ = self.requests.send(WorkerRequest::Grid(req));
                }
            }
            Some(AppCommand::Editor(cmd)) => {
                self.editor.command(cmd);
            }
            Some(AppCommand::Run(key)) => {
                let target = match key {
                    RunKey::CursorOrSelection => {
                        if self.editor.buffer().selection().is_some() {
                            RunTarget::Selection
                        } else {
                            RunTarget::Cursor
                        }
                    }
                    RunKey::Buffer => RunTarget::Buffer,
                };
                self.start_run(target);
            }
            Some(AppCommand::CancelOrQuit) => match self.run.request_cancel() {
                Some(req) => {
                    let _ = self.cancels.send(req);
                    self.status = Some(Status::info("cancelling…"));
                }
                None if self.run.is_active() => {
                    self.status = Some(Status::info("cancelling…"));
                }
                None => self.should_quit = true,
            },
            None => {}
        }
    }

    pub fn start_run(&mut self, target: RunTarget) {
        if self.run.is_active() {
            self.status = Some(Status::info("a query is already running"));
            return;
        }
        match crate::editor::plan_run(self.editor.buffer(), target) {
            Err(split_err) => {
                self.status = Some(Status::error(split_err.message.clone()));
                let pos = self.editor.buffer().position_of(split_err.span.start);
                self.editor.buffer_mut().move_to(pos, Motion::Move);
            }
            Ok(units) if units.is_empty() => {
                self.status = Some(Status::info("nothing to run"));
            }
            Ok(units) => {
                if let Some(req) = self.run.start(units) {
                    self.grid.begin_query(req.id, title_of(&req.sql));
                    let _ = self.requests.send(WorkerRequest::Query(req));
                    self.status = Some(running_status(&self.run));
                }
            }
        }
    }

    fn apply_query_response(&mut self, r: run::QueryResponse) {
        use run::QueryResponse;
        match r {
            QueryResponse::Started { id, query_id } => {
                if let Some(req) = self.run.on_started(id, query_id) {
                    let _ = self.cancels.send(req);
                }
            }
            QueryResponse::Finished { id, result } => {
                if !self.run.owns(id) {
                    return;
                }
                let displayed = self.grid.finish_query(id, result.as_ref());
                if let Err(e) = &result
                    && let Some(unit) = self.run.current().cloned()
                {
                    self.jump_to_error_position(&unit, e);
                }
                match self.run.on_finished(id, &result) {
                    Some(RunOutcome::Next(req)) => {
                        // No `displayed`-driven note here (unlike the `Done`
                        // branch below): `begin_query` unconditionally moves
                        // the grid onto this next statement's loading state
                        // regardless of `displayed`, so a "result not shown
                        // (grid moved on)" note would be backwards here -- it
                        // would describe the grid as having stayed on a table
                        // view it is, in this very branch, about to clobber.
                        self.grid.begin_query(req.id, title_of(&req.sql));
                        let _ = self.requests.send(WorkerRequest::Query(req));
                        self.status = Some(running_status(&self.run));
                    }
                    Some(RunOutcome::Done(summary)) => {
                        let mut status = summary_status(&summary);
                        if !displayed {
                            status.text.push_str(" · result not shown (grid moved on)");
                        }
                        self.status = Some(status);
                    }
                    None => {}
                }
            }
            QueryResponse::CancelFailed { id, message } => {
                if self.run.owns(id) {
                    self.status = Some(Status::error(message));
                }
            }
        }
    }

    fn jump_to_error_position(
        &mut self,
        unit: &crate::editor::RunUnit,
        err: &crate::datasource::DataSourceError,
    ) {
        let Some(pos) = err.error_position() else {
            return;
        };
        let text = self.editor.buffer().text();
        let Some(offset) = error_offset(&text, unit, pos) else {
            return;
        };
        let buffer_pos = self.editor.buffer().position_of(offset);
        self.editor.buffer_mut().move_to(buffer_pos, Motion::Move);
    }

    fn activate(&mut self) {
        if let Some(row) = self.tree.selected()
            && let TreeRowKey::Node(NodeKey::Table { schema, table }) = &row.key
        {
            let (schema, table) = (schema.clone(), table.clone());
            let req = self.grid.open(schema, table);
            self.focus = Focus::Grid;
            let _ = self.requests.send(WorkerRequest::Grid(req));
            return;
        }
        if let Some(req) = self.tree.command(TreeCommand::Toggle) {
            let _ = self.requests.send(WorkerRequest::Tree(req));
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

        let panes = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(chunks[0]);
        let tree_area = panes[0];
        let right_area = panes[1];

        let (editor_area, grid_area) = if self.grid.is_open() {
            let rows = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(right_area);
            (rows[0], Some(rows[1]))
        } else {
            (right_area, None)
        };

        let tree_style = pane_border_style(self.focus == Focus::Tree);
        let tree_block = Block::bordered()
            .border_style(tree_style)
            .title(format!(" ratwarren — {} ", self.connection_name));
        let tree_widget = ObjectTreeWidget::new()
            .block(tree_block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(tree_widget, tree_area, &mut self.tree);

        let editor_style = pane_border_style(self.focus == Focus::Editor);
        let editor_block = Block::bordered()
            .border_style(editor_style)
            .title(" editor ");
        let editor_inner = editor_block.inner(editor_area);
        let editor_widget = EditorWidget::new().block(editor_block);
        frame.render_stateful_widget(editor_widget, editor_area, &mut self.editor);
        if self.focus == Focus::Editor
            && let Some(pos) = self.editor.cursor_screen_pos(editor_inner)
        {
            frame.set_cursor_position(pos);
        }

        if let Some(grid_area) = grid_area {
            let grid_style = pane_border_style(self.focus == Focus::Grid);
            let grid_block = Block::bordered().border_style(grid_style);
            let grid_widget = DataGridWidget::new()
                .block(grid_block)
                .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            frame.render_stateful_widget(grid_widget, grid_area, &mut self.grid);
        }

        match &self.status {
            Some(status) => {
                let style = match status.kind {
                    StatusKind::Info => Style::default(),
                    StatusKind::Error => Style::default().fg(Color::Red),
                    StatusKind::Warn => Style::default().fg(Color::Yellow),
                };
                let footer = Paragraph::new(status.text.clone()).style(style);
                frame.render_widget(footer, chunks[1]);
            }
            None => {
                let footer_text = match self.focus {
                    Focus::Tree => TREE_FOOTER,
                    Focus::Grid => GRID_FOOTER,
                    Focus::Editor => EDITOR_FOOTER,
                };
                let footer = Paragraph::new(footer_text);
                frame.render_widget(footer, chunks[1]);
            }
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

/// Byte offset in `text` of a 1-based, character-counted server error
/// position reported for `unit`. `None` when the buffer no longer matches
/// what was actually sent (edited while the query was in flight -- including
/// edited so far down that `unit.span` no longer fits at all), or when the
/// position is outside the statement.
fn error_offset(text: &str, unit: &crate::editor::RunUnit, pos: u32) -> Option<usize> {
    let idx = (pos as usize).checked_sub(1)?;
    if unit.span.try_slice(text)? != unit.sql {
        return None;
    }
    let (byte, _) = unit.sql.char_indices().nth(idx)?;
    Some(unit.span.start + byte)
}

fn title_of(sql: &str) -> String {
    let first_line = sql.lines().next().unwrap_or("");
    const MAX: usize = 40;
    if first_line.chars().count() > MAX {
        format!("{}…", first_line.chars().take(MAX).collect::<String>())
    } else {
        first_line.to_string()
    }
}

fn running_status(run: &RunState) -> Status {
    let (n, total) = run.progress();
    if total <= 1 {
        Status::info("running…")
    } else {
        Status::info(format!("running statement {n} of {total}…"))
    }
}

fn summary_status(summary: &RunSummary) -> Status {
    match summary.cancelled {
        Some(CancelOutcome::Interrupted) => Status::info(format!(
            "cancelled — statement {} of {} interrupted",
            summary.ran, summary.total
        )),
        Some(CancelOutcome::CompletedFirst) if summary.ran < summary.total => {
            Status::warn(format!(
                "cancel came too late — statement {} of {} completed; stopped before statement {}",
                summary.ran,
                summary.total,
                summary.ran + 1
            ))
        }
        Some(CancelOutcome::CompletedFirst) => Status::warn(format!(
            "cancel came too late — statement {} of {} completed",
            summary.ran, summary.total
        )),
        None => {
            if let Some(err) = &summary.failed {
                Status::error(format!(
                    "statement {} of {} failed: {}",
                    summary.ran, summary.total, err
                ))
            } else if let Some(n) = summary.last_affected {
                Status::info(format!(
                    "ran {} of {} · {n} rows affected",
                    summary.ran, summary.total
                ))
            } else {
                Status::info(format!("ran {} of {}", summary.ran, summary.total))
            }
        }
    }
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

pub async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    use futures_util::StreamExt;

    let mut events = crossterm::event::EventStream::new();
    app.start();
    loop {
        terminal.draw(|f| app.render(f))?;

        tokio::select! {
            event = events.next() => match event {
                Some(Ok(crossterm::event::Event::Key(k)))
                    if k.kind == crossterm::event::KeyEventKind::Press =>
                {
                    app.on_key(k)
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            response = app.responses.recv() => match response {
                Some(r) => app.apply(r),
                None => return Err(std::io::Error::other("datasource worker stopped")),
            },
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::DataSourceError;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc::unbounded_channel;

    // These live inside `app`'s own module tree specifically so they can
    // reach `App`'s private fields (status/editor/run) directly, the same
    // way `App::on_key`/`start_run` themselves do -- no new pub/pub(crate)
    // accessor was added to support this.
    fn new_app() -> (
        App,
        UnboundedReceiver<WorkerRequest>,
        UnboundedReceiver<CancelRequest>,
    ) {
        let (req_tx, req_rx) = unbounded_channel();
        let (_resp_tx, resp_rx) = unbounded_channel();
        let (cancel_tx, cancel_rx) = unbounded_channel();
        let app = App::new("test".to_string(), req_tx, resp_rx, cancel_tx);
        (app, req_rx, cancel_rx)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn a_non_run_non_cancel_key_clears_a_stale_status() {
        let (mut app, _req_rx, _cancel_rx) = new_app();
        app.status = Some(Status::error("stale error from a previous run"));
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            app.status.is_none(),
            "any key other than Run/CancelOrQuit must clear the status bar"
        );
    }

    #[test]
    fn cancel_or_quit_key_does_not_blanket_clear_a_pre_existing_status_before_quitting() {
        // The one branch of CancelOrQuit that never writes `status` itself
        // (no active run, so it just quits) is the only place where the
        // "except Run/CancelOrQuit" exclusion is actually observable: every
        // other branch of Run/CancelOrQuit overwrites `status` unconditionally
        // regardless of the exclusion, so this is the precise case that
        // pins the documented rule rather than an outcome that would hold
        // either way.
        let (mut app, _req_rx, _cancel_rx) = new_app();
        assert!(!app.run.is_active());
        app.status = Some(Status::error("keep me"));
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
        assert!(
            matches!(&app.status, Some(s) if s.text == "keep me"),
            "CancelOrQuit with no active run must not blanket-clear the pre-existing status \
             before quitting"
        );
    }

    #[test]
    fn run_key_sets_an_already_running_status_and_it_survives_the_same_dispatch() {
        let (mut app, _req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT pg_sleep(1)");
        app.start_run(RunTarget::Buffer);
        assert!(app.run.is_active(), "test setup: a run must be in flight");

        app.on_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let status = app
            .status
            .as_ref()
            .expect("start_run's already-running branch must set a status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("already running"));
    }

    #[test]
    fn starting_a_multi_statement_run_shows_progress_in_the_status_bar() {
        let (mut app, _req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 3;");
        app.start_run(RunTarget::Buffer);
        let status = app
            .status
            .as_ref()
            .expect("starting a run must set a progress status");
        assert_eq!(status.text, "running statement 1 of 3…");
    }

    #[test]
    fn a_single_statement_run_shows_a_plain_running_message() {
        let (mut app, _req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        app.start_run(RunTarget::Buffer);
        let status = app
            .status
            .as_ref()
            .expect("starting a run must set a status");
        assert_eq!(status.text, "running…");
    }

    #[test]
    fn a_keypress_during_an_active_run_does_not_wipe_the_progress_message() {
        let (mut app, _req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 3;");
        app.start_run(RunTarget::Buffer);
        assert!(app.run.is_active(), "test setup: a run must be in flight");

        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        let status = app
            .status
            .as_ref()
            .expect("the progress message must survive an unrelated keypress");
        assert!(status.text.contains("running"));
    }

    #[test]
    fn the_final_summary_replaces_the_progress_message() {
        let (mut app, mut req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        app.start_run(RunTarget::Buffer);
        let req = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => {
                panic!("expected a Query request")
            }
        };
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("running")),
            "test setup: a progress status must precede the summary"
        );

        app.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 3 }),
        }));

        let status = app
            .status
            .as_ref()
            .expect("a finished run must set a summary status");
        assert!(
            !status.text.contains("running"),
            "the summary must replace the progress message, got {:?}",
            status.text
        );
    }

    #[test]
    fn jump_to_error_position_is_a_noop_for_error_variants_without_a_position() {
        // `DataSourceError::error_position()` only ever returns `Some` for
        // `DataSourceError::Query` wrapping a real `tokio_postgres::Error`
        // with a DB-reported position -- and `tokio_postgres::Error`/`DbError`
        // expose no public constructor for that outside the crate itself
        // (verified: `Error::db`/`DbError::parse` are both `pub(crate)` in
        // tokio-postgres 0.7.18), so `jump_to_error_position` itself (the
        // `err.error_position()` branch and the call into `error_offset`)
        // cannot be exercised end-to-end from a pure unit test without a live
        // Postgres connection. That part is only covered end-to-end today by
        // manual verification (per the Phase 7 status note in
        // docs/MVP0-PLAN.md) and indirectly by
        // `syntax_error_surfaces_from_first_next_call_with_an_error_position`
        // in tests/postgres.rs, which confirms a real syntax error DOES carry
        // a position (necessary precondition) but doesn't drive `App`.
        //
        // The guard logic that runs once a position IS known -- the
        // concurrent-edit check and the char->byte offset math -- is,
        // however, fully unit-tested in isolation via the pure `error_offset`
        // function below (see the `error_offset_*` tests).
        //
        // What IS unit-testable here without a DB: every other
        // `DataSourceError` variant must leave the cursor untouched, since
        // `jump_to_error_position` returns immediately when
        // `error_position()` is `None`.
        let (mut app, _req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1");
        let before = app.editor.buffer().cursor();

        let unit = crate::editor::RunUnit {
            sql: "SELECT 1".to_string(),
            span: crate::editor::ByteSpan { start: 0, end: 8 },
            start: crate::editor::Position::default(),
        };
        for err in [
            DataSourceError::Cancelled,
            DataSourceError::Busy {
                name: "test".to_string(),
            },
            DataSourceError::MultipleStatements,
        ] {
            assert_eq!(err.error_position(), None, "test setup precondition");
            app.jump_to_error_position(&unit, &err);
            assert_eq!(
                app.editor.buffer().cursor(),
                before,
                "a DataSourceError with no error_position() must never move the cursor"
            );
        }
    }

    fn summary(ran: usize, total: usize, cancelled: Option<CancelOutcome>) -> RunSummary {
        RunSummary {
            ran,
            total,
            last_affected: None,
            failed: None,
            cancelled,
        }
    }

    #[test]
    fn summary_status_completed_first_not_the_last_statement_names_the_statement_it_stopped_before()
    {
        let s = summary(1, 3, Some(CancelOutcome::CompletedFirst));
        let status = summary_status(&s);
        assert_eq!(status.kind, StatusKind::Warn);
        assert_eq!(
            status.text,
            "cancel came too late — statement 1 of 3 completed; stopped before statement 2"
        );
    }

    #[test]
    fn summary_status_completed_first_on_the_last_statement_has_no_stopped_before_clause() {
        let s = summary(3, 3, Some(CancelOutcome::CompletedFirst));
        let status = summary_status(&s);
        assert_eq!(
            status.text,
            "cancel came too late — statement 3 of 3 completed"
        );
        assert!(!status.text.contains("stopped before"));
    }

    #[test]
    fn summary_status_distinguishes_an_interrupted_cancel_from_one_that_completed_first() {
        // Phase 8's whole point for `CancelOutcome`: a cancel that actually
        // aborted the statement must read differently from one that arrived
        // after the statement already committed, so a user doesn't assume a
        // completed DELETE was rolled back.
        let interrupted = summary(2, 3, Some(CancelOutcome::Interrupted));
        let completed_first = summary(2, 3, Some(CancelOutcome::CompletedFirst));
        let interrupted_text = summary_status(&interrupted).text;
        let completed_first_text = summary_status(&completed_first).text;
        assert_ne!(interrupted_text, completed_first_text);
        assert!(interrupted_text.contains("interrupted"));
        assert!(completed_first_text.contains("completed"));
    }

    #[test]
    fn summary_status_with_no_cancel_and_no_failure_reports_a_plain_completion() {
        let s = summary(2, 2, None);
        let status = summary_status(&s);
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "ran 2 of 2");
    }

    #[test]
    fn stale_cancel_failed_from_a_finished_run_is_ignored_and_does_not_clobber_a_new_runs_status() {
        let (mut app, mut req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        app.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };

        // The run finishes (successfully) -- `req1.id` is no longer owned by
        // `self.run` once this returns.
        app.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));
        assert!(!app.run.is_active(), "test setup: the run must have ended");
        let summary_text = app
            .status
            .as_ref()
            .expect("the finished run must have set a summary status")
            .text
            .clone();

        // A `CancelFailed` for that same (now stale) request id arrives late,
        // e.g. from a cancel that was in flight against the transport when
        // the run ended.
        app.apply(WorkerResponse::Query(run::QueryResponse::CancelFailed {
            id: req1.id,
            message: "stale transport failure".to_string(),
        }));
        assert_eq!(
            app.status.as_ref().map(|s| s.text.clone()),
            Some(summary_text),
            "a CancelFailed for a request id the run no longer owns must not overwrite the status"
        );

        // Start a brand-new run (mints a fresh RequestId) and confirm the
        // stale CancelFailed doesn't resurface / clobber it either.
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 2;");
        app.start_run(RunTarget::Buffer);
        let running_text = app
            .status
            .as_ref()
            .expect("starting the new run must set a running status")
            .text
            .clone();
        assert!(running_text.contains("running"));

        app.apply(WorkerResponse::Query(run::QueryResponse::CancelFailed {
            id: req1.id,
            message: "stale transport failure".to_string(),
        }));
        assert_eq!(
            app.status.as_ref().map(|s| s.text.clone()),
            Some(running_text),
            "the stale CancelFailed must not clobber the new run's status, since `run.owns(id)` \
             is false for the old id once a new run has started"
        );
    }

    #[test]
    fn intermediate_statement_finishing_does_not_append_a_misleading_grid_moved_on_note() {
        // If a table is opened mid-run (origin diverges from `Query`) and an
        // *intermediate* statement's result then arrives, the `Next` branch
        // of `apply_query_response` unconditionally calls `begin_query` for
        // the next statement -- which immediately reclaims the grid from
        // whatever table view the user opened. A "result not shown (grid
        // moved on)" note in that branch would therefore be backwards: the
        // grid didn't stay moved onto the table, it just got yanked back
        // onto the run. So this branch must never append that note; it's
        // only accurate in the `Done` branch, where nothing subsequent
        // re-claims the grid (see the sibling test below for that case).
        let (mut app, mut req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2;");
        app.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };
        assert!(
            app.grid.is_open(),
            "test setup: begin_query must open the grid"
        );

        // Simulate the user opening a table-browse view mid-run -- the same
        // `DataGridState::open` call `App::activate` makes -- which switches
        // the grid's origin away from `Query`.
        let _ = app.grid.open("public".to_string(), "t".to_string());

        app.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));

        let status = app
            .status
            .as_ref()
            .expect("advancing to the next statement must set a status");
        assert!(
            !status.text.contains("grid moved on"),
            "an intermediate statement's discarded result must not claim the grid \"moved on\" \
             when this very branch is about to reclaim the grid for the next statement, got {:?}",
            status.text
        );
        assert!(
            matches!(
                app.grid.origin(),
                Some(crate::ui::grid::state::GridOrigin::Query { .. })
            ),
            "test setup sanity: begin_query for statement 2 must have reclaimed the grid"
        );
    }

    #[test]
    fn grid_moved_on_note_is_appended_when_the_final_statements_result_is_discarded() {
        let (mut app, mut req_rx, _cancel_rx) = new_app();
        *app.editor.buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        app.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };
        assert!(
            app.grid.is_open(),
            "test setup: begin_query must open the grid"
        );

        let _ = app.grid.open("public".to_string(), "t".to_string());

        app.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));

        let status = app
            .status
            .as_ref()
            .expect("the final statement finishing must set a summary status");
        assert!(
            status.text.contains("· result not shown (grid moved on)"),
            "the final statement's discarded result must be noted in the status, got {:?}",
            status.text
        );
    }

    fn unit_at(sql: &str, start: usize) -> crate::editor::RunUnit {
        crate::editor::RunUnit {
            sql: sql.to_string(),
            span: crate::editor::ByteSpan {
                start,
                end: start + sql.len(),
            },
            start: crate::editor::Position::default(),
        }
    }

    #[test]
    fn error_offset_returns_none_when_the_span_no_longer_fits_the_edited_buffer() {
        let unit = unit_at("SELECT 1", 0);
        // The buffer shrank (e.g. a huge chunk got deleted) so the original
        // span's end is now past the end of the text entirely -- must not
        // panic, must return None.
        let text = "SE";
        assert_eq!(error_offset(text, &unit, 1), None);
    }

    #[test]
    fn error_offset_returns_none_when_the_span_ends_mid_utf8_char() {
        // The current buffer has a multi-byte char straddling byte offset 1
        // -- not a valid boundary -- simulating an edit that left the
        // captured span landing mid-character in the NEW text. `sql`'s
        // content is irrelevant here: the boundary check must reject this
        // before any content comparison happens.
        let text = "🦀";
        let unit = crate::editor::RunUnit {
            sql: "x".to_string(),
            span: crate::editor::ByteSpan { start: 0, end: 1 },
            start: crate::editor::Position::default(),
        };
        assert_eq!(error_offset(text, &unit, 1), None);
    }

    #[test]
    fn error_offset_returns_none_when_the_text_at_the_span_changed() {
        let unit = unit_at("SELECT 1", 0);
        // Same length, different content: the buffer was edited in place
        // while the query was in flight.
        let text = "SELECT 2";
        assert_eq!(error_offset(text, &unit, 1), None);
    }

    #[test]
    fn error_offset_returns_none_for_a_position_past_the_statement() {
        let unit = unit_at("SELECT 1", 0);
        let text = unit.sql.clone();
        assert_eq!(
            error_offset(
                &text,
                &unit,
                u32::try_from(unit.sql.chars().count() + 1).unwrap()
            ),
            None
        );
        assert_eq!(error_offset(&text, &unit, 0), None);
    }

    #[test]
    fn error_offset_maps_a_1_based_char_position_to_a_byte_offset_past_multibyte_text() {
        // "SELECT '🦀', 1" -- the crab is a 4-byte UTF-8 char but counts as
        // ONE character for the server's 1-based position, so the byte
        // offset of anything after it must NOT be off by the extra 3 bytes.
        let sql = "SELECT '🦀', 1";
        let base = 10;
        let unit = unit_at(sql, base);
        let mut text = " ".repeat(base);
        text.push_str(sql);

        // Position 13 (1-based, char-counted) is the '1' right after "🦀', ".
        let char_idx = sql.chars().position(|c| c == '1').unwrap();
        let pos = u32::try_from(char_idx + 1).unwrap();
        let offset = error_offset(&text, &unit, pos).expect("position is within the statement");

        let expected_byte = base + sql.char_indices().nth(char_idx).unwrap().0;
        assert_eq!(offset, expected_byte);
        assert_eq!(&text[offset..offset + 1], "1");
    }
}
