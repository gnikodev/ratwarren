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
use crate::pages::PageName;
use crate::ui::pages::{PagesPromptState, PagesPromptWidget, PendingAction};
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
    // Deliberately its own field rather than folded into `picker` or a
    // general overlay enum -- see `PagesPromptState`'s doc comment.
    pages_prompt: Option<PagesPromptState>,
    // The session tab that was active when `save_discard_prompt` began a
    // save-then-`then` flow, captured *before* `continue_save_flow` moves
    // `self.active` around to bring an unnamed scratch page's `SaveAs`
    // prompt into focus. Restored if that `SaveAs` prompt is cancelled
    // (Esc) instead of left wherever the flow happened to move it. `None`
    // outside of an in-progress save flow (including for the ordinary
    // explicit Ctrl+S/F2-rename `SaveAs` prompts, which never touch
    // `self.active` in the first place and so need no restore). Cleared by
    // `run_pending_action` once the flow's real terminal action runs (not
    // on its own `SaveRemaining` recursion, which is still mid-flow).
    save_flow_restore: Option<usize>,
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
            pages_prompt: None,
            save_flow_restore: None,
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

        if self.pages_prompt.is_some() {
            self.pages_prompt_key(key);
            return;
        }

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
                (KeyCode::Char('q'), _) => self.request_quit(),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.request_quit(),
                _ => {}
            }
            return;
        };

        match map_key(key, session.focus()) {
            Some(AppCommand::OpenPicker) => self.open_picker(),
            Some(AppCommand::CloseTab) => self.close_active(),
            Some(AppCommand::NextTab) => self.next_tab(),
            Some(AppCommand::PrevTab) => self.prev_tab(),
            Some(AppCommand::OpenPageList) => self.open_page_list(),
            Some(AppCommand::SavePage) => self.save_active_page(),
            Some(AppCommand::RenamePage) => self.rename_active_page(),
            Some(AppCommand::NewPage) => {
                if let Some(session) = self.active_mut() {
                    session.pages_mut().new_scratch();
                }
            }
            Some(AppCommand::ClosePage) => self.close_active_page(),
            Some(AppCommand::NextPage) => {
                if let Some(session) = self.active_mut() {
                    session.pages_mut().next();
                }
            }
            Some(AppCommand::PrevPage) => {
                if let Some(session) = self.active_mut() {
                    session.pages_mut().prev();
                }
            }
            Some(AppCommand::ReloadPage) => self.reload_active_page(),
            _ => {
                if let Some(SessionAction::Quit) = self.active_mut().and_then(|s| s.on_key(key)) {
                    self.request_quit();
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
            (KeyCode::Char('q'), _) if self.sessions.is_empty() => self.request_quit(),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) if self.sessions.is_empty() => {
                self.request_quit();
            }
            _ => {}
        }
    }

    /// Mirrors `picker_key`'s dispatch shape: raw `KeyCode`/`KeyModifiers`
    /// matched directly against the active `PagesPromptState` variant,
    /// rather than routing through `map_key`/`AppCommand` (this overlay's
    /// keys -- `y`/`n`, free-text input -- aren't part of the session
    /// keymap).
    fn pages_prompt_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        let Some(prompt) = &self.pages_prompt else {
            return;
        };

        match prompt {
            PagesPromptState::Open { .. } => match (key.code, key.modifiers) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    self.pages_prompt
                        .as_mut()
                        .expect("checked Some above")
                        .move_up();
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    self.pages_prompt
                        .as_mut()
                        .expect("checked Some above")
                        .move_down();
                }
                (KeyCode::Enter, _) => self.confirm_open_prompt(),
                (KeyCode::Char('d'), _) => self.request_delete_selected_page(),
                (KeyCode::Esc, _) => self.pages_prompt = None,
                _ => {}
            },
            PagesPromptState::Discard { then, .. } => {
                // A delete-confirmation prompt (`then` is `DeletePage`) has
                // exactly two valid outcomes -- delete (y/Enter) or cancel
                // (n/Esc) -- there is nothing to "save" here. `s` must stay
                // unbound for this variant: `dirty_pages_for` returns an
                // empty scope for `DeletePage`, so routing `s` through the
                // ordinary save-then-`then` path would fall straight through
                // to `run_pending_action(DeletePage(name))` and silently
                // delete the page instead of saving it.
                let is_delete_confirm = matches!(then, PendingAction::DeletePage(_));
                match (key.code, key.modifiers) {
                    (KeyCode::Enter, _) | (KeyCode::Char('y'), _) => self.confirm_discard_prompt(),
                    (KeyCode::Char('s'), _) if !is_delete_confirm => self.save_discard_prompt(),
                    (KeyCode::Esc, _) | (KeyCode::Char('n'), _) => {
                        // A `Discard` reached here either as the original
                        // prompt or re-raised by `continue_save_flow` after a
                        // save `Err` (see `discard_with_error`) -- either way,
                        // cancelling it abandons the whole flow, so any
                        // pending `save_flow_restore` from `save_discard_prompt`
                        // must be dropped too. Leaving it set would let a
                        // later, unrelated `SaveAs` cancellation restore
                        // `self.active` to this stale index.
                        self.save_flow_restore = None;
                        self.pages_prompt = None;
                    }
                    _ => {}
                }
            }
            PagesPromptState::SaveAs { .. } => match (key.code, key.modifiers) {
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    self.pages_prompt
                        .as_mut()
                        .expect("checked Some above")
                        .insert_char(c);
                }
                (KeyCode::Backspace, _) => {
                    self.pages_prompt
                        .as_mut()
                        .expect("checked Some above")
                        .backspace();
                }
                (KeyCode::Enter, _) => self.confirm_save_as_prompt(),
                (KeyCode::Esc, _) => {
                    // If this `SaveAs` was raised mid save-flow (an unnamed
                    // scratch page needing a name before a discard-prompt's
                    // "save" choice could proceed -- see `continue_save_flow`),
                    // cancelling it abandons the whole pending action, but
                    // `self.active` may have been moved to whichever
                    // session owned that scratch page. Restore it rather
                    // than leaving the tab strip focused wherever the flow
                    // happened to land.
                    if let Some(idx) = self.save_flow_restore.take()
                        && idx < self.sessions.len()
                    {
                        self.active = idx;
                    }
                    self.pages_prompt = None;
                }
                _ => {}
            },
        }
    }

    fn confirm_open_prompt(&mut self) {
        let name = self
            .pages_prompt
            .as_ref()
            .and_then(|p| p.selected_page())
            .cloned();
        self.pages_prompt = None;
        let (Some(name), Some(session)) = (name, self.active_mut()) else {
            return;
        };
        if let Err(e) = session.pages_mut().open(&name) {
            session.set_error_status(crate::ui::error_chain(&e));
        }
    }

    fn confirm_discard_prompt(&mut self) {
        let Some(PagesPromptState::Discard { then, .. }) = self.pages_prompt.take() else {
            return;
        };
        self.run_pending_action(then);
    }

    /// The `Discard` prompt's `s` ("save") choice: saves the dirty pages
    /// `then` implies the scope of, rather than discarding them, then
    /// proceeds with `then` exactly as the ordinary discard path would.
    fn save_discard_prompt(&mut self) {
        let Some(PagesPromptState::Discard { then, .. }) = self.pages_prompt.take() else {
            return;
        };
        // Captured here, at the flow's actual entry point, rather than
        // inside `continue_save_flow` itself -- that function re-enters via
        // `PendingAction::SaveRemaining` every time an unnamed scratch page
        // needs a `SaveAs` prompt, and re-capturing there would overwrite
        // this with whatever `self.active` the flow had already moved to.
        self.save_flow_restore = Some(self.active);
        self.continue_save_flow(then);
    }

    /// Saves every still-dirty page in `then`'s scope, one at a time,
    /// recomputing the scope fresh before each save (a page that was just
    /// saved is no longer dirty and drops out of the scope on its own,
    /// which is also what lets a retry after `Err` below pick up exactly
    /// where it left off). An unnamed scratch page needs a name first: this
    /// suspends into a `SaveAs` prompt wrapping the remaining work in
    /// `PendingAction::SaveRemaining`, resuming here via `run_pending_action`
    /// once that resolves. Never closes/opens a session or page itself, so
    /// the `(session_index, page_index)` pairs `dirty_pages_for` returns
    /// stay valid across the whole flow.
    fn continue_save_flow(&mut self, then: PendingAction) {
        loop {
            let queue = self.dirty_pages_for(&then);
            let Some(&(session_idx, page_idx)) = queue.first() else {
                break;
            };
            let session = self.sessions.get_mut(session_idx).expect(
                "dirty_pages_for only returns indices into the current self.sessions, which \
                 this loop never adds to or removes from",
            );
            session.pages_mut().select(page_idx);
            match session.pages_mut().save_active() {
                Ok(crate::pages::SaveOutcome::Saved) => {}
                Ok(crate::pages::SaveOutcome::NeedsName) => {
                    self.active = session_idx;
                    self.pages_prompt = Some(PagesPromptState::save_as(
                        PendingAction::SaveRemaining(Box::new(then)),
                    ));
                    return;
                }
                Err(e) => {
                    let message = crate::ui::error_chain(&e);
                    session.set_error_status(message.clone());
                    let titles = self.titles_for(&queue);
                    self.pages_prompt =
                        Some(PagesPromptState::discard_with_error(titles, then, message));
                    return;
                }
            }
        }
        self.run_pending_action(then);
    }

    /// The `(session_index, page_index)` pairs still dirty within whatever
    /// scope `then` implies -- one page for `ClosePage`, a session's pages
    /// for `CloseTab`, every session's pages for `Quit`. Empty for `None`,
    /// `SaveRemaining`, and `DeletePage`, none of which name a save scope.
    fn dirty_pages_for(&self, then: &PendingAction) -> Vec<(usize, usize)> {
        match then {
            PendingAction::ClosePage => self
                .sessions
                .get(self.active)
                .into_iter()
                .flat_map(|session| {
                    let idx = session.pages().active_index();
                    let dirty = session.pages().tabs()[idx].is_dirty();
                    dirty.then_some((self.active, idx))
                })
                .collect(),
            PendingAction::CloseTab => self
                .sessions
                .get(self.active)
                .into_iter()
                .flat_map(|session| {
                    session
                        .pages()
                        .tabs()
                        .iter()
                        .enumerate()
                        .filter(|(_, page)| page.is_dirty())
                        .map(|(i, _)| (self.active, i))
                        .collect::<Vec<_>>()
                })
                .collect(),
            PendingAction::Quit => self
                .sessions
                .iter()
                .enumerate()
                .flat_map(|(session_idx, session)| {
                    session
                        .pages()
                        .tabs()
                        .iter()
                        .enumerate()
                        .filter(|(_, page)| page.is_dirty())
                        .map(move |(i, _)| (session_idx, i))
                })
                .collect(),
            PendingAction::None
            | PendingAction::SaveRemaining(_)
            | PendingAction::DeletePage(_) => Vec::new(),
        }
    }

    fn titles_for(&self, pages: &[(usize, usize)]) -> Vec<String> {
        pages
            .iter()
            .filter_map(|&(session_idx, page_idx)| {
                self.sessions
                    .get(session_idx)
                    .map(|session| session.pages().tabs()[page_idx].title().to_string())
            })
            .collect()
    }

    fn request_delete_selected_page(&mut self) {
        let Some(name) = self
            .pages_prompt
            .as_ref()
            .and_then(|p| p.selected_page())
            .cloned()
        else {
            return;
        };
        self.pages_prompt = Some(PagesPromptState::confirm_delete(name));
    }

    fn reload_active_page(&mut self) {
        let Some(session) = self.active_mut() else {
            return;
        };
        match session.pages_mut().reload_active() {
            Ok(()) => session.set_info_status("reloaded".to_string()),
            Err(e) => session.set_error_status(crate::ui::error_chain(&e)),
        }
    }

    fn confirm_save_as_prompt(&mut self) {
        let Some(PagesPromptState::SaveAs {
            input,
            then,
            rename,
            ..
        }) = self.pages_prompt.take()
        else {
            return;
        };

        let typed = input.trim().to_string();
        if typed.is_empty() {
            self.pages_prompt = Some(PagesPromptState::SaveAs {
                input,
                error: Some("name must not be empty".to_string()),
                then,
                rename,
            });
            return;
        }
        let file_name = if typed.ends_with(".sql") {
            typed
        } else {
            format!("{typed}.sql")
        };
        let name = match PageName::new(&file_name) {
            Ok(name) => name,
            Err(e) => {
                self.pages_prompt = Some(PagesPromptState::SaveAs {
                    input,
                    error: Some(crate::ui::error_chain(&e)),
                    then,
                    rename,
                });
                return;
            }
        };

        let Some(session) = self.active_mut() else {
            return;
        };
        let result = if rename {
            session.pages_mut().rename_active(&name)
        } else {
            session.pages_mut().save_active_as(&name)
        };
        match result {
            Ok(()) => self.run_pending_action(then),
            Err(e) => {
                self.pages_prompt = Some(PagesPromptState::SaveAs {
                    input,
                    error: Some(crate::ui::error_chain(&e)),
                    then,
                    rename,
                });
            }
        }
    }

    fn run_pending_action(&mut self, then: PendingAction) {
        // The flow's real terminal action is about to run (or this call is
        // unrelated to a save flow at all) -- either way `save_flow_restore`
        // is no longer needed. `SaveRemaining` is the one exception: it's
        // `continue_save_flow` recursing into itself mid-flow, not the
        // flow's actual conclusion, and clearing here would drop the
        // restore point before a later `SaveAs` in the same flow could be
        // cancelled.
        if !matches!(then, PendingAction::SaveRemaining(_)) {
            self.save_flow_restore = None;
        }
        match then {
            PendingAction::None => {}
            PendingAction::ClosePage => {
                if let Some(session) = self.active_mut() {
                    let _ = session.close_page(true);
                }
            }
            PendingAction::CloseTab => self.close_active_forced(),
            PendingAction::Quit => self.should_quit = true,
            PendingAction::SaveRemaining(then) => self.continue_save_flow(*then),
            PendingAction::DeletePage(name) => {
                if let Some(session) = self.active_mut() {
                    match session.delete_page(&name) {
                        Ok(()) => session.set_info_status(format!("deleted {}", name.as_str())),
                        Err(e) => session.set_error_status(crate::ui::error_chain(&e)),
                    }
                }
            }
        }
    }

    fn open_page_list(&mut self) {
        let Some(session) = self.active_mut() else {
            return;
        };
        match session.pages().list_available() {
            Ok(rows) => self.pages_prompt = Some(PagesPromptState::open(rows)),
            Err(e) => session.set_error_status(crate::ui::error_chain(&e)),
        }
    }

    fn save_active_page(&mut self) {
        let Some(session) = self.active_mut() else {
            return;
        };
        match session.pages_mut().save_active() {
            Ok(crate::pages::SaveOutcome::Saved) => {
                session.set_info_status("saved".to_string());
            }
            Ok(crate::pages::SaveOutcome::NeedsName) => {
                self.pages_prompt = Some(PagesPromptState::save_as(PendingAction::None));
            }
            Err(e) => session.set_error_status(crate::ui::error_chain(&e)),
        }
    }

    fn rename_active_page(&mut self) {
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        let current_title = session.pages().active().title().to_string();
        self.pages_prompt = Some(PagesPromptState::rename(&current_title));
    }

    fn close_active_page(&mut self) {
        let Some(session) = self.active_mut() else {
            return;
        };
        match session.close_page(false) {
            Ok(true) => {}
            Ok(false) => {
                let titles = vec![session.pages().active().title().to_string()];
                self.pages_prompt =
                    Some(PagesPromptState::discard(titles, PendingAction::ClosePage));
            }
            Err(e) => session.set_error_status(crate::ui::error_chain(&e)),
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
        if let Some(prompt) = &mut self.pages_prompt {
            frame.render_stateful_widget(PagesPromptWidget::new(), area, prompt);
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

    /// The sole funnel for setting `should_quit`: quitting while any
    /// session has unsaved page edits raises a `Discard` prompt (listing
    /// every dirty page across every session) instead of quitting
    /// immediately.
    pub fn request_quit(&mut self) {
        let dirty_titles: Vec<String> = self
            .sessions
            .iter()
            .flat_map(Session::dirty_titles)
            .collect();
        if dirty_titles.is_empty() {
            self.should_quit = true;
        } else {
            self.pages_prompt = Some(PagesPromptState::discard(dirty_titles, PendingAction::Quit));
        }
    }

    pub async fn shutdown(self) {
        for session in self.sessions {
            // Best-effort: never surfaces an error, never blocks the
            // teardown below. The terminal is already torn down by the time
            // `shutdown` runs, so this is not a prompt point -- unsaved
            // pages are simply not persisted to the sidecar past this point
            // (their content is still safely under the user's control on
            // disk from whatever the last successful `save`/`save_as` was;
            // only the open-tab/cursor bookkeeping is lost).
            session.pages().persist_sidecar();
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
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        if session.is_dirty() {
            self.pages_prompt = Some(PagesPromptState::discard(
                session.dirty_titles(),
                PendingAction::CloseTab,
            ));
            return;
        }
        self.close_active_forced();
    }

    /// The actual removal + teardown, skipping the dirty-pages gate --
    /// `close_active` calls this directly once there's nothing dirty to
    /// confirm, and `run_pending_action`'s `CloseTab` branch calls this
    /// after the user has already confirmed discarding the dirty pages.
    fn close_active_forced(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if let Some(session) = self.sessions.get(self.active) {
            session.pages().persist_sidecar();
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
        app.sessions.push(Session::new_with_pages(
            SessionId(0),
            "conn".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        ));
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

    // --- Phase 3: dirty-aware close/quit, request_quit as the sole should_quit funnel ---

    fn dirty_scratch_session(id: u64, name: &str) -> Session {
        let mut session = Session::new_with_pages(
            SessionId(id),
            name.to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        session
            .pages_mut()
            .editor_mut()
            .buffer_mut()
            .insert_str("unsaved edits");
        session
    }

    #[test]
    fn close_active_on_a_session_with_a_dirty_page_opens_the_discard_prompt_and_keeps_it() {
        let mut app = App::new(Config::default());
        app.sessions.push(dirty_scratch_session(0, "a"));
        app.sessions.push(Session::new_with_pages(
            SessionId(1),
            "b".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        ));
        app.active = 0;

        app.close_active();

        assert_eq!(
            app.sessions.len(),
            2,
            "a session with a dirty page must not be removed before the prompt is confirmed"
        );
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::Discard { .. })),
            "closing a tab with a dirty page must raise the discard prompt"
        );
    }

    #[test]
    fn request_quit_with_a_dirty_page_in_a_non_active_session_still_prompts() {
        let mut app = App::new(Config::default());
        app.sessions.push(Session::new_with_pages(
            SessionId(0),
            "a".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        ));
        app.sessions.push(dirty_scratch_session(1, "b"));
        app.active = 0; // the active session ("a") is clean; "b" is dirty.

        app.request_quit();

        assert!(
            !app.should_quit(),
            "a dirty page in ANY session, not just the active one, must block an immediate quit"
        );
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Discard { .. })
        ));
    }

    #[test]
    fn request_quit_with_everything_clean_sets_should_quit_immediately() {
        let mut app = App::new(Config::default());
        app.sessions.push(Session::new_with_pages(
            SessionId(0),
            "a".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        ));

        app.request_quit();

        assert!(app.should_quit());
        assert!(app.pages_prompt.is_none());
    }

    #[test]
    fn request_quit_with_no_sessions_at_all_sets_should_quit_immediately() {
        let mut app = App::new(Config::default());
        app.request_quit();
        assert!(app.should_quit());
    }

    // --- Phase 3: page-tab AppCommand dispatch never sends a WorkerRequest ---

    #[test]
    fn page_tab_key_commands_never_dispatch_a_worker_request() {
        let (mut session, mut req_rx, _cancel_rx) =
            session::test_ready_session(SessionId(0), "conn".to_string(), None);
        session.on_key(key(KeyCode::Tab)); // Focus::Tree -> Focus::Editor.

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        // Ctrl+G new page, Ctrl+N/Ctrl+P switch, Alt+W close, Ctrl+S save
        // (scratch -> prompt, no dispatch), Esc dismiss that prompt.
        app.on_key(ctrl_key(KeyCode::Char('g')));
        app.on_key(ctrl_key(KeyCode::Char('n')));
        app.on_key(ctrl_key(KeyCode::Char('p')));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
        app.on_key(ctrl_key(KeyCode::Char('s')));
        app.on_key(key(KeyCode::Esc));

        assert!(
            req_rx.try_recv().is_err(),
            "no page-tab key command must ever dispatch a WorkerRequest"
        );
    }

    // --- Code-review fix pass: Discard prompt's "save" choice, ReloadPage, delete ---

    /// A session whose `PageTabs` is backed by a real (tempdir) `PagesDir`,
    /// unlike `session::test_ready_session`'s `PageTabs::detached()` -- these
    /// tests need `save_active`/`save_active_as` to actually succeed, not
    /// fail with `PagesError::Path(ConfigError::NoDataDir)` the way a
    /// detached `PageTabs` always would.
    fn session_with_dir_pages(id: u64, conn_name: &str, dir_root: std::path::PathBuf) -> Session {
        let dir = crate::pages::PagesDir::at(dir_root.clone());
        let sidecar_path = dir_root.join("missing.tabs.toml");
        let pages = crate::pages::PageTabs::restore_in(dir, sidecar_path);
        Session::new_with_pages(SessionId(id), conn_name.to_string(), None, pages)
    }

    #[test]
    fn discard_prompt_save_choice_on_a_dirty_named_page_saves_it_and_runs_the_pending_action() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "one").unwrap();
        let mut session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());
        session.pages_mut().open(&name).unwrap();
        *session.pages_mut().editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("one edited");
        assert!(session.pages().active().is_dirty(), "test setup");

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        app.close_active_page();
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::Discard { .. })),
            "test setup: closing a dirty page must raise the discard prompt"
        );

        app.on_key(key(KeyCode::Char('s')));

        assert!(
            app.pages_prompt.is_none(),
            "a named page needs no further prompt to save"
        );
        assert_eq!(
            crate::pages::PagesDir::at(tmp.path().to_path_buf())
                .load(&name)
                .unwrap(),
            "one edited",
            "the save choice must persist the edit to disk"
        );
        assert_eq!(
            app.sessions[0].pages().tabs().len(),
            1,
            "the pending ClosePage action must have run once the save completed"
        );
        assert!(
            app.sessions[0].pages().active().name().is_none(),
            "the page must actually have been closed (leaving a fresh scratch page), proving \
             `then` ran"
        );
    }

    #[test]
    fn discard_prompt_save_choice_on_a_dirty_scratch_page_transitions_to_save_as_then_runs_the_pending_action()
     {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());
        session
            .pages_mut()
            .editor_mut()
            .buffer_mut()
            .insert_str("SELECT 1;");
        assert!(session.pages().active().is_dirty(), "test setup");

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        app.close_active_page();
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Discard { .. })
        ));

        app.on_key(key(KeyCode::Char('s')));
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::SaveAs { .. })),
            "a scratch page has no name yet, so the save choice must transition to save-as"
        );

        for c in "new".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));

        assert!(app.pages_prompt.is_none());
        let saved = crate::pages::PagesDir::at(tmp.path().to_path_buf())
            .load(&crate::pages::PageName::new("new.sql").unwrap())
            .unwrap();
        assert_eq!(saved, "SELECT 1;");
        assert_eq!(
            app.sessions[0].pages().tabs().len(),
            1,
            "the pending ClosePage action must have run once the save-as completed"
        );
        assert!(app.sessions[0].pages().active().name().is_none());
    }

    #[test]
    fn discard_prompt_save_choice_across_two_sessions_saves_both_and_quit_proceeds() {
        let tmp_a = tempfile::tempdir().unwrap();
        let dir_a = crate::pages::PagesDir::at(tmp_a.path().to_path_buf());
        let name_a = crate::pages::PageName::new("a.sql").unwrap();
        dir_a.save(&name_a, "one").unwrap();
        let mut session_a = session_with_dir_pages(0, "a", tmp_a.path().to_path_buf());
        session_a.pages_mut().open(&name_a).unwrap();
        *session_a.pages_mut().editor_mut().buffer_mut() =
            crate::editor::TextBuffer::from_text("one edited");

        let tmp_b = tempfile::tempdir().unwrap();
        let mut session_b = session_with_dir_pages(1, "b", tmp_b.path().to_path_buf());
        session_b
            .pages_mut()
            .editor_mut()
            .buffer_mut()
            .insert_str("SELECT 2;");

        let mut app = App::new(Config::default());
        app.sessions.push(session_a);
        app.sessions.push(session_b);
        app.active = 0;

        app.request_quit();
        assert!(!app.should_quit());
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Discard { .. })
        ));

        app.on_key(key(KeyCode::Char('s')));

        // Session A's named page is saved immediately with no further prompt;
        // session B's unnamed scratch page needs a name, so a save-as prompt
        // must now be showing, focused on session B.
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::SaveAs { .. })),
            "session b's unnamed scratch page must raise a save-as prompt"
        );
        assert_eq!(
            app.active, 1,
            "the save-as prompt must be raised against session b, not whatever was active before"
        );
        assert_eq!(
            crate::pages::PagesDir::at(tmp_a.path().to_path_buf())
                .load(&name_a)
                .unwrap(),
            "one edited",
            "session a's named page must already be saved before session b's prompt is shown"
        );

        for c in "new".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));

        assert!(
            app.should_quit(),
            "quit must proceed once every dirty page across every session is saved"
        );
        let saved = crate::pages::PagesDir::at(tmp_b.path().to_path_buf())
            .load(&crate::pages::PageName::new("new.sql").unwrap())
            .unwrap();
        assert_eq!(saved, "SELECT 2;");
    }

    #[test]
    fn cancelling_a_save_as_raised_mid_quit_flow_restores_the_originally_active_session() {
        // Regression test for the review finding that a cancelled save-as
        // during a save-then-quit/close flow left `self.active` wherever
        // `continue_save_flow` had moved it (to the session owning the
        // scratch page that needed a name) instead of restoring the tab
        // that was actually focused when the flow began.
        let session_a = Session::new_with_pages(
            SessionId(0),
            "a".to_string(),
            None,
            crate::pages::PageTabs::detached(),
        );
        let session_b = dirty_scratch_session(1, "b");

        let mut app = App::new(Config::default());
        app.sessions.push(session_a);
        app.sessions.push(session_b);
        app.active = 0; // session "a" is active; "b" (index 1) has the dirty scratch page.

        app.request_quit();
        assert!(!app.should_quit());
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Discard { .. })
        ));

        app.on_key(key(KeyCode::Char('s')));
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::SaveAs { .. })),
            "test setup: session b's unnamed scratch page must raise a save-as prompt"
        );
        assert_eq!(
            app.active, 1,
            "test setup: the flow must have moved focus to session b for the save-as prompt"
        );

        app.on_key(key(KeyCode::Esc));

        assert!(
            app.pages_prompt.is_none(),
            "cancelling the save-as must close the prompt"
        );
        assert!(
            !app.should_quit(),
            "cancelling the save-as must abandon the pending quit"
        );
        assert_eq!(
            app.active, 0,
            "cancelling the save-as must restore focus to session a, not leave it on session b"
        );
    }

    #[test]
    fn reload_page_key_reloads_the_active_page_from_disk_regardless_of_focus() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "original").unwrap();
        let mut session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());
        session.pages_mut().open(&name).unwrap();
        session
            .pages_mut()
            .editor_mut()
            .buffer_mut()
            .insert_str("garbage");
        assert!(session.pages().active().is_dirty(), "test setup");

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        app.on_key(key(KeyCode::F(5)));

        assert_eq!(
            app.sessions[0].pages().active().editor().buffer().text(),
            "original"
        );
        assert!(!app.sessions[0].pages().active().is_dirty());
    }

    #[test]
    fn delete_key_in_the_open_page_list_raises_a_confirm_prompt_that_deletes_on_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "SELECT 1;").unwrap();
        let session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        app.on_key(ctrl_key(KeyCode::Char('o')));
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Open { .. })
        ));

        app.on_key(key(KeyCode::Char('d')));
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::Discard { .. })),
            "d must raise a delete-confirm prompt reusing the Discard-style confirm plumbing"
        );

        app.on_key(key(KeyCode::Enter));

        assert!(app.pages_prompt.is_none());
        assert!(
            !crate::pages::PagesDir::at(tmp.path().to_path_buf()).exists(&name),
            "confirming delete must remove the file"
        );
    }

    #[test]
    fn s_on_a_delete_confirm_prompt_does_not_delete_the_page_and_leaves_the_prompt_open() {
        // Regression test for the second-round review finding: `Discard`
        // is reused for both "discard unsaved edits" (where `s` correctly
        // means "save instead") and "confirm this delete" (where `s` must
        // be unbound, since there's nothing to save). Before the fix, `s`
        // fell through `continue_save_flow`'s empty `dirty_pages_for`
        // scope for `DeletePage` straight into `run_pending_action`,
        // silently deleting the page.
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "SELECT 1;").unwrap();
        let session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.pages_prompt = Some(PagesPromptState::confirm_delete(name.clone()));

        app.on_key(key(KeyCode::Char('s')));

        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::Discard { .. })),
            "s must not be a bound key on a delete-confirm prompt; the prompt must stay open"
        );
        assert!(
            crate::pages::PagesDir::at(tmp.path().to_path_buf()).exists(&name),
            "s on a delete-confirm prompt must not delete the file"
        );
    }

    #[test]
    fn n_and_esc_on_a_delete_confirm_prompt_cancel_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "SELECT 1;").unwrap();
        let session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.pages_prompt = Some(PagesPromptState::confirm_delete(name.clone()));

        app.on_key(key(KeyCode::Char('n')));

        assert!(
            app.pages_prompt.is_none(),
            "n must dismiss the delete-confirm prompt"
        );
        assert!(
            crate::pages::PagesDir::at(tmp.path().to_path_buf()).exists(&name),
            "n on a delete-confirm prompt must not delete the file"
        );

        app.pages_prompt = Some(PagesPromptState::confirm_delete(name.clone()));
        app.on_key(key(KeyCode::Esc));

        assert!(app.pages_prompt.is_none(), "Esc must dismiss the prompt");
        assert!(
            crate::pages::PagesDir::at(tmp.path().to_path_buf()).exists(&name),
            "Esc on a delete-confirm prompt must not delete the file"
        );
    }

    #[test]
    fn y_on_a_delete_confirm_prompt_deletes_the_page() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let name = crate::pages::PageName::new("a.sql").unwrap();
        dir.save(&name, "SELECT 1;").unwrap();
        let session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;
        app.pages_prompt = Some(PagesPromptState::confirm_delete(name.clone()));

        app.on_key(key(KeyCode::Char('y')));

        assert!(app.pages_prompt.is_none());
        assert!(
            !crate::pages::PagesDir::at(tmp.path().to_path_buf()).exists(&name),
            "y on a delete-confirm prompt must delete the file"
        );
    }

    #[test]
    fn cancelling_a_discard_prompt_after_a_save_error_clears_the_stale_restore_index() {
        // Regression test for the third-round review finding: `continue_save_flow`'s
        // `Err` branch re-raises `Discard` with `save_flow_restore` already set from
        // `save_discard_prompt`, but cancelling that re-raised prompt used to leave
        // `save_flow_restore` set. A later, unrelated `SaveAs` cancellation would then
        // restore `self.active` to that stale session index instead of leaving it alone.
        let tmp_a = tempfile::tempdir().unwrap();
        let dir_a = crate::pages::PagesDir::at(tmp_a.path().to_path_buf());
        let existing = crate::pages::PageName::new("existing.sql").unwrap();
        dir_a.save(&existing, "SELECT 1;").unwrap();
        let mut session_a = session_with_dir_pages(0, "a", tmp_a.path().to_path_buf());
        session_a.pages_mut().open(&existing).unwrap();
        session_a
            .pages_mut()
            .editor_mut()
            .buffer_mut()
            .insert_str("dirty");
        assert!(session_a.pages().active().is_dirty(), "test setup");

        let tmp_b = tempfile::tempdir().unwrap();
        let session_b = session_with_dir_pages(1, "b", tmp_b.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session_a);
        app.sessions.push(session_b);
        app.active = 0;

        // Force the save on session a's dirty page to fail: `PagesDir::save`
        // recreates its root via `create_dir_private` on every call, so merely
        // removing the directory isn't enough -- replace it with a plain file
        // at the same path, which `create_dir_all` cannot turn into a directory.
        std::fs::remove_dir_all(tmp_a.path()).unwrap();
        std::fs::write(tmp_a.path(), b"not a directory").unwrap();

        app.on_key(ctrl_key(KeyCode::Char('w'))); // close active tab (session a) -> dirty
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::Discard { .. })
        ));
        app.on_key(key(KeyCode::Char('s'))); // choose save -> save fails, re-raises Discard
        assert!(
            matches!(app.pages_prompt, Some(PagesPromptState::Discard { .. })),
            "a save error must re-raise the discard prompt, not close it"
        );
        app.on_key(key(KeyCode::Esc)); // cancel out of the failed save entirely
        assert!(app.pages_prompt.is_none());
        assert_eq!(
            app.sessions.len(),
            2,
            "cancelling must not have removed the session"
        );

        // Now drive an unrelated save-as-cancel flow on session b (now the only
        // reachable session if the stale index pointed elsewhere) and confirm
        // `self.active` isn't silently yanked by the leftover restore index.
        app.active = 1;
        app.on_key(ctrl_key(KeyCode::Char('s'))); // scratch page -> save-as prompt
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::SaveAs { .. })
        ));
        app.on_key(key(KeyCode::Esc));

        assert!(app.pages_prompt.is_none());
        assert_eq!(
            app.active, 1,
            "a stale save_flow_restore from the earlier cancelled flow must not \
             hijack this unrelated save-as cancellation"
        );
    }

    #[test]
    fn save_as_prompt_surfaces_already_exists_and_does_not_close_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::pages::PagesDir::at(tmp.path().to_path_buf());
        let taken = crate::pages::PageName::new("taken.sql").unwrap();
        dir.save(&taken, "existing").unwrap();
        let session = session_with_dir_pages(0, "conn", tmp.path().to_path_buf());

        let mut app = App::new(Config::default());
        app.sessions.push(session);
        app.active = 0;

        app.on_key(ctrl_key(KeyCode::Char('s'))); // scratch page -> save-as prompt
        assert!(matches!(
            app.pages_prompt,
            Some(PagesPromptState::SaveAs { .. })
        ));

        for c in "taken".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));

        match &app.pages_prompt {
            Some(PagesPromptState::SaveAs { error: Some(e), .. }) => {
                assert!(e.contains("already exists"), "got {e:?}");
            }
            _ => panic!("expected the save-as prompt to stay open with an already-exists error"),
        }
        assert_eq!(
            crate::pages::PagesDir::at(tmp.path().to_path_buf())
                .load(&taken)
                .unwrap(),
            "existing",
            "the existing file must be untouched by the refused save-as"
        );
    }
}
