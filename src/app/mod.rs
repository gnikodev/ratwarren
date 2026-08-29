pub mod keymap;
pub mod message;
pub mod run;
pub mod session;
pub mod status;
pub mod worker;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::config::Config;
use crate::ui::picker::{PickerCommand, PickerState, PickerWidget};
use keymap::{AppCommand, map_key};
use message::SessionResponse;
use session::{OpenEvent, Session, SessionAction, SessionId, SessionState, spawn_open};
use status::StatusKind;

const TAB_HINT: &str = "Ctrl+T open  Ctrl+W close  Ctrl+N/P switch";
const EMPTY_FOOTER: &str = "↑/↓ select  ⏎ open  q quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Grid,
    Editor,
}

pub struct App {
    sessions: Vec<Session>,
    // Invariant: sessions.is_empty() || active < sessions.len().
    active: usize,
    next_session_id: u64,
    config: Config,
    picker: Option<PickerState>,
    // Kept alongside its receiver so `responses.recv()` never observes the
    // channel closing -- every session's worker/canceller holds a clone of
    // this same sender.
    responses_tx: UnboundedSender<SessionResponse>,
    responses: UnboundedReceiver<SessionResponse>,
    open_tx: UnboundedSender<OpenEvent>,
    opens: UnboundedReceiver<OpenEvent>,
    should_quit: bool,
}

impl App {
    pub fn new(config: Config) -> App {
        let (responses_tx, responses) = unbounded_channel();
        let (open_tx, opens) = unbounded_channel();
        App {
            sessions: Vec::new(),
            active: 0,
            next_session_id: 0,
            config,
            picker: None,
            responses_tx,
            responses,
            open_tx,
            opens,
            should_quit: false,
        }
    }

    pub fn open_connection(&mut self, name: &str) {
        let Some(conn) = self.config.connection(name) else {
            return;
        };
        let conn = conn.clone();

        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let session = Session::new(id, conn.name.clone(), conn.group.clone());
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        self.picker = None;

        spawn_open(conn, id, self.responses_tx.clone(), self.open_tx.clone());
    }

    /// `RequestId` is scoped to *(SessionId, state machine)*: `RunState`'s
    /// and `DataGridState`'s counters are independent and both start at 0,
    /// so `RequestId(0)` in session A and `RequestId(0)` in session B are
    /// unrelated values. Routing is therefore by `SessionId` first and
    /// unconditionally; a `RequestId` may only be compared after a session
    /// has been located. This must NEVER fall back to the active session
    /// when the lookup fails -- a response for a closed or unknown session
    /// is silently dropped, not misrouted.
    pub fn apply(&mut self, msg: SessionResponse) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == msg.session) else {
            return;
        };
        session.apply(msg.response);
    }

    pub fn apply_open_event(&mut self, ev: OpenEvent) {
        match ev {
            OpenEvent::Progress { session, message } => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session) {
                    session.set_connecting_message(message);
                }
            }
            OpenEvent::Done { session, result } => {
                match self.sessions.iter_mut().find(|s| s.id == session) {
                    Some(session) => match result {
                        Ok(handle) => session.on_connected(handle),
                        Err(message) => session.on_failed(message),
                    },
                    // The tab this open was for was closed while it was still
                    // in flight. The handle it just produced was never
                    // attached to anything and must still be torn down
                    // properly (not bare-dropped, which would skip
                    // `close().await`'s `conn_task.abort()`).
                    None => {
                        if let Ok(handle) = result {
                            tokio::spawn(async move { handle.shutdown().await });
                        }
                    }
                }
            }
        }
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }

        let Some(session) = self.sessions.get(self.active) else {
            // No sessions and (unexpectedly) no picker open either -- closing
            // the last tab always reopens the picker, so this shouldn't
            // normally be reachable, but there is no `Focus` to feed
            // `map_key` here regardless. Handle only the two keys that must
            // always work.
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) => self.should_quit = true,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
                _ => {}
            }
            return;
        };

        match map_key(key, session.focus()) {
            Some(AppCommand::OpenPicker) => self.open_picker(),
            Some(AppCommand::CloseTab) => self.close_active(),
            Some(AppCommand::NextTab) => self.next_tab(),
            Some(AppCommand::PrevTab) => self.prev_tab(),
            _ => {
                if let Some(SessionAction::Quit) = self.active_mut().and_then(|s| s.on_key(key)) {
                    self.should_quit = true;
                }
            }
        }
    }

    fn picker_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match (key.code, key.modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                if let Some(picker) = &mut self.picker {
                    picker.command(PickerCommand::MoveUp);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                if let Some(picker) = &mut self.picker {
                    picker.command(PickerCommand::MoveDown);
                }
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                if let Some(picker) = &mut self.picker {
                    picker.command(PickerCommand::First);
                }
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                if let Some(picker) = &mut self.picker {
                    picker.command(PickerCommand::Last);
                }
            }
            (KeyCode::Enter, _) => {
                let name = self
                    .picker
                    .as_ref()
                    .and_then(|p| p.selected_connection())
                    .map(str::to_string);
                if let Some(name) = name {
                    self.open_connection(&name);
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                // Closing the picker with no sessions to fall back to would
                // leave nothing on screen and no `Focus`-driven key to
                // reopen it -- only let it close when there's a session
                // underneath.
                if !self.sessions.is_empty() {
                    self.picker = None;
                }
            }
            // `q`/`Ctrl+C`-quits-from-picker only applies in the zero-sessions
            // state, where `Esc` has nowhere useful to go (see the `Esc`
            // branch above) and there is no open session whose work could be
            // lost. When the picker was opened via `Ctrl+T` with existing
            // tabs still open, quitting from what looks like a passive
            // "browse connections" action would risk losing an open session's
            // state -- `Esc`/`Ctrl+T` is the only way to close the overlay in
            // that case, same as any other key not meaningful to the picker.
            (KeyCode::Char('q'), _) if self.sessions.is_empty() => self.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) if self.sessions.is_empty() => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        self.render_tabs(frame, chunks[0]);
        if let Some(session) = self.sessions.get_mut(self.active) {
            session.render(frame, chunks[1]);
        }
        self.render_footer(frame, chunks[2]);

        if let Some(picker) = &mut self.picker {
            frame.render_stateful_widget(PickerWidget::new(), area, picker);
        }
    }

    fn render_tabs(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let titles: Vec<Line> = self.sessions.iter().map(Session::tab_title).collect();
        let select = if self.sessions.is_empty() {
            None
        } else {
            Some(self.active)
        };
        let tabs = Tabs::new(titles)
            .select(select)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(tabs, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let Some(session) = self.sessions.get(self.active) else {
            frame.render_widget(Paragraph::new(EMPTY_FOOTER), area);
            return;
        };

        let mut spans = match session.status() {
            Some(status) => {
                let style = match status.kind {
                    StatusKind::Info => Style::default(),
                    StatusKind::Error => Style::default().fg(Color::Red),
                    StatusKind::Warn => Style::default().fg(Color::Yellow),
                };
                vec![Span::styled(status.text.clone(), style)]
            }
            None => {
                let text = format!("{}  {}", session.footer_text(), TAB_HINT);
                vec![Span::raw(text)]
            }
        };

        // T2's guaranteed-visible half (docs/MVP1-PHASE2-DESIGN.md §2 T2 item
        // 5): the tab title's sticky `⚠` can be clipped by `Tabs` on a long
        // tab strip (that widget doesn't scroll/elide), so this line is shown
        // for the active session regardless of what else the footer/status
        // line is currently displaying, not only when `status()` is `None`.
        if let Some(warning) = session.tunnel_warning() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(warning, Style::default().fg(Color::Yellow)));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub async fn shutdown(self) {
        for session in self.sessions {
            if let SessionState::Ready(handle) = session.into_state() {
                handle.shutdown().await;
            }
        }
    }

    fn active_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }

    fn open_picker(&mut self) {
        self.picker = Some(PickerState::from_config(&self.config));
    }

    fn ensure_picker_if_no_sessions(&mut self) {
        if self.sessions.is_empty() && self.picker.is_none() {
            self.open_picker();
        }
    }

    fn close_active(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let session = self.sessions.remove(self.active);
        if let SessionState::Ready(handle) = session.into_state() {
            // Detached: `on_key` is sync and can't await. Safe at process
            // exit -- dropping the task at runtime shutdown drops the
            // `SourceHandle` -> `PostgresDataSource` -> tunnel, and the
            // tunnel's own Drop impl reaps the ssh child regardless.
            tokio::spawn(async move { handle.shutdown().await });
        }
        // A `Connecting` session's in-flight `OpenEvent::Done` is handled by
        // `apply_open_event`'s "unknown session" branch above, which spawns
        // `handle.shutdown()` on the handle it receives rather than
        // resurrecting this session or bare-dropping it.

        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        }
        self.ensure_picker_if_no_sessions();
    }

    fn next_tab(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.active = (self.active + 1) % self.sessions.len();
    }

    fn prev_tab(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.active = (self.active + self.sessions.len() - 1) % self.sessions.len();
    }
}

pub async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    use futures_util::StreamExt;

    let mut events = crossterm::event::EventStream::new();
    app.ensure_picker_if_no_sessions();
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
            msg = app.responses.recv() => match msg {
                Some(m) => app.apply(m),
                // Unreachable: `app` always holds its own clone of
                // `responses_tx`, so the channel never actually closes.
                None => return Err(std::io::Error::other("response channel closed unexpectedly")),
            },
            ev = app.opens.recv() => match ev {
                Some(e) => app.apply_open_event(e),
                // Unreachable: same reasoning as above, via `open_tx`.
                None => return Err(std::io::Error::other("open channel closed unexpectedly")),
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
    use crate::app::message::{WorkerRequest, WorkerResponse};
    use crate::app::run;
    use crate::editor::RunTarget;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    // A snapshot of everything `App::apply`'s routing invariant promises
    // stays untouched -- status, whether a run is active, the grid's
    // content, and the editor's text -- compared by value rather than by
    // asserting on any one field in isolation, per the design doc's "byte
    // identical" wording.
    fn snapshot(session: &Session) -> (Option<(StatusKind, String)>, bool, String, String) {
        (
            session.status().map(|s| (s.kind, s.text.clone())),
            session.run_is_active_for_test(),
            format!("{:?}", session.grid_content_for_test()),
            session.editor_text_for_test(),
        )
    }

    // --- Routing (the core safety property; §9 items 1-2) ---

    #[test]
    fn apply_routes_by_session_id_first_and_a_response_for_b_leaves_a_byte_identical() {
        let (mut session_a, mut req_a, _cancel_a) =
            session::test_ready_session(SessionId(0), "a".to_string(), None);
        let (mut session_b, mut req_b, _cancel_b) =
            session::test_ready_session(SessionId(1), "b".to_string(), None);

        // Both sessions' RunState (and, via `begin_query`, their
        // DataGridState) counters independently start at 0 -- the very
        // first run in each session mints the exact same RequestId, which
        // is the collision this test exists to guard against.
        session_a.set_editor_text_for_test("SELECT 1;");
        session_a.start_run(RunTarget::Buffer);
        session_b.set_editor_text_for_test("SELECT 2;");
        session_b.start_run(RunTarget::Buffer);

        let req_a_query = match req_a
            .try_recv()
            .expect("session A must have sent a request")
        {
            WorkerRequest::Query(q) => q,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => {
                panic!("expected a Query request from session A")
            }
        };
        let req_b_query = match req_b
            .try_recv()
            .expect("session B must have sent a request")
        {
            WorkerRequest::Query(q) => q,
            WorkerRequest::Tree(_) | WorkerRequest::Grid(_) => {
                panic!("expected a Query request from session B")
            }
        };
        assert_eq!(
            req_a_query.id, req_b_query.id,
            "test setup: both sessions' first run must mint the same RequestId"
        );

        let before = snapshot(&session_a);

        let mut app = App::new(Config::default());
        app.sessions.push(session_a);
        app.sessions.push(session_b);
        app.active = 0; // session A (index 0) is the active tab.

        // A response tagged for B, carrying the SAME RequestId A's own run
        // is also currently waiting on.
        app.apply(SessionResponse {
            session: SessionId(1),
            response: WorkerResponse::Query(run::QueryResponse::Finished {
                id: req_b_query.id,
                result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
            }),
        });

        let after = snapshot(&app.sessions[0]);
        assert_eq!(
            before, after,
            "a SessionResponse tagged for B must leave A's status/run/grid/editor completely \
             untouched, not merely leave B changed"
        );

        // B's own response must actually have taken effect -- otherwise this
        // test would trivially pass by nothing being routed anywhere.
        assert!(
            !app.sessions[1].run_is_active_for_test(),
            "test sanity: B's own matching response must have finished B's run"
        );
    }

    #[test]
    fn apply_for_an_unknown_session_id_mutates_nothing_and_never_falls_through_to_the_active_session()
     {
        let (mut session_a, _req_a, _cancel_a) =
            session::test_ready_session(SessionId(0), "a".to_string(), None);
        session_a.set_editor_text_for_test("SELECT 1;");
        session_a.start_run(RunTarget::Buffer);
        let before = snapshot(&session_a);

        let mut app = App::new(Config::default());
        app.sessions.push(session_a);
        app.active = 0;

        app.apply(SessionResponse {
            session: SessionId(999), // never existed / already closed
            response: WorkerResponse::Query(run::QueryResponse::Finished {
                id: crate::ui::RequestId(0),
                result: Ok(run::QueryOutcome::NoResultSet { rows_affected: 0 }),
            }),
        });

        assert_eq!(
            app.sessions.len(),
            1,
            "no session must be created or removed"
        );
        let after = snapshot(&app.sessions[0]);
        assert_eq!(
            before, after,
            "a response for an unknown SessionId must be silently dropped, not misrouted to the \
             active session"
        );
    }

    // --- Tabs (§9 items 3-4) ---

    fn push_ready(app: &mut App, id: u64, name: &str) {
        let (session, _req, _cancel) =
            session::test_ready_session(SessionId(id), name.to_string(), None);
        app.sessions.push(session);
    }

    #[test]
    fn closing_a_middle_tab_removes_it_and_active_lands_on_the_tab_that_shifted_into_its_slot() {
        let mut app = App::new(Config::default());
        push_ready(&mut app, 0, "a");
        push_ready(&mut app, 1, "b");
        push_ready(&mut app, 2, "c");
        app.active = 1; // "b"

        app.close_active();

        assert_eq!(
            app.sessions
                .iter()
                .map(|s| s.connection_name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(
            app.active, 1,
            "active must land on \"c\", which shifted into b's old slot"
        );
    }

    #[test]
    fn closing_the_tab_at_the_last_index_moves_active_back_to_the_new_last_tab() {
        let mut app = App::new(Config::default());
        push_ready(&mut app, 0, "a");
        push_ready(&mut app, 1, "b");
        push_ready(&mut app, 2, "c");
        app.active = 2; // "c", the last index

        app.close_active();

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active, 1);
    }

    #[test]
    fn closing_the_only_tab_leaves_sessions_empty_and_auto_opens_the_picker() {
        let mut app = App::new(Config::default());
        push_ready(&mut app, 0, "solo");
        app.active = 0;

        app.close_active();

        assert!(app.sessions.is_empty());
        assert!(
            app.picker.is_some(),
            "closing the only tab must auto-open the picker rather than leaving nothing on screen"
        );
    }

    #[test]
    fn next_tab_and_prev_tab_wrap_around_at_the_ends() {
        let mut app = App::new(Config::default());
        push_ready(&mut app, 0, "a");
        push_ready(&mut app, 1, "b");
        push_ready(&mut app, 2, "c");
        app.active = 0;

        app.next_tab();
        assert_eq!(app.active, 1);
        app.next_tab();
        assert_eq!(app.active, 2);
        app.next_tab();
        assert_eq!(
            app.active, 0,
            "next_tab from the last tab must wrap to the first"
        );

        app.prev_tab();
        assert_eq!(
            app.active, 2,
            "prev_tab from the first tab must wrap to the last"
        );
        app.prev_tab();
        assert_eq!(app.active, 1);
    }

    #[test]
    fn next_tab_and_prev_tab_are_noops_when_sessions_is_empty() {
        let mut app = App::new(Config::default());
        app.next_tab();
        assert_eq!(app.active, 0);
        app.prev_tab();
        assert_eq!(app.active, 0);
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn q_from_the_empty_no_sessions_no_picker_state_quits() {
        let mut app = App::new(Config::default());
        assert!(app.sessions.is_empty());
        assert!(
            app.picker.is_none(),
            "test setup: this state is reachable before app::run's ensure_picker_if_no_sessions has ever run"
        );

        app.on_key(key(KeyCode::Char('q')));

        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_from_the_empty_no_sessions_no_picker_state_quits() {
        let mut app = App::new(Config::default());
        app.on_key(ctrl_key(KeyCode::Char('c')));
        assert!(app.should_quit());
    }

    #[test]
    fn closing_a_connecting_tab_then_its_late_open_done_does_not_resurrect_a_session() {
        // `Session::new` starts in `Connecting` state -- exactly the state an
        // in-flight `spawn_open` task's tab is in while its `OpenEvent::Done`
        // is still outstanding.
        let mut app = App::new(Config::default());
        app.sessions
            .push(Session::new(SessionId(0), "conn".to_string(), None));
        app.active = 0;

        app.close_active();
        assert!(
            app.sessions.is_empty(),
            "test setup: closing the only (Connecting) tab empties sessions"
        );

        // The late `Done` arrives after the tab is gone. `Err(..)` is used
        // here (rather than `Ok(handle)`) because `SourceHandle` is a
        // concrete `Arc<PostgresDataSource>` that can only be constructed
        // against a real, live Postgres connection -- see the doc comment on
        // `SessionState::TestReady`. This still fully exercises the "must
        // not resurrect" half of the requirement; the "the handle must be
        // shutdown()'d, not bare-dropped" half (the `Ok` branch) is verified
        // by code inspection only -- see this crate's test report.
        app.apply_open_event(OpenEvent::Done {
            session: SessionId(0),
            result: Err("connection refused".to_string()),
        });

        assert!(
            app.sessions.is_empty(),
            "a late OpenEvent::Done for a closed session id must not resurrect a session"
        );
        assert!(!app.should_quit());
    }

    // --- Picker interception (§9 item 7) ---

    #[test]
    fn picker_open_intercepts_every_key_even_ones_that_would_insert_into_a_focused_editor() {
        let (mut session, req_rx, _cancel) =
            session::test_ready_session(SessionId(0), "conn".to_string(), None);
        session.on_key(key(KeyCode::Tab)); // Focus::Tree -> Focus::Editor
        assert_eq!(
            session.focus(),
            Focus::Editor,
            "test setup: focus must be Editor"
        );

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.picker = Some(PickerState::from_config(&app.config));

        app.on_key(key(KeyCode::Char('x')));

        assert_eq!(
            app.sessions[0].editor_text_for_test(),
            "",
            "a character key must never reach the session's editor while the picker is open"
        );
        drop(req_rx);
    }

    #[test]
    fn q_from_the_picker_does_not_quit_or_close_the_picker_when_a_session_is_already_open() {
        // The inverse of `q_from_the_empty_no_sessions_no_picker_state_quits`:
        // `q`/`Ctrl+C` quitting unconditionally whenever the picker is open
        // used to be a data-loss trap for a tab opened via `Ctrl+T` with an
        // existing session already open -- see the picker_key `q`/`Ctrl+C`
        // arms' doc comment for why they're now guarded on
        // `self.sessions.is_empty()`.
        let (session, req_rx, _cancel) =
            session::test_ready_session(SessionId(0), "conn".to_string(), None);

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.picker = Some(PickerState::from_config(&app.config));

        app.on_key(key(KeyCode::Char('q')));

        assert!(
            !app.should_quit(),
            "q must not quit the app while the picker is open over an existing session"
        );
        assert!(
            app.picker.is_some(),
            "q is not a picker command, so the picker must remain open"
        );
        assert_eq!(
            app.sessions.len(),
            1,
            "the existing session must not be touched"
        );
        drop(req_rx);
    }

    #[test]
    fn ctrl_c_from_the_picker_does_not_quit_when_a_session_is_already_open() {
        let (session, req_rx, _cancel) =
            session::test_ready_session(SessionId(0), "conn".to_string(), None);

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.picker = Some(PickerState::from_config(&app.config));

        app.on_key(ctrl_key(KeyCode::Char('c')));

        assert!(
            !app.should_quit(),
            "Ctrl+C must not quit the app while the picker is open over an existing session"
        );
        assert!(app.picker.is_some(), "the picker must remain open");
        drop(req_rx);
    }
}
