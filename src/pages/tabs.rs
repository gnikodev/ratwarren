use std::collections::HashSet;
use std::path::PathBuf;

use crate::config::ConfigError;
use crate::editor::{Motion, Position, TextBuffer};
use crate::ui::editor::EditorState;

use super::sidecar::{self, Sidecar};
use super::{PageName, PagesDir, PagesError};

/// The error every `PageTabs` operation reports for `self.dir: None` --
/// there was never a `name` looked up on disk to be "not found", the real
/// problem is that the data directory itself couldn't be resolved (see
/// `PageTabs::restore`'s degradation to `detached()`). Reuses the existing
/// `PagesError::Path(#[from] ConfigError)` transparent variant rather than
/// adding a new one.
fn no_data_dir() -> PagesError {
    PagesError::Path(ConfigError::NoDataDir)
}

pub struct Page {
    name: Option<PageName>,
    editor: EditorState,
    // Contents as of the last load/save; `None` for a never-saved scratch
    // page, which is dirty whenever the buffer is non-empty instead.
    saved_text: Option<String>,
}

impl Page {
    pub fn scratch() -> Page {
        Page {
            name: None,
            editor: EditorState::new(),
            saved_text: None,
        }
    }

    pub fn name(&self) -> Option<&PageName> {
        self.name.as_ref()
    }

    pub fn title(&self) -> &str {
        match &self.name {
            Some(name) => name.stem(),
            None => "scratch",
        }
    }

    pub fn editor(&self) -> &EditorState {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut EditorState {
        &mut self.editor
    }

    pub fn is_dirty(&self) -> bool {
        match &self.saved_text {
            Some(saved) => self.editor.buffer().text() != *saved,
            None => !self.editor.buffer().is_empty(),
        }
    }
}

pub enum SaveOutcome {
    Saved,
    NeedsName,
}

/// Several pages open at once for one session, with a page tab strip. The
/// active page's buffer is what the session's editor pane renders and edits.
///
/// Invariants: `pages` is never empty; `active < pages.len()`.
///
/// Collision guarding is scoped to a single `PageTabs`: `open`/`save_active_as`
/// refuse to let two tabs in the same `PageTabs` bind to the same on-disk
/// name. Two *sessions* open on the same connection each get their own
/// `PageTabs` restored independently from the same `PagesDir`/sidecar path --
/// that cross-session case is a known, accepted gap (last-writer-wins on both
/// the sidecar and file content), not something this type attempts to guard.
/// Building cross-session file locking is out of scope; see the MVP1 Phase 3
/// review notes this comment mirrors.
pub struct PageTabs {
    // `None` if the data dir couldn't be resolved -- editing still works,
    // saving reports the error rather than panicking or silently no-op'ing.
    dir: Option<PagesDir>,
    pages: Vec<Page>,
    active: usize,
    sidecar_path: Option<PathBuf>,
}

impl PageTabs {
    /// No filesystem access, one scratch page -- the test/degradation seam
    /// every failure path below falls back to.
    pub fn detached() -> PageTabs {
        PageTabs {
            dir: None,
            pages: vec![Page::scratch()],
            active: 0,
            sidecar_path: None,
        }
    }

    /// Never fails: an unresolvable data dir, a missing/corrupt/truncated
    /// sidecar, or every restorable page failing to load all degrade to
    /// `detached()`-shaped state (one fresh scratch page) rather than
    /// blocking a session from opening.
    ///
    /// Runs synchronously inline rather than via `spawn_blocking`: this is
    /// bounded local file IO under the session's own data directory, not the
    /// Phase 2 tunnel wrong-target risk class. If the data dir ever moves to
    /// network storage, this should move to `spawn_blocking` + the existing
    /// `OpenEvent` channel.
    pub fn restore(connection_name: &str) -> PageTabs {
        let dir = match PagesDir::for_connection(connection_name) {
            Ok(dir) => dir,
            Err(_) => return PageTabs::detached(),
        };
        let sidecar_path = match crate::config::paths::state_dir() {
            Ok(state_dir) => state_dir.join(format!(
                "{}{}",
                crate::config::paths::sanitize_connection_name(connection_name),
                sidecar::SIDECAR_SUFFIX
            )),
            Err(_) => return PageTabs::detached(),
        };
        PageTabs::restore_in(dir, sidecar_path)
    }

    /// Same degradation logic as `restore`, with explicit paths -- the test
    /// seam that avoids touching the real per-platform data dir.
    pub fn restore_in(dir: PagesDir, sidecar_path: PathBuf) -> PageTabs {
        let raw = sidecar::load(&sidecar_path);
        let sidecar = if raw.version == sidecar::SIDECAR_VERSION {
            raw
        } else {
            Sidecar::empty()
        };

        let mut pages = Vec::new();
        let mut seen = HashSet::new();
        let mut surviving_active = None;

        for (i, entry) in sidecar.open.iter().enumerate() {
            if pages.len() >= sidecar::MAX_RESTORED_PAGES {
                break;
            }
            let Ok(name) = PageName::new(&entry.file) else {
                continue;
            };
            if !seen.insert(name.as_str().to_string()) {
                continue;
            }
            let Ok(text) = dir.load(&name) else {
                continue;
            };

            let mut editor = EditorState::new();
            *editor.buffer_mut() = TextBuffer::from_text(&text);
            editor.buffer_mut().move_to(
                Position {
                    line: entry.line,
                    col: entry.col,
                },
                Motion::Move,
            );

            if i == sidecar.active {
                surviving_active = Some(pages.len());
            }
            pages.push(Page {
                name: Some(name),
                editor,
                saved_text: Some(text),
            });
        }

        if pages.is_empty() {
            pages.push(Page::scratch());
        }
        let active = surviving_active.unwrap_or(0);

        PageTabs {
            dir: Some(dir),
            pages,
            active,
            sidecar_path: Some(sidecar_path),
        }
    }

    pub fn active(&self) -> &Page {
        &self.pages[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Page {
        &mut self.pages[self.active]
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn tabs(&self) -> &[Page] {
        &self.pages
    }

    pub fn editor(&self) -> &EditorState {
        self.active().editor()
    }

    pub fn editor_mut(&mut self) -> &mut EditorState {
        self.active_mut().editor_mut()
    }

    /// Focuses `name` if it's already open; otherwise loads it from disk and
    /// pushes a new tab, focused.
    pub fn open(&mut self, name: &PageName) -> Result<(), PagesError> {
        if let Some(idx) = self.pages.iter().position(|p| p.name() == Some(name)) {
            self.active = idx;
            return Ok(());
        }
        let dir = self.dir.as_ref().ok_or_else(no_data_dir)?;
        let text = dir.load(name)?;
        let mut editor = EditorState::new();
        *editor.buffer_mut() = TextBuffer::from_text(&text);
        self.pages.push(Page {
            name: Some(name.clone()),
            editor,
            saved_text: Some(text),
        });
        self.active = self.pages.len() - 1;
        Ok(())
    }

    pub fn new_scratch(&mut self) {
        self.pages.push(Page::scratch());
        self.active = self.pages.len() - 1;
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % self.pages.len();
    }

    pub fn prev(&mut self) {
        self.active = (self.active + self.pages.len() - 1) % self.pages.len();
    }

    pub fn select(&mut self, index: usize) {
        if index < self.pages.len() {
            self.active = index;
        }
    }

    pub fn save_active(&mut self) -> Result<SaveOutcome, PagesError> {
        let name = match self.active().name() {
            Some(name) => name.clone(),
            None => return Ok(SaveOutcome::NeedsName),
        };
        self.write_active(&name)?;
        Ok(SaveOutcome::Saved)
    }

    pub fn save_active_as(&mut self, name: &PageName) -> Result<(), PagesError> {
        // Re-saving a page to the name it already has is an ordinary save,
        // not a clobber -- everything else must refuse a name that's already
        // taken, on disk or by another open tab (see `name_is_taken`).
        if self.active().name() != Some(name) && self.name_is_taken(name) {
            return Err(PagesError::AlreadyExists(name.as_str().to_string()));
        }
        self.write_active(name)?;
        self.active_mut().name = Some(name.clone());
        Ok(())
    }

    /// `true` if `name` is already spoken for: either an existing on-disk
    /// file (guards `save_active_as` against silently clobbering it, the
    /// same rationale as `PagesDir::rename`'s own guard) or another
    /// currently-open tab in this `PageTabs` (guards against orphaning that
    /// tab's in-memory edits -- relevant even before it's ever been saved
    /// under that name, though in practice every named `Page` already has a
    /// backing file per this type's invariants).
    fn name_is_taken(&self, name: &PageName) -> bool {
        let on_disk = self.dir.as_ref().is_some_and(|dir| dir.exists(name));
        let open_in_another_tab = self
            .pages
            .iter()
            .enumerate()
            .any(|(i, page)| i != self.active && page.name() == Some(name));
        on_disk || open_in_another_tab
    }

    fn write_active(&mut self, name: &PageName) -> Result<(), PagesError> {
        let dir = self.dir.as_ref().ok_or_else(no_data_dir)?;
        let text = self.active().editor().buffer().text();
        dir.save(name, &text)?;
        self.active_mut().saved_text = Some(text);
        Ok(())
    }

    pub fn rename_active(&mut self, to: &PageName) -> Result<(), PagesError> {
        let dir = self.dir.as_ref().ok_or_else(no_data_dir)?;
        let Some(from) = self.active().name().cloned() else {
            // An unnamed (scratch) page has nothing on disk to rename --
            // renaming it is the same as saving it under a new name.
            return self.save_active_as(to);
        };
        dir.rename(&from, to)?;
        self.active_mut().name = Some(to.clone());
        Ok(())
    }

    pub fn delete(&mut self, name: &PageName) -> Result<(), PagesError> {
        let dir = self.dir.as_ref().ok_or_else(no_data_dir)?;
        dir.delete(name)?;
        if let Some(idx) = self.pages.iter().position(|p| p.name() == Some(name)) {
            self.close_at(idx);
        }
        Ok(())
    }

    /// Discards in-memory edits and re-reads the active page from disk. A
    /// no-op (returns `Ok`) for an unnamed scratch page, since there is
    /// nothing on disk to reload from.
    pub fn reload_active(&mut self) -> Result<(), PagesError> {
        let Some(name) = self.active().name().cloned() else {
            return Ok(());
        };
        let dir = self.dir.as_ref().ok_or_else(no_data_dir)?;
        let text = dir.load(&name)?;
        let page = self.active_mut();
        *page.editor.buffer_mut() = TextBuffer::from_text(&text);
        page.saved_text = Some(text);
        Ok(())
    }

    /// `Ok(true)` = closed. `Ok(false)` = the page is dirty and `force` was
    /// `false`; the caller must prompt before retrying with `force: true`.
    /// Closing the last page leaves a fresh scratch page in its place --
    /// `pages` is never empty.
    pub fn close_active(&mut self, force: bool) -> Result<bool, PagesError> {
        if !force && self.active().is_dirty() {
            return Ok(false);
        }
        self.close_at(self.active);
        Ok(true)
    }

    fn close_at(&mut self, index: usize) {
        self.pages.remove(index);
        if self.pages.is_empty() {
            self.pages.push(Page::scratch());
            self.active = 0;
            return;
        }
        if self.active >= self.pages.len() {
            self.active = self.pages.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
    }

    pub fn any_dirty(&self) -> bool {
        self.pages.iter().any(Page::is_dirty)
    }

    pub fn dirty_titles(&self) -> Vec<String> {
        self.pages
            .iter()
            .filter(|p| p.is_dirty())
            .map(|p| p.title().to_string())
            .collect()
    }

    pub fn list_available(&self) -> Result<Vec<PageName>, PagesError> {
        match &self.dir {
            Some(dir) => dir.list(),
            None => Ok(Vec::new()),
        }
    }

    /// Best-effort: never surfaces an error, never blocks a quit.
    pub fn persist_sidecar(&self) {
        let Some(sidecar_path) = &self.sidecar_path else {
            return;
        };
        let open = self
            .pages
            .iter()
            .filter_map(|p| {
                let name = p.name()?;
                let cursor = p.editor().buffer().cursor();
                Some(sidecar::SidecarPage {
                    file: name.as_str().to_string(),
                    line: cursor.line,
                    col: cursor.col,
                })
            })
            .collect::<Vec<_>>();
        // `active` in the stored sidecar indexes into `sidecar.open` (named
        // pages only), not into `self.pages` (which may include an unnamed
        // scratch tab) -- recompute it against the filtered list.
        let active = self
            .pages
            .iter()
            .take(self.active)
            .filter(|p| p.name().is_some())
            .count();

        let sidecar = Sidecar {
            version: sidecar::SIDECAR_VERSION,
            active,
            open,
        };
        sidecar::store(sidecar_path, &sidecar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Position;
    use std::fs;

    fn temp_pages_dir() -> (tempfile::TempDir, PagesDir) {
        let tmp = tempfile::tempdir().expect("tempdir creation");
        let dir = PagesDir::at(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn name(s: &str) -> PageName {
        PageName::new(s).unwrap()
    }

    #[test]
    fn detached_has_exactly_one_non_dirty_scratch_page() {
        let tabs = PageTabs::detached();
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.active_index(), 0);
        assert!(tabs.active().name().is_none());
        assert!(!tabs.active().is_dirty());
    }

    #[test]
    fn restore_in_with_no_sidecar_file_is_one_non_dirty_scratch_page() {
        let (tmp, dir) = temp_pages_dir();
        let sidecar_path = tmp.path().join("missing.tabs.toml");

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.active_index(), 0);
        assert!(tabs.active().name().is_none());
        assert!(!tabs.active().is_dirty());
    }

    // --- restore_in degradation ---

    fn write_sidecar_toml(path: &std::path::Path, toml_text: &str) {
        fs::write(path, toml_text).unwrap();
    }

    #[test]
    fn restore_in_drops_an_entry_naming_a_nonexistent_file_but_restores_siblings() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("exists.sql"), "SELECT 1;").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        write_sidecar_toml(
            &sidecar_path,
            r#"
                version = 1
                active = 0
                [[open]]
                file = "missing.sql"
                [[open]]
                file = "exists.sql"
            "#,
        );

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.tabs()[0].name(), Some(&name("exists.sql")));
    }

    #[test]
    fn restore_in_drops_an_entry_with_an_invalid_page_name() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("exists.sql"), "SELECT 1;").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        write_sidecar_toml(
            &sidecar_path,
            r#"
                version = 1
                active = 0
                [[open]]
                file = "../evil.sql"
                [[open]]
                file = "exists.sql"
            "#,
        );

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.tabs()[0].name(), Some(&name("exists.sql")));
    }

    #[test]
    fn restore_in_collapses_duplicate_file_entries_to_one_page_keeping_the_first() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        write_sidecar_toml(
            &sidecar_path,
            r#"
                version = 1
                active = 0
                [[open]]
                file = "a.sql"
                line = 0
                col = 1
                [[open]]
                file = "a.sql"
                line = 0
                col = 5
            "#,
        );

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(
            tabs.tabs()[0].editor().buffer().cursor(),
            Position { line: 0, col: 1 },
            "the first of the duplicate entries must win"
        );
    }

    #[test]
    fn restore_in_caps_the_number_of_restored_pages_at_max_restored_pages() {
        let (tmp, dir) = temp_pages_dir();
        let count = sidecar::MAX_RESTORED_PAGES + 5;
        let mut toml_text = String::from("version = 1\nactive = 0\n");
        for i in 0..count {
            let file = format!("p{i}.sql");
            dir.save(&name(&file), "SELECT 1;").unwrap();
            toml_text.push_str(&format!("[[open]]\nfile = \"{file}\"\n"));
        }
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        write_sidecar_toml(&sidecar_path, &toml_text);

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), sidecar::MAX_RESTORED_PAGES);
    }

    #[test]
    fn restore_in_falls_back_active_to_0_when_the_active_entry_was_dropped() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        dir.save(&name("b.sql"), "SELECT 2;").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        // active = 0 names an entry that fails PageName::new, so it never
        // becomes a page -- the survivors are "a.sql" (index 1 in the
        // sidecar) and "b.sql" (index 2), neither of which is index 0.
        write_sidecar_toml(
            &sidecar_path,
            r#"
                version = 1
                active = 0
                [[open]]
                file = "../evil.sql"
                [[open]]
                file = "a.sql"
                [[open]]
                file = "b.sql"
            "#,
        );

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn restore_in_with_an_unrecognised_sidecar_version_degrades_to_one_scratch_page() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        write_sidecar_toml(
            &sidecar_path,
            r#"
                version = 999
                active = 0
                [[open]]
                file = "a.sql"
            "#,
        );

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert!(tabs.active().name().is_none());
    }

    #[test]
    fn restore_in_of_a_corrupt_sidecar_degrades_to_one_scratch_page() {
        let (tmp, dir) = temp_pages_dir();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");
        fs::write(&sidecar_path, b"not valid toml {{{").unwrap();

        let tabs = PageTabs::restore_in(dir, sidecar_path);

        assert_eq!(tabs.tabs().len(), 1);
        assert!(tabs.active().name().is_none());
        assert!(!tabs.active().is_dirty());
    }

    #[test]
    fn persist_sidecar_then_restore_in_preserves_order_active_and_cursor() {
        let (tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "line1\nline2\nline3\n").unwrap();
        dir.save(&name("b.sql"), "select 1;\n").unwrap();
        let sidecar_path = tmp.path().join("sidecar.tabs.toml");

        let mut tabs =
            PageTabs::restore_in(PagesDir::at(dir.root().to_path_buf()), sidecar_path.clone());
        // `restore_in` with no sidecar file starts with one unnamed scratch
        // page at index 0; close it once both named pages are open so the
        // remaining indices line up with `[a.sql, b.sql]`.
        tabs.open(&name("a.sql")).unwrap();
        tabs.open(&name("b.sql")).unwrap();
        tabs.select(0);
        assert!(
            tabs.active().name().is_none(),
            "test setup: index 0 is the leftover scratch page"
        );
        tabs.close_active(true)
            .expect("closing the clean scratch page must succeed");
        assert_eq!(
            tabs.tabs()
                .iter()
                .map(|p| p.name().cloned())
                .collect::<Vec<_>>(),
            vec![Some(name("a.sql")), Some(name("b.sql"))],
            "test setup: only the two named pages should remain"
        );

        // Focus "a.sql" and move its cursor somewhere identifiable.
        tabs.select(0);
        tabs.editor_mut()
            .buffer_mut()
            .move_to(Position { line: 1, col: 2 }, crate::editor::Motion::Move);
        tabs.select(1); // "b.sql" ends up active.

        tabs.persist_sidecar();

        let restored = PageTabs::restore_in(PagesDir::at(dir.root().to_path_buf()), sidecar_path);
        assert_eq!(
            restored
                .tabs()
                .iter()
                .map(|p| p.name().cloned())
                .collect::<Vec<_>>(),
            vec![Some(name("a.sql")), Some(name("b.sql"))]
        );
        assert_eq!(
            restored.active_index(),
            1,
            "b.sql was active when persisted"
        );
        assert_eq!(
            restored.tabs()[0].editor().buffer().cursor(),
            Position { line: 1, col: 2 }
        );
    }

    // --- Page-tab lifecycle ---

    #[test]
    fn opening_an_already_open_page_focuses_it_instead_of_duplicating() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        dir.save(&name("b.sql"), "SELECT 2;").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        tabs.open(&name("b.sql")).unwrap();
        assert_eq!(tabs.tabs().len(), 3, "test setup: scratch + a + b");

        tabs.select(0);
        tabs.open(&name("a.sql")).unwrap();

        assert_eq!(
            tabs.tabs().len(),
            3,
            "opening an already-open page must not duplicate it"
        );
        assert_eq!(tabs.active().name(), Some(&name("a.sql")));
    }

    #[test]
    fn close_active_on_the_only_page_leaves_a_fresh_scratch_page() {
        let mut tabs = PageTabs::detached();
        tabs.editor_mut().buffer_mut().insert_str("some text");
        assert!(tabs.active().is_dirty(), "test setup: page must be dirty");

        let closed = tabs.close_active(true).expect("close should succeed");

        assert!(closed);
        assert_eq!(tabs.tabs().len(), 1);
        assert!(tabs.active().name().is_none());
        assert!(
            !tabs.active().is_dirty(),
            "the replacement page must be a fresh scratch page"
        );
    }

    #[test]
    fn close_active_without_force_refuses_on_a_dirty_page_and_removes_nothing() {
        let mut tabs = PageTabs::detached();
        tabs.editor_mut().buffer_mut().insert_str("dirty");

        let result = tabs.close_active(false).expect("close should not error");

        assert!(
            !result,
            "close_active(false) on a dirty page must return Ok(false)"
        );
        assert_eq!(tabs.tabs().len(), 1, "nothing should have been removed");
        assert!(
            tabs.active().is_dirty(),
            "the dirty page must still be there, untouched"
        );
    }

    #[test]
    fn close_active_with_force_removes_a_dirty_page() {
        let mut tabs = PageTabs::detached();
        tabs.editor_mut().buffer_mut().insert_str("dirty");

        let result = tabs.close_active(true).expect("close should not error");

        assert!(result);
    }

    #[test]
    fn a_named_page_edited_back_to_its_saved_text_is_not_dirty() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        assert!(
            !tabs.active().is_dirty(),
            "freshly opened must not be dirty"
        );

        tabs.editor_mut().buffer_mut().insert_str(" more");
        assert!(tabs.active().is_dirty(), "test setup: edit must dirty it");

        tabs.editor_mut().buffer_mut().delete_backward();
        tabs.editor_mut().buffer_mut().delete_backward();
        tabs.editor_mut().buffer_mut().delete_backward();
        tabs.editor_mut().buffer_mut().delete_backward();
        tabs.editor_mut().buffer_mut().delete_backward();

        assert_eq!(tabs.active().editor().buffer().text(), "SELECT 1;");
        assert!(
            !tabs.active().is_dirty(),
            "text edited back to exactly the saved content must not be dirty"
        );
    }

    #[test]
    fn save_active_on_a_scratch_page_needs_a_name_and_writes_nothing() {
        let (tmp, dir) = temp_pages_dir();
        let sidecar_path = tmp.path().join("missing.tabs.toml");
        let mut tabs = PageTabs::restore_in(dir, sidecar_path);
        tabs.editor_mut().buffer_mut().insert_str("SELECT 1;");

        let outcome = tabs.save_active().expect("must not error");

        assert!(matches!(outcome, SaveOutcome::NeedsName));
        assert_eq!(
            tabs.list_available().unwrap(),
            Vec::new(),
            "save_active on a scratch page must not write anything to disk"
        );
    }

    #[test]
    fn rename_active_onto_an_existing_name_fails_and_leaves_both_files_intact() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "one").unwrap();
        dir.save(&name("b.sql"), "two").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();

        let err = tabs
            .rename_active(&name("b.sql"))
            .expect_err("must refuse to clobber b.sql");

        assert!(matches!(err, PagesError::AlreadyExists(n) if n == "b.sql"));
        assert_eq!(tabs.active().name(), Some(&name("a.sql")));
        assert_eq!(
            tabs.list_available().unwrap(),
            vec![name("a.sql"), name("b.sql")],
            "both files must still exist on disk"
        );
    }

    // --- save_active_as clobber/collision guards (code-review findings 1, 2a) ---

    #[test]
    fn save_active_as_onto_an_existing_different_name_is_refused_and_the_original_is_untouched() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "one").unwrap();
        dir.save(&name("b.sql"), "two").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        tabs.editor_mut().buffer_mut().insert_str(" edited");

        let err = tabs
            .save_active_as(&name("b.sql"))
            .expect_err("must refuse to clobber b.sql");

        assert!(matches!(err, PagesError::AlreadyExists(n) if n == "b.sql"));
        assert_eq!(
            tabs.active().name(),
            Some(&name("a.sql")),
            "the active page must keep its original name"
        );
        assert_eq!(
            dir_at(tabs.dir.as_ref().unwrap())
                .load(&name("b.sql"))
                .unwrap(),
            "two",
            "b.sql's on-disk content must be untouched"
        );
    }

    fn dir_at(dir: &PagesDir) -> PagesDir {
        PagesDir::at(dir.root().to_path_buf())
    }

    #[test]
    fn save_active_as_onto_its_own_current_name_succeeds_as_an_ordinary_overwrite() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "one").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        *tabs.editor_mut().buffer_mut() = TextBuffer::from_text("one edited");

        tabs.save_active_as(&name("a.sql"))
            .expect("re-saving onto the page's own current name must succeed");

        assert_eq!(
            dir_at(tabs.dir.as_ref().unwrap())
                .load(&name("a.sql"))
                .unwrap(),
            "one edited"
        );
        assert!(!tabs.active().is_dirty());
    }

    #[test]
    fn opening_the_same_name_twice_in_one_page_tabs_focuses_the_existing_tab() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "SELECT 1;").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        tabs.new_scratch();
        assert_eq!(tabs.tabs().len(), 3, "test setup: scratch + a + scratch");

        tabs.open(&name("a.sql")).unwrap();

        assert_eq!(
            tabs.tabs().len(),
            3,
            "opening an already-open name a second time must not duplicate the tab"
        );
        assert_eq!(tabs.active().name(), Some(&name("a.sql")));
    }

    #[test]
    fn save_as_from_one_tab_onto_a_name_open_with_unsaved_edits_in_another_tab_is_refused() {
        let (_tmp, dir) = temp_pages_dir();
        dir.save(&name("a.sql"), "one").unwrap();
        dir.save(&name("b.sql"), "two").unwrap();
        let mut tabs = PageTabs::restore_in(dir, std::path::PathBuf::from("/nonexistent"));
        tabs.open(&name("a.sql")).unwrap();
        tabs.open(&name("b.sql")).unwrap();
        // Dirty b.sql's in-memory buffer without saving, then switch back to
        // a.sql and try to save-as onto "b.sql" -- must not clobber b's file
        // (which would silently orphan its dirty in-memory edits).
        tabs.editor_mut().buffer_mut().insert_str(" unsaved");
        assert!(tabs.active().is_dirty(), "test setup: b.sql must be dirty");
        tabs.select(1); // a.sql

        let err = tabs
            .save_active_as(&name("b.sql"))
            .expect_err("save-as onto a name open with unsaved edits in another tab must refuse");

        assert!(matches!(err, PagesError::AlreadyExists(n) if n == "b.sql"));
        tabs.select(2); // b.sql
        assert!(
            tabs.active().is_dirty(),
            "b.sql's unsaved edits must be untouched by the refused save-as"
        );
    }

    // --- Code-review fix pass: CRLF round trip through TextBuffer (finding 4) ---

    #[test]
    fn crlf_and_missing_trailing_newline_content_round_trips_exactly_through_text_buffer() {
        let cases = ["a\r\nb\r\n", "a\r\nb", "line1\r\nline2\r\nline3\r\n"];
        for (i, content) in cases.iter().enumerate() {
            let (tmp, dir) = temp_pages_dir();
            let file = format!("case{i}.sql");
            let page_name = name(&file);
            dir.save(&page_name, content).unwrap();

            let mut tabs = PageTabs::restore_in(dir, tmp.path().join(format!("case{i}.tabs.toml")));
            tabs.open(&page_name).unwrap();
            assert_eq!(
                tabs.active().editor().buffer().text(),
                *content,
                "TextBuffer::from_text must preserve content exactly for case {i}"
            );
            assert!(
                !tabs.active().is_dirty(),
                "freshly opened content must not read as dirty for case {i}"
            );

            // A trivial edit-and-undo round trip through TextBuffer, then a
            // real save/reload -- not just PagesDir::save/load directly --
            // so this actually exercises TextBuffer::text(), not just raw
            // file IO.
            tabs.editor_mut().buffer_mut().insert_str("x");
            tabs.editor_mut().buffer_mut().delete_backward();
            assert_eq!(
                tabs.active().editor().buffer().text(),
                *content,
                "insert-then-delete-backward must round-trip back to the original text for case {i}"
            );
            tabs.save_active().expect("save should succeed");

            let reloaded = dir_at(tabs.dir.as_ref().unwrap()).load(&page_name).unwrap();
            assert_eq!(
                &reloaded, content,
                "byte-exact round trip through TextBuffer failed for case {i}"
            );
        }
    }

    // --- Code-review fix pass: NoDataDir, not a fabricated NotFound (finding 5) ---

    #[test]
    fn operations_on_a_detached_page_tabs_report_no_data_dir_not_a_fabricated_not_found() {
        let mut tabs = PageTabs::detached();

        let open_err = tabs
            .open(&name("a.sql"))
            .expect_err("a detached PageTabs has no dir to load from");
        assert!(
            matches!(open_err, PagesError::Path(ConfigError::NoDataDir)),
            "open must report NoDataDir, not fabricate a NotFound for a name never looked up on \
             disk, got {open_err:?}"
        );

        let delete_err = tabs
            .delete(&name("a.sql"))
            .expect_err("a detached PageTabs has no dir to delete from");
        assert!(
            matches!(delete_err, PagesError::Path(ConfigError::NoDataDir)),
            "delete must report NoDataDir, got {delete_err:?}"
        );

        tabs.editor_mut().buffer_mut().insert_str("SELECT 1;");
        let save_as_err = tabs
            .save_active_as(&name("a.sql"))
            .expect_err("a detached PageTabs has no dir to save to");
        assert!(
            matches!(save_as_err, PagesError::Path(ConfigError::NoDataDir)),
            "save_active_as must report NoDataDir, got {save_as_err:?}"
        );
    }
}
