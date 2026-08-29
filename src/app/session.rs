use std::sync::Arc;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Tabs, Wrap};
#[cfg(test)]
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::datasource::{ConnectOptions, DataSource, DataSourceError, PostgresDataSource};
use crate::editor::{Motion, RunTarget};
use crate::pages::{PageTabs, PagesError};
use crate::ui::editor::EditorState;
use crate::ui::editor::widget::EditorWidget;
use crate::ui::grid::state::DataGridState;
use crate::ui::grid::widget::DataGridWidget;
use crate::ui::tree::model::NodeKey;
use crate::ui::tree::state::{ObjectTreeState, TreeCommand, TreeRowKey};
use crate::ui::tree::widget::ObjectTreeWidget;

use super::Focus;
use super::keymap::{AppCommand, RunKey, map_key};
use super::message::{SessionResponse, WorkerRequest, WorkerResponse};
use super::run::{self, CancelOutcome, CancelRequest, RunOutcome, RunState, RunSummary};
use super::status::Status;
use super::worker;

const TREE_FOOTER: &str =
    "↑/↓ move  →/← expand/collapse  ⏎ open/toggle  r refresh  . system  Tab grid  q quit";
const GRID_FOOTER: &str = "↑/↓ move  ←/→ scroll cols  PgUp/PgDn page  n/p next/prev page  r refresh  Esc/Tab tree  q quit";
const EDITOR_FOOTER: &str = "type to edit  Ctrl+R run cursor/selection  Ctrl+E run buffer  \
                              Ctrl+A select all  Esc clear selection  Ctrl+C cancel/quit  Tab tree/grid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Wires an already-connected `PostgresDataSource` into a worker + canceller
/// task pair. Deliberately split from the connect step (`spawn_open` below)
/// so a later activity-monitor handle -- which would dial the session's
/// existing tunnel rather than opening a new one -- can reuse exactly this
/// half without a second `ssh -L`.
pub struct SourceHandle {
    // Concrete, not `Arc<dyn DataSource>`: `close()`/`tunnel_local_port()`/
    // `tunnel_forward_confirmed()` are inherent methods on
    // `PostgresDataSource`, not part of the `DataSource` trait.
    source: Arc<PostgresDataSource>,
    requests: UnboundedSender<WorkerRequest>,
    cancels: UnboundedSender<CancelRequest>,
    worker: JoinHandle<()>,
    canceller: JoinHandle<()>,
}

impl SourceHandle {
    pub fn attach(
        source: PostgresDataSource,
        session: SessionId,
        responses: UnboundedSender<SessionResponse>,
    ) -> SourceHandle {
        let source = Arc::new(source);
        let worker_source: Arc<dyn DataSource> = source.clone();
        let canceller_source: Arc<dyn DataSource> = source.clone();

        let (requests_tx, requests_rx) = unbounded_channel();
        let (cancels_tx, cancels_rx) = unbounded_channel();

        let worker = worker::spawn(session, worker_source, requests_rx, responses.clone());
        let canceller = worker::spawn_canceller(session, canceller_source, cancels_rx, responses);

        SourceHandle {
            source,
            requests: requests_tx,
            cancels: cancels_tx,
            worker,
            canceller,
        }
    }

    pub fn send(&self, req: WorkerRequest) {
        let _ = self.requests.send(req);
    }

    pub fn cancel(&self, req: CancelRequest) {
        let _ = self.cancels.send(req);
    }

    pub fn source(&self) -> &Arc<PostgresDataSource> {
        &self.source
    }

    pub fn tunnel_local_port(&self) -> Option<u16> {
        self.source.tunnel_local_port()
    }

    pub fn tunnel_forward_confirmed(&self) -> Option<bool> {
        self.source.tunnel_forward_confirmed()
    }

    /// Verbatim port of the teardown block that used to live at the end of
    /// `main.rs::run` (drop the senders, abort+await both tasks so their
    /// `Arc<dyn DataSource>` clones drop, then `Arc::into_inner` + `close()`).
    pub async fn shutdown(self) {
        // The worker only notices the channel closing *after* its current
        // `handle(...).await` call returns, and nothing times out a request
        // against an unresponsive connection -- so a plain `.await` here can
        // hang forever. Abort instead: for a session-close path, not waiting
        // for an in-flight DataSource call to finish is the right tradeoff,
        // since nothing consumes its response after close anyway.
        // `worker.await` resolves promptly once the task notices the abort
        // at its next await point, which drops its `Arc<dyn DataSource>`
        // clone -- a precondition for `Arc::into_inner` below to succeed.
        // The canceller task holds its own separate clone and must be
        // aborted/awaited the same way before that precondition holds.
        self.worker.abort();
        let _ = self.worker.await;
        self.canceller.abort();
        let _ = self.canceller.await;

        // `None` only if something above still holds a clone; the tunnel's
        // own Drop impl reaps the ssh child regardless, so that case is safe
        // to skip.
        if let Some(pg) = Arc::into_inner(self.source) {
            pg.close().await;
        }
    }
}

pub enum SessionState {
    Connecting {
        message: String,
    },
    Ready(SourceHandle),
    Failed {
        message: String,
    },
    /// Test-only stand-in for `Ready` that skips `SourceHandle::attach`
    /// entirely: `SourceHandle`'s `source` is a concrete
    /// `Arc<PostgresDataSource>`, which can only be constructed via a real,
    /// live Postgres connection -- there is no fake/mock `DataSource` to
    /// substitute in a unit test. This variant preserves the pre-Phase-2
    /// test pattern (capture what a session tries to send on a plain
    /// channel, with no worker task actually consuming it) without requiring
    /// a live database in `cargo test`.
    #[cfg(test)]
    TestReady {
        requests: UnboundedSender<WorkerRequest>,
        cancels: UnboundedSender<CancelRequest>,
    },
}

pub enum SessionAction {
    Quit,
}

pub struct Session {
    pub id: SessionId,
    pub connection_name: String,
    pub group: Option<String>,
    tree: ObjectTreeState,
    grid: DataGridState,
    pages: PageTabs,
    run: RunState,
    // Which page (by index into `pages.tabs()`) the currently in-flight run
    // was started from. Guards `jump_to_error_position`: if the user has
    // switched pages by the time an error response arrives, the page whose
    // buffer the error's byte offset refers to is no longer the one on
    // screen, so jumping the cursor there would silently edit the wrong
    // page. Cleared whenever a run finishes or a page closes (page indices
    // shift on close, so a stale index could otherwise alias a different
    // page entirely).
    run_page: Option<usize>,
    status: Option<Status>,
    focus: Focus,
    state: SessionState,
}

impl Session {
    pub fn new(id: SessionId, connection_name: String, group: Option<String>) -> Session {
        let pages = PageTabs::restore(&connection_name);
        Session::new_with_pages(id, connection_name, group, pages)
    }

    /// Test seam: `PageTabs::restore` can only be bypassed by supplying an
    /// already-constructed `PageTabs` (e.g. `PageTabs::detached()`), since
    /// the real constructor resolves and touches the platform data
    /// directory.
    pub fn new_with_pages(
        id: SessionId,
        connection_name: String,
        group: Option<String>,
        pages: PageTabs,
    ) -> Session {
        Session {
            id,
            connection_name,
            group,
            tree: ObjectTreeState::new(),
            grid: DataGridState::new(),
            pages,
            run: RunState::new(),
            run_page: None,
            status: None,
            focus: Focus::Tree,
            state: SessionState::Connecting {
                message: "connecting…".to_string(),
            },
        }
    }

    pub fn on_connected(&mut self, handle: SourceHandle) {
        self.state = SessionState::Ready(handle);
        let req = self.tree.refresh_root();
        self.send(WorkerRequest::Tree(req));
    }

    pub fn on_failed(&mut self, message: String) {
        self.state = SessionState::Failed { message };
    }

    pub fn set_connecting_message(&mut self, message: String) {
        if let SessionState::Connecting { message: m } = &mut self.state {
            *m = message;
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub fn footer_text(&self) -> &'static str {
        match self.focus {
            Focus::Tree => TREE_FOOTER,
            Focus::Grid => GRID_FOOTER,
            Focus::Editor => EDITOR_FOOTER,
        }
    }

    pub fn pages(&self) -> &PageTabs {
        &self.pages
    }

    pub fn pages_mut(&mut self) -> &mut PageTabs {
        &mut self.pages
    }

    pub fn is_dirty(&self) -> bool {
        self.pages.any_dirty()
    }

    pub fn dirty_titles(&self) -> Vec<String> {
        self.pages.dirty_titles()
    }

    /// The active page's buffer, delegated through `pages` -- kept private
    /// so every other call site in this module reads exactly like it did
    /// before pages existed, with `self.editor()`/`self.editor_mut()` in
    /// place of the old `self.editor` field access.
    fn editor(&self) -> &EditorState {
        self.pages.editor()
    }

    fn editor_mut(&mut self) -> &mut EditorState {
        self.pages.editor_mut()
    }

    /// Closes the active page, refusing (returning `Ok(false)`) if it's
    /// dirty and `force` is `false` -- the caller must re-invoke with
    /// `force: true` after confirming with the user. Always clears
    /// `run_page` on an actual close, since page indices shift.
    pub fn close_page(&mut self, force: bool) -> Result<bool, PagesError> {
        let closed = self.pages.close_active(force)?;
        if closed {
            self.run_page = None;
        }
        Ok(closed)
    }

    /// Deletes `name` (from disk, and its tab if it's open) via
    /// `PageTabs::delete`, then clears `run_page` the same way `close_page`
    /// does -- `PageTabs::delete` closes the deleted page's tab internally
    /// (`PageTabs::close_at`, not `close_active`), which is exactly the kind
    /// of index-shifting close `run_page`'s doc comment says must clear it.
    /// Deliberately unconditional (not gated on whether `name` happened to be
    /// open) since it's always safe and this is not a hot path.
    pub fn delete_page(&mut self, name: &crate::pages::PageName) -> Result<(), PagesError> {
        self.pages.delete(name)?;
        self.run_page = None;
        Ok(())
    }

    /// Lets `App` report a page-operation outcome (save/open/rename/close
    /// error or success) on this session's own status line, the same one
    /// `on_key`/`start_run` already use -- without exposing the `status`
    /// field itself.
    pub fn set_error_status(&mut self, message: String) {
        self.status = Some(Status::error(message));
    }

    pub fn set_info_status(&mut self, message: String) {
        self.status = Some(Status::info(message));
    }

    /// Consumes `self` to hand back its `SessionState` for teardown --
    /// `App::close_active`/`App::shutdown` need to move the `SourceHandle`
    /// out of a `Ready` session without cloning it.
    pub(crate) fn into_state(self) -> SessionState {
        self.state
    }

    // A single body (not two `#[cfg(not(test))]`/`#[cfg(test)]` copies of the
    // whole function) so unit tests exercise the exact same `Ready` arm that
    // ships. Only the extra `TestReady` arm is gated -- and every other
    // `SessionState` variant is matched explicitly on both sides rather than
    // via a wildcard, so a future new variant fails to compile here instead
    // of being silently swallowed on just the test path.
    fn send(&self, req: WorkerRequest) {
        match &self.state {
            SessionState::Ready(handle) => handle.send(req),
            SessionState::Connecting { .. } | SessionState::Failed { .. } => {}
            #[cfg(test)]
            SessionState::TestReady { requests, .. } => {
                let _ = requests.send(req);
            }
        }
    }

    /// `true` for `Ready` (and the test-only `TestReady` seam) -- the same
    /// states `send` actually forwards a `WorkerRequest` for, rather than
    /// silently dropping it. See `start_run`'s guard, which must not begin a
    /// run in any state where `send` would swallow the resulting request.
    fn is_ready(&self) -> bool {
        match &self.state {
            SessionState::Ready(_) => true,
            SessionState::Connecting { .. } | SessionState::Failed { .. } => false,
            #[cfg(test)]
            SessionState::TestReady { .. } => true,
        }
    }

    fn send_cancel(&self, req: CancelRequest) {
        match &self.state {
            SessionState::Ready(handle) => handle.cancel(req),
            SessionState::Connecting { .. } | SessionState::Failed { .. } => {}
            #[cfg(test)]
            SessionState::TestReady { cancels, .. } => {
                let _ = cancels.send(req);
            }
        }
    }

    pub fn apply(&mut self, response: WorkerResponse) {
        match response {
            WorkerResponse::Tree(r) => self.tree.apply(r),
            WorkerResponse::Grid(r) => self.grid.apply(r),
            WorkerResponse::Query(r) => self.apply_query_response(r),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<SessionAction> {
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
            Some(AppCommand::Quit) => return Some(SessionAction::Quit),
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
                    self.send(WorkerRequest::Tree(req));
                }
            }
            Some(AppCommand::Grid(cmd)) => {
                if let Some(req) = self.grid.command(cmd) {
                    self.send(WorkerRequest::Grid(req));
                }
            }
            Some(AppCommand::Editor(cmd)) => {
                self.editor_mut().command(cmd);
            }
            Some(AppCommand::Run(key)) => {
                let target = match key {
                    RunKey::CursorOrSelection => {
                        if self.editor().buffer().selection().is_some() {
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
                    self.send_cancel(req);
                    self.status = Some(Status::info("cancelling…"));
                }
                None if self.run.is_active() => {
                    self.status = Some(Status::info("cancelling…"));
                }
                None => return Some(SessionAction::Quit),
            },
            // Tab/page-tab commands are intercepted by `App::on_key` before a
            // session ever sees them; kept here only so this match stays
            // exhaustive.
            Some(AppCommand::OpenPicker)
            | Some(AppCommand::CloseTab)
            | Some(AppCommand::NextTab)
            | Some(AppCommand::PrevTab)
            | Some(AppCommand::OpenPageList)
            | Some(AppCommand::SavePage)
            | Some(AppCommand::RenamePage)
            | Some(AppCommand::NewPage)
            | Some(AppCommand::ClosePage)
            | Some(AppCommand::NextPage)
            | Some(AppCommand::PrevPage)
            | Some(AppCommand::ReloadPage) => {}
            None => {}
        }
        None
    }

    pub fn start_run(&mut self, target: RunTarget) {
        if !self.is_ready() {
            // Matches `send`'s existing silent-no-op convention for a
            // non-Ready session: a run-triggering key reachable while a tab
            // is still `Connecting`/`Failed` (Tab/character-insert keys work
            // in every state per `map_key`, even though the editor isn't
            // rendered outside `Ready`) must never call `self.run.start(..)`,
            // since `send` would then silently drop the resulting
            // `WorkerRequest` -- leaving `RunState::is_active()` stuck `true`
            // forever with no `Finished` ever able to arrive to clear it.
            return;
        }
        if self.run.is_active() {
            self.status = Some(Status::info("a query is already running"));
            return;
        }
        match crate::editor::plan_run(self.editor().buffer(), target) {
            Err(split_err) => {
                self.status = Some(Status::error(split_err.message.clone()));
                let pos = self.editor().buffer().position_of(split_err.span.start);
                self.editor_mut().buffer_mut().move_to(pos, Motion::Move);
            }
            Ok(units) if units.is_empty() => {
                self.status = Some(Status::info("nothing to run"));
            }
            Ok(units) => {
                if let Some(req) = self.run.start(units) {
                    self.run_page = Some(self.pages.active_index());
                    self.grid.begin_query(req.id, title_of(&req.sql));
                    self.send(WorkerRequest::Query(req));
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
                    self.send_cancel(req);
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
                        self.send(WorkerRequest::Query(req));
                        self.status = Some(running_status(&self.run));
                    }
                    Some(RunOutcome::Done(summary)) => {
                        self.run_page = None;
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

    fn jump_to_error_position(&mut self, unit: &crate::editor::RunUnit, err: &DataSourceError) {
        // The run this error belongs to was started from a different page
        // than the one currently active (the user switched pages while it
        // was in flight) -- the byte offset in `unit`/`err` refers to that
        // other page's buffer, not this one, so jumping here would silently
        // edit the wrong page.
        if self.run_page != Some(self.pages.active_index()) {
            return;
        }
        let Some(pos) = err.error_position() else {
            return;
        };
        let text = self.editor().buffer().text();
        let Some(offset) = error_offset(&text, unit, pos) else {
            return;
        };
        let buffer_pos = self.editor().buffer().position_of(offset);
        self.editor_mut()
            .buffer_mut()
            .move_to(buffer_pos, Motion::Move);
    }

    fn activate(&mut self) {
        if let Some(row) = self.tree.selected()
            && let TreeRowKey::Node(NodeKey::Table { schema, table }) = &row.key
        {
            let (schema, table) = (schema.clone(), table.clone());
            let req = self.grid.open(schema, table);
            self.focus = Focus::Grid;
            self.send(WorkerRequest::Grid(req));
            return;
        }
        if let Some(req) = self.tree.command(TreeCommand::Toggle) {
            self.send(WorkerRequest::Tree(req));
        }
    }

    /// T2's user-facing warning (docs/MVP1-PHASE2-DESIGN.md §2 T2 item 5):
    /// `Some(_)` only for a `Ready` session whose tunnel's `ssh` never
    /// confirmed it owns the forwarded port (see `Tunnel::forward_confirmed`'s
    /// doc comment for what that means). Shared by `tab_title`'s sticky `⚠`
    /// and `App::render_footer`'s guaranteed-visible warning line -- the tab
    /// title marker alone isn't enough, since `Tabs` clips rather than
    /// scrolling a long tab strip.
    pub fn tunnel_warning(&self) -> Option<String> {
        let SessionState::Ready(handle) = &self.state else {
            return None;
        };
        if handle.tunnel_forward_confirmed() != Some(false) {
            return None;
        }
        let port = handle.tunnel_local_port().unwrap_or(0);
        Some(format!(
            "tunnel readiness unconfirmed — could not verify this ssh owns port {port}; it may \
             belong to another process"
        ))
    }

    pub fn tab_title(&self) -> Line<'_> {
        let warn = self.tunnel_warning().is_some();
        match &self.state {
            SessionState::Ready(_) => {
                let text = if warn {
                    format!(" ⚠ {} ", self.connection_name)
                } else {
                    format!(" {} ", self.connection_name)
                };
                Line::from(text)
            }
            SessionState::Connecting { .. } => Line::styled(
                format!(" … {} ", self.connection_name),
                Style::default().add_modifier(Modifier::DIM),
            ),
            SessionState::Failed { .. } => Line::styled(
                format!(" ! {} ", self.connection_name),
                Style::default().fg(Color::Red),
            ),
            #[cfg(test)]
            SessionState::TestReady { .. } => Line::from(format!(" {} ", self.connection_name)),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let SessionState::Connecting { message } = &self.state {
            render_connecting(frame, area, message);
            return;
        }
        if let SessionState::Failed { message } = &self.state {
            render_failed(frame, area, message);
            return;
        }
        self.render_panes(frame, area);
    }

    fn render_panes(&mut self, frame: &mut Frame, area: Rect) {
        let panes = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);
        let tree_area = panes[0];
        let right_area = panes[1];

        let right_rows =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(right_area);
        let page_tabs_area = right_rows[0];
        let editor_and_grid_area = right_rows[1];

        let (editor_area, grid_area) = if self.grid.is_open() {
            let rows = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(editor_and_grid_area);
            (rows[0], Some(rows[1]))
        } else {
            (editor_and_grid_area, None)
        };

        let tree_style = pane_border_style(self.focus == Focus::Tree);
        let tree_block = Block::bordered()
            .border_style(tree_style)
            .title(format!(" ratwarren — {} ", self.connection_name));
        let tree_widget = ObjectTreeWidget::new()
            .block(tree_block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(tree_widget, tree_area, &mut self.tree);

        let page_titles: Vec<Line> = self
            .pages
            .tabs()
            .iter()
            .map(|page| {
                let text = if page.is_dirty() {
                    format!(" {}* ", page.title())
                } else {
                    format!(" {} ", page.title())
                };
                Line::from(text)
            })
            .collect();
        let page_tabs = Tabs::new(page_titles)
            .select(Some(self.pages.active_index()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(page_tabs, page_tabs_area);

        let editor_style = pane_border_style(self.focus == Focus::Editor);
        let editor_block = Block::bordered()
            .border_style(editor_style)
            .title(" editor ");
        let editor_inner = editor_block.inner(editor_area);
        let editor_widget = EditorWidget::new().block(editor_block);
        frame.render_stateful_widget(editor_widget, editor_area, self.pages.editor_mut());
        if self.focus == Focus::Editor
            && let Some(pos) = self.editor().cursor_screen_pos(editor_inner)
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
    }
}

// Test-only seam, `pub(crate)` rather than confined to this module's own
// `#[cfg(test)] mod tests` below: `app::mod`'s own tests (routing/tab-lifecycle
// safety properties that are genuinely `App`-level, not `Session`-level) need
// to build multiple `Ready`-like sessions and inspect their private
// `run`/`grid`/`editor` state, but `SourceHandle::attach` can only be
// constructed against a real, live Postgres connection (see
// `SessionState::TestReady`'s doc comment) and `Session`'s fields are private
// to this module by design. This is the narrowest surface that unblocks that:
// one constructor plus small read/write accessors, all compiled only under
// `#[cfg(test)]` and visible only within the crate.
#[cfg(test)]
pub(crate) fn test_ready_session(
    id: SessionId,
    connection_name: String,
    group: Option<String>,
) -> (
    Session,
    UnboundedReceiver<WorkerRequest>,
    UnboundedReceiver<CancelRequest>,
) {
    let (req_tx, req_rx) = unbounded_channel();
    let (cancel_tx, cancel_rx) = unbounded_channel();
    // `new_with_pages` + `PageTabs::detached()`, not `Session::new`: the real
    // constructor calls `PageTabs::restore`, which touches the platform data
    // directory -- undesirable in a unit test.
    let mut session = Session::new_with_pages(
        id,
        connection_name,
        group,
        crate::pages::PageTabs::detached(),
    );
    session.state = SessionState::TestReady {
        requests: req_tx,
        cancels: cancel_tx,
    };
    (session, req_rx, cancel_rx)
}

#[cfg(test)]
impl Session {
    pub(crate) fn set_editor_text_for_test(&mut self, text: &str) {
        *self.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text(text);
    }

    pub(crate) fn editor_text_for_test(&self) -> String {
        self.editor().buffer().text()
    }

    pub(crate) fn run_is_active_for_test(&self) -> bool {
        self.run.is_active()
    }

    pub(crate) fn grid_content_for_test(&self) -> &crate::ui::Load<crate::ui::grid::GridContent> {
        self.grid.content()
    }
}

fn render_connecting(frame: &mut Frame, area: Rect, message: &str) {
    let block = Block::bordered().title(" connecting… ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let paragraph = Paragraph::new(message)
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, inner);
}

fn render_failed(frame: &mut Frame, area: Rect, message: &str) {
    let block = Block::bordered().title(" connection failed ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = format!("{message}\n\nCtrl+W to close");
    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::Red))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, inner);
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

/// Opens a session's connection off the event loop: reads the OS keyring,
/// dials Postgres (including its SSH tunnel, if any), and reports back via
/// `events`/`responses` -- never blocks the caller. Every session, including
/// the first one at startup, goes through this same path (see S3 in
/// docs/MVP1-PHASE2-DESIGN.md): there is no separate pre-`ratatui::init()`
/// special case.
pub fn spawn_open(
    conn: crate::config::Connection,
    session: SessionId,
    responses: UnboundedSender<SessionResponse>,
    events: UnboundedSender<OpenEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if crate::secret::will_look_up_keyring(&conn) {
            let _ = events.send(OpenEvent::Progress {
                session,
                message: "reading the OS keyring…".to_string(),
            });
        }

        let notes_events = events.clone();
        let notes = move |message: String| {
            let _ = notes_events.send(OpenEvent::Progress { session, message });
        };
        let secret = crate::secret::resolve_async(&conn, &notes).await;
        if let Some(note) = secret.note() {
            let _ = events.send(OpenEvent::Progress {
                session,
                message: note,
            });
        }

        // T1's `OPEN_LOCK` (docs/MVP1-PHASE2-DESIGN.md §2) serializes tunnel
        // opens process-wide, so a second tab's tunnel can queue silently
        // behind a first tab's stuck open for up to `ready_timeout`. Without
        // this probe the queued tab shows the same "connecting to X…" message
        // as a tab that's slow on its own merits, and the user can't tell
        // "my tab is queued" from "my tab itself is stuck". A `try_lock()`
        // here is just a probe -- the real (blocking) acquire still happens
        // inside `connect_with` right below regardless of what this observes.
        let message = if conn.tunnel.is_some() && crate::tunnel::OPEN_LOCK.try_lock().is_err() {
            "waiting for another tab's SSH tunnel to finish opening…".to_string()
        } else {
            format!("connecting to {}…", conn.name)
        };
        let _ = events.send(OpenEvent::Progress { session, message });
        let connected =
            PostgresDataSource::connect_with(&conn, secret.password(), &ConnectOptions::default())
                .await;
        drop(secret);

        let result = match connected {
            Ok(source) => Ok(SourceHandle::attach(source, session, responses)),
            Err(e) => Err(crate::ui::error_chain(&e)),
        };
        let _ = events.send(OpenEvent::Done { session, result });
    })
}

pub enum OpenEvent {
    Progress {
        session: SessionId,
        message: String,
    },
    Done {
        session: SessionId,
        result: Result<SourceHandle, String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::status::StatusKind;
    use crate::datasource::DataSourceError;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    // These live inside `session`'s own module tree specifically so they can
    // reach `Session`'s private fields (status/editor/run) directly, the
    // same way `Session::on_key`/`start_run` themselves do -- no new
    // pub/pub(crate) accessor was added to support this.
    fn new_session() -> (
        Session,
        UnboundedReceiver<WorkerRequest>,
        UnboundedReceiver<CancelRequest>,
    ) {
        let (req_tx, req_rx) = unbounded_channel();
        let (cancel_tx, cancel_rx) = unbounded_channel();
        let mut session = Session::new_with_pages(
            SessionId(0),
            "test".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        session.state = SessionState::TestReady {
            requests: req_tx,
            cancels: cancel_tx,
        };
        (session, req_rx, cancel_rx)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn a_non_run_non_cancel_key_clears_a_stale_status() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        session.status = Some(Status::error("stale error from a previous run"));
        session.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            session.status.is_none(),
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
        let (mut session, _req_rx, _cancel_rx) = new_session();
        assert!(!session.run.is_active());
        session.status = Some(Status::error("keep me"));
        let action = session.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(SessionAction::Quit)));
        assert!(
            matches!(&session.status, Some(s) if s.text == "keep me"),
            "CancelOrQuit with no active run must not blanket-clear the pre-existing status \
             before quitting"
        );
    }

    #[test]
    fn run_key_sets_an_already_running_status_and_it_survives_the_same_dispatch() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT pg_sleep(1)");
        session.start_run(RunTarget::Buffer);
        assert!(
            session.run.is_active(),
            "test setup: a run must be in flight"
        );

        session.on_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let status = session
            .status
            .as_ref()
            .expect("start_run's already-running branch must set a status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("already running"));
    }

    #[test]
    fn starting_a_multi_statement_run_shows_progress_in_the_status_bar() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 3;");
        session.start_run(RunTarget::Buffer);
        let status = session
            .status
            .as_ref()
            .expect("starting a run must set a progress status");
        assert_eq!(status.text, "running statement 1 of 3…");
    }

    #[test]
    fn a_single_statement_run_shows_a_plain_running_message() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        let status = session
            .status
            .as_ref()
            .expect("starting a run must set a status");
        assert_eq!(status.text, "running…");
    }

    #[test]
    fn a_keypress_during_an_active_run_does_not_wipe_the_progress_message() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2; SELECT 3;");
        session.start_run(RunTarget::Buffer);
        assert!(
            session.run.is_active(),
            "test setup: a run must be in flight"
        );

        session.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        let status = session
            .status
            .as_ref()
            .expect("the progress message must survive an unrelated keypress");
        assert!(status.text.contains("running"));
    }

    #[test]
    fn the_final_summary_replaces_the_progress_message() {
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        let req = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => {
                panic!("expected a Query request")
            }
        };
        assert!(
            session
                .status
                .as_ref()
                .is_some_and(|s| s.text.contains("running")),
            "test setup: a progress status must precede the summary"
        );

        session.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 3 }),
        }));

        let status = session
            .status
            .as_ref()
            .expect("a finished run must set a summary status");
        assert!(
            !status.text.contains("running"),
            "the summary must replace the progress message, got {:?}",
            status.text
        );
    }

    // --- start_run's is_ready() guard (Phase 2 wedged-tab fix) ---

    #[test]
    fn start_run_is_a_noop_on_a_connecting_session() {
        // Reproduces the wedged-tab bug the `is_ready()` guard fixed:
        // `Tab`/character-insert keys route through `on_key` in every
        // session state (per `map_key`), even though the editor itself is
        // only rendered while `Ready` -- so `Ctrl+R` was reachable on a tab
        // still showing "connecting…". Before the guard, `start_run` called
        // `self.run.start(..)` unconditionally, setting
        // `RunState::is_active()` true; the follow-up `self.send(..)` then
        // silently dropped the request (via `send`'s own pre-existing
        // Connecting/Failed no-op arm, unrelated to this fix), so no
        // `Finished` response could ever arrive to clear it -- wedging the
        // tab's run state permanently. The load-bearing assertion is
        // `is_active()` staying `false`; a "no request was sent" check
        // would be vacuous here regardless of this fix, since `Connecting`
        // carries no channel at all to send on in the first place (`send`
        // already no-ops for it independently of `is_ready()`).
        let mut session = Session::new_with_pages(
            SessionId(0),
            "test".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        assert!(
            matches!(session.state, SessionState::Connecting { .. }),
            "test setup precondition"
        );
        session.set_editor_text_for_test("SELECT 1;");

        session.start_run(RunTarget::Buffer);

        assert!(
            !session.run.is_active(),
            "start_run on a Connecting session must be a silent no-op, not leave a run stuck \
             active forever"
        );
    }

    #[test]
    fn start_run_is_a_noop_on_a_failed_session() {
        let mut session = Session::new_with_pages(
            SessionId(0),
            "test".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        session.on_failed("connection refused".to_string());
        assert!(
            matches!(session.state, SessionState::Failed { .. }),
            "test setup precondition"
        );
        session.set_editor_text_for_test("SELECT 1;");

        session.start_run(RunTarget::Buffer);

        assert!(
            !session.run.is_active(),
            "start_run on a Failed session must be a silent no-op, not leave a run stuck active \
             forever"
        );
    }

    #[test]
    fn start_run_on_a_ready_session_still_starts_a_run() {
        // Sanity check that the `is_ready()` guard didn't over-restrict the
        // happy path -- `TestReady` is `Session`'s stand-in for `Ready` (see
        // its doc comment), the one other state `is_ready()` returns `true`
        // for.
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        session.set_editor_text_for_test("SELECT 1;");

        session.start_run(RunTarget::Buffer);

        assert!(
            session.run.is_active(),
            "start_run on a Ready session must still start a run"
        );
        match req_rx
            .try_recv()
            .expect("start_run on a Ready session must still send a WorkerRequest")
        {
            WorkerRequest::Query(_) => {}
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => {
                panic!("expected a Query request")
            }
        }
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
        // a position (necessary precondition) but doesn't drive `Session`.
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
        let (mut session, _req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1");
        let before = session.editor().buffer().cursor();

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
            session.jump_to_error_position(&unit, &err);
            assert_eq!(
                session.editor().buffer().cursor(),
                before,
                "a DataSourceError with no error_position() must never move the cursor"
            );
        }
    }

    #[test]
    fn tunnel_warning_is_none_while_connecting() {
        let session = Session::new_with_pages(
            SessionId(0),
            "test".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        assert!(matches!(session.state, SessionState::Connecting { .. }));
        assert_eq!(
            session.tunnel_warning(),
            None,
            "a Connecting session has no SourceHandle to consult, so it must never show the T2 \
             tunnel warning regardless of what a tunnel might eventually report"
        );
    }

    #[test]
    fn tunnel_warning_is_none_while_failed() {
        let mut session = Session::new_with_pages(
            SessionId(0),
            "test".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        session.on_failed("connection refused".to_string());
        assert!(matches!(session.state, SessionState::Failed { .. }));
        assert_eq!(
            session.tunnel_warning(),
            None,
            "a Failed session has no SourceHandle to consult, so it must never show the T2 \
             tunnel warning regardless of what a tunnel might eventually report"
        );
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
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };

        // The run finishes (successfully) -- `req1.id` is no longer owned by
        // `self.run` once this returns.
        session.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));
        assert!(
            !session.run.is_active(),
            "test setup: the run must have ended"
        );
        let summary_text = session
            .status
            .as_ref()
            .expect("the finished run must have set a summary status")
            .text
            .clone();

        // A `CancelFailed` for that same (now stale) request id arrives late,
        // e.g. from a cancel that was in flight against the transport when
        // the run ended.
        session.apply(WorkerResponse::Query(run::QueryResponse::CancelFailed {
            id: req1.id,
            message: "stale transport failure".to_string(),
        }));
        assert_eq!(
            session.status.as_ref().map(|s| s.text.clone()),
            Some(summary_text),
            "a CancelFailed for a request id the run no longer owns must not overwrite the status"
        );

        // Start a brand-new run (mints a fresh RequestId) and confirm the
        // stale CancelFailed doesn't resurface / clobber it either.
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 2;");
        session.start_run(RunTarget::Buffer);
        let running_text = session
            .status
            .as_ref()
            .expect("starting the new run must set a running status")
            .text
            .clone();
        assert!(running_text.contains("running"));

        session.apply(WorkerResponse::Query(run::QueryResponse::CancelFailed {
            id: req1.id,
            message: "stale transport failure".to_string(),
        }));
        assert_eq!(
            session.status.as_ref().map(|s| s.text.clone()),
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
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("SELECT 1; SELECT 2;");
        session.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };
        assert!(
            session.grid.is_open(),
            "test setup: begin_query must open the grid"
        );

        // Simulate the user opening a table-browse view mid-run -- the same
        // `DataGridState::open` call `Session::activate` makes -- which
        // switches the grid's origin away from `Query`.
        let _ = session.grid.open("public".to_string(), "t".to_string());

        session.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));

        let status = session
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
                session.grid.origin(),
                Some(crate::ui::grid::state::GridOrigin::Query { .. })
            ),
            "test setup sanity: begin_query for statement 2 must have reclaimed the grid"
        );
    }

    #[test]
    fn grid_moved_on_note_is_appended_when_the_final_statements_result_is_discarded() {
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        *session.editor_mut().buffer_mut() = crate::editor::TextBuffer::from_text("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        let req1 = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };
        assert!(
            session.grid.is_open(),
            "test setup: begin_query must open the grid"
        );

        let _ = session.grid.open("public".to_string(), "t".to_string());

        session.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req1.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));

        let status = session
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

    // --- T1's OPEN_LOCK-contention progress message (docs/MVP1-PHASE2-DESIGN.md §2) ---

    #[tokio::test]
    async fn spawn_open_reports_the_waiting_for_another_tabs_tunnel_message_when_open_lock_is_held()
    {
        // Holds the *real* `crate::tunnel::OPEN_LOCK` -- the exact static
        // `spawn_open`'s own `try_lock()` probes below -- rather than
        // duplicating its message-selection logic in the test. Because
        // `PostgresDataSource::connect_with`'s own (real, blocking) acquire
        // of the same lock happens strictly after that probe, holding it
        // here also prevents `spawn_open`'s task from ever reaching far
        // enough to spawn a real `ssh` child, so this test needs no stub-ssh
        // infrastructure and stays hermetic.
        let _held = crate::tunnel::OPEN_LOCK.lock().await;

        let conn = crate::config::Connection {
            name: "contended".to_string(),
            group: None,
            host: "dbhost".to_string(),
            port: 5432,
            database: "postgres".to_string(),
            user: "postgres".to_string(),
            password: None,
            tunnel: Some(crate::config::SshTunnel {
                host: "bastion.example.invalid".to_string(),
                user: None,
                port: None,
            }),
        };

        let (responses_tx, _responses_rx) = unbounded_channel();
        let (events_tx, mut events_rx) = unbounded_channel();
        let join = spawn_open(conn, SessionId(0), responses_tx, events_tx);

        const WANT: &str = "waiting for another tab's SSH tunnel to finish opening…";
        // Drains `Progress` messages until the specific T1 contention text
        // arrives, rather than asserting on the very first one: `spawn_open`
        // can also emit a `secret.note()` `Progress` event ahead of it when
        // `Resolved::FromEnv`'s note is non-`None` -- reachable whenever
        // `RATWARREN_PASSWORD` happens to be set in the test-runner's shell,
        // which is outside this test's control and unrelated to what it's
        // actually pinning (see the code-review finding this closes).
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
                .await
                .expect("spawn_open should send a Progress event well within 5s")
                .expect(
                    "the events channel should not close before spawn_open blocks on OPEN_LOCK",
                );
            match event {
                OpenEvent::Progress { message, .. } if message == WANT => break,
                OpenEvent::Progress { .. } => continue,
                OpenEvent::Done { .. } => panic!(
                    "spawn_open must not reach Done while this test still holds OPEN_LOCK -- \
                     connect_with's own acquire of the same lock must block behind it, and the \
                     T1 contention message ({WANT:?}) never arrived before it did"
                ),
            }
        }

        join.abort();
        let _ = join.await;
    }

    // --- Phase 3: Session::pages()/is_dirty()/dirty_titles()/close_page() ---

    fn new_session_with_pages(
        pages: crate::pages::PageTabs,
    ) -> (
        Session,
        UnboundedReceiver<WorkerRequest>,
        UnboundedReceiver<CancelRequest>,
    ) {
        let (req_tx, req_rx) = unbounded_channel();
        let (cancel_tx, cancel_rx) = unbounded_channel();
        let mut session = Session::new_with_pages(SessionId(0), "test".to_string(), None, pages);
        session.state = SessionState::TestReady {
            requests: req_tx,
            cancels: cancel_tx,
        };
        (session, req_rx, cancel_rx)
    }

    #[test]
    fn a_fresh_session_has_one_non_dirty_scratch_page() {
        let (session, _req_rx, _cancel_rx) = new_session();
        assert!(!session.is_dirty());
        assert!(session.dirty_titles().is_empty());
        assert_eq!(session.pages().tabs().len(), 1);
    }

    #[test]
    fn editing_the_active_page_makes_the_session_dirty_and_names_it() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        session.set_editor_text_for_test("some sql");
        assert!(session.is_dirty());
        assert_eq!(session.dirty_titles(), vec!["scratch".to_string()]);
    }

    #[test]
    fn close_page_without_force_on_a_dirty_page_returns_ok_false_and_removes_nothing() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        session.set_editor_text_for_test("dirty");

        let result = session.close_page(false).expect("must not error");

        assert!(!result);
        assert!(session.is_dirty(), "the dirty page must still be there");
    }

    #[test]
    fn close_page_with_force_removes_the_page_and_clears_run_page() {
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        session.set_editor_text_for_test("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        assert_eq!(
            session.run_page,
            Some(0),
            "test setup: start_run must record the page it ran from"
        );
        let _ = req_rx.try_recv();

        let result = session.close_page(true).expect("must not error");

        assert!(result);
        assert_eq!(
            session.run_page, None,
            "closing a page must clear run_page since page indices shift"
        );
    }

    #[test]
    fn delete_page_clears_run_page_since_indices_shift() {
        // Regression test for the code-review finding that `run_pending_action`'s
        // `DeletePage` branch used to call `pages_mut().delete(..)` directly,
        // bypassing the `run_page`-clearing invariant `close_page` upholds.
        // Deletes a page *before* the one a run is in flight from, so a stale
        // `run_page` would now alias a different page entirely.
        let tmp = tempfile::tempdir().expect("tempdir creation");
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let sidecar_path = tmp.path().join("missing.tabs.toml");
        let pages = crate::pages::PageTabs::restore_in(dir, sidecar_path);
        let (mut session, mut req_rx, _cancel_rx) = new_session_with_pages(pages);

        let name_a = crate::pages::PageName::new("a.sql").unwrap();
        session
            .pages_mut()
            .save_active_as(&name_a)
            .expect("save_active_as should succeed");
        session.pages_mut().new_scratch();
        assert_eq!(session.pages.active_index(), 1, "test setup");
        session.set_editor_text_for_test("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        assert_eq!(
            session.run_page,
            Some(1),
            "test setup: the run started from the second (scratch) page"
        );
        let _ = req_rx.try_recv();

        session
            .delete_page(&name_a)
            .expect("deleting a.sql should succeed");

        assert_eq!(
            session.run_page, None,
            "deleting a page must clear run_page since page indices shift, the same invariant \
             close_page upholds"
        );
    }

    #[test]
    fn starting_a_run_records_the_active_page_index_in_run_page() {
        let (mut session, _req_rx, _cancel_rx) = new_session();
        session.pages_mut().new_scratch();
        assert_eq!(session.pages.active_index(), 1, "test setup");
        session.set_editor_text_for_test("SELECT 1;");

        session.start_run(RunTarget::Buffer);

        assert_eq!(session.run_page, Some(1));
    }

    #[test]
    fn a_finished_run_clears_run_page() {
        let (mut session, mut req_rx, _cancel_rx) = new_session();
        session.set_editor_text_for_test("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        let req = match req_rx.try_recv().expect("start_run must send a request") {
            WorkerRequest::Query(req) => req,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => panic!("expected a Query request"),
        };
        assert_eq!(session.run_page, Some(0), "test setup");

        session.apply(WorkerResponse::Query(run::QueryResponse::Finished {
            id: req.id,
            result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
        }));

        assert_eq!(
            session.run_page, None,
            "run_page must be cleared once the run is done, since it no longer refers to an \
             in-flight run"
        );
    }

    #[test]
    fn jump_to_error_position_is_a_noop_when_run_page_no_longer_matches_the_active_page() {
        // Reproduces the scenario `run_page`'s doc comment exists to guard:
        // the run was started from one page, but the user has since switched
        // to another before the (here: position-less, since a real
        // `DataSourceError::Query` position can't be constructed outside a
        // live Postgres connection -- see the sibling
        // `jump_to_error_position_is_a_noop_for_error_variants_without_a_position`
        // test's comment) response arrives. The cursor must stay put on the
        // now-active page's own buffer.
        let (mut session, _req_rx, _cancel_rx) = new_session();
        session.pages_mut().new_scratch();
        session.pages_mut().select(0);
        session.set_editor_text_for_test("SELECT 1;");
        session.start_run(RunTarget::Buffer);
        assert_eq!(session.run_page, Some(0), "test setup");

        session.pages_mut().select(1);
        assert_ne!(
            session.run_page,
            Some(session.pages.active_index()),
            "test setup: the active page must have changed since the run started"
        );
        let before = session.editor().buffer().cursor();

        let unit = crate::editor::RunUnit {
            sql: "SELECT 1;".to_string(),
            span: crate::editor::ByteSpan { start: 0, end: 9 },
            start: crate::editor::Position::default(),
        };
        session.jump_to_error_position(&unit, &DataSourceError::Cancelled);

        assert_eq!(
            session.editor().buffer().cursor(),
            before,
            "a run_page/active-page mismatch must leave the (different) active page's cursor \
             untouched"
        );
    }

    #[test]
    fn no_page_operation_ever_dispatches_a_worker_request() {
        let tmp = tempfile::tempdir().expect("tempdir creation");
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let sidecar_path = tmp.path().join("missing.tabs.toml");
        let pages = crate::pages::PageTabs::restore_in(dir, sidecar_path);
        let (mut session, mut req_rx, _cancel_rx) = new_session_with_pages(pages);

        let name_a = crate::pages::PageName::new("a.sql").unwrap();
        session
            .pages_mut()
            .save_active_as(&name_a)
            .expect("save_active_as should succeed");
        session.pages_mut().new_scratch();
        let name_b = crate::pages::PageName::new("b.sql").unwrap();
        session
            .pages_mut()
            .save_active_as(&name_b)
            .expect("save_active_as should succeed");
        session.pages_mut().next();
        session.pages_mut().prev();
        session.pages_mut().select(0);
        session
            .pages_mut()
            .open(&name_b)
            .expect("opening an already-open page should succeed");
        let name_b2 = crate::pages::PageName::new("b2.sql").unwrap();
        session
            .pages_mut()
            .rename_active(&name_b2)
            .expect("rename should succeed");
        let _ = session.close_page(true);
        let _ = session.pages_mut().delete(&name_a);
        let _ = session.pages_mut().list_available();
        let _ = session.pages_mut().reload_active();

        assert!(
            req_rx.try_recv().is_err(),
            "no page operation (open/save/close/rename/delete/switch) must ever dispatch a \
             WorkerRequest"
        );
    }
}
