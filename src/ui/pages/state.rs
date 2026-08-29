use crate::pages::PageName;

/// What to do once a `Discard`/`SaveAs` prompt's action actually completes
/// (a confirmed discard, or a successful save). `None` for a prompt that
/// isn't gating some other pending operation (e.g. an explicit `Ctrl+S` on
/// an unnamed page has nothing to do afterward but close the prompt).
pub enum PendingAction {
    None,
    ClosePage,
    CloseTab,
    Quit,
    /// Resumes the Discard prompt's "save" flow after a scratch page's
    /// `SaveAs` prompt (raised mid-flow because that page has no name yet)
    /// resolves: `App::continue_save_flow` recomputes which pages are still
    /// dirty for the wrapped action and keeps saving until none remain,
    /// then finally runs it. Boxed since this wraps one of this same enum's
    /// other variants.
    SaveRemaining(Box<PendingAction>),
    /// Confirmed via the same y/Enter path as a `Discard` prompt (raised
    /// from the `Open` page-list overlay's `d` key) -- deletes the named
    /// page rather than discarding unsaved edits.
    DeletePage(PageName),
}

/// One of MVP1 Phase 3's modal overlays. Deliberately its own field on
/// `App` (`pages_prompt: Option<PagesPromptState>`) rather than folded into
/// `picker: Option<PickerState>` or a general overlay enum -- that
/// consolidation is Phase 8's job.
pub enum PagesPromptState {
    /// Ctrl+O: pick a saved page to open in the active session.
    Open {
        rows: Vec<PageName>,
        selected: usize,
    },
    /// Raised whenever a dirty close (page/tab/quit) needs confirmation, or
    /// (when `then` is `DeletePage`) a page deletion needs confirmation.
    /// `titles` are the dirty pages' display titles across whatever scope
    /// `then` implies (one page for `ClosePage`, a session's pages for
    /// `CloseTab`, every session's pages for `Quit`, empty for
    /// `DeletePage` -- the page's name is carried in `then` itself and
    /// rendered directly). `error` surfaces a failure from a previous
    /// attempt (e.g. the "save" choice failing partway through) without
    /// dropping the prompt.
    Discard {
        titles: Vec<String>,
        then: PendingAction,
        error: Option<String>,
    },
    /// A plain text-input box: append/backspace only, no cursor motion --
    /// this isn't a real text-input widget. Shared by three flows: an
    /// explicit save-as, `Ctrl+S` on a never-saved scratch page, and `F2`
    /// rename (`rename: true` selects `PageTabs::rename_active` over
    /// `save_active_as` at confirm time).
    SaveAs {
        input: String,
        error: Option<String>,
        then: PendingAction,
        rename: bool,
    },
}

impl PagesPromptState {
    pub fn open(rows: Vec<PageName>) -> PagesPromptState {
        PagesPromptState::Open { rows, selected: 0 }
    }

    pub fn discard(titles: Vec<String>, then: PendingAction) -> PagesPromptState {
        PagesPromptState::Discard {
            titles,
            then,
            error: None,
        }
    }

    /// Re-raises a `Discard` prompt carrying a failure message from a
    /// previous attempt (see `App::continue_save_flow`'s `Err` branch)
    /// instead of silently dropping the prompt on error.
    pub fn discard_with_error(
        titles: Vec<String>,
        then: PendingAction,
        error: String,
    ) -> PagesPromptState {
        PagesPromptState::Discard {
            titles,
            then,
            error: Some(error),
        }
    }

    /// Confirmed the same way as a `Discard` prompt (y/Enter) -- raised by
    /// the `Open` page-list overlay's `d` key.
    pub fn confirm_delete(name: PageName) -> PagesPromptState {
        PagesPromptState::Discard {
            titles: Vec::new(),
            then: PendingAction::DeletePage(name),
            error: None,
        }
    }

    pub fn save_as(then: PendingAction) -> PagesPromptState {
        PagesPromptState::SaveAs {
            input: String::new(),
            error: None,
            then,
            rename: false,
        }
    }

    pub fn rename(current_title: &str) -> PagesPromptState {
        PagesPromptState::SaveAs {
            input: current_title.to_string(),
            error: None,
            then: PendingAction::None,
            rename: true,
        }
    }

    pub fn move_up(&mut self) {
        if let PagesPromptState::Open { selected, .. } = self {
            *selected = selected.saturating_sub(1);
        }
    }

    pub fn move_down(&mut self) {
        if let PagesPromptState::Open { rows, selected } = self
            && !rows.is_empty()
        {
            *selected = (*selected + 1).min(rows.len() - 1);
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if let PagesPromptState::SaveAs { input, error, .. } = self {
            input.push(c);
            *error = None;
        }
    }

    pub fn backspace(&mut self) {
        if let PagesPromptState::SaveAs { input, error, .. } = self {
            input.pop();
            *error = None;
        }
    }

    pub fn selected_page(&self) -> Option<&PageName> {
        match self {
            PagesPromptState::Open { rows, selected } => rows.get(*selected),
            PagesPromptState::Discard { .. } | PagesPromptState::SaveAs { .. } => None,
        }
    }
}
