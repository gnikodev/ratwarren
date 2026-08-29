use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Focus;
use crate::editor::Motion;
use crate::ui::editor::EditorCommand;
use crate::ui::grid::state::GridCommand;
use crate::ui::tree::state::TreeCommand;

pub enum AppCommand {
    Quit,
    ToggleFocus,
    FocusTree,
    Activate,
    Tree(TreeCommand),
    Grid(GridCommand),
    Editor(EditorCommand),
    Run(RunKey),
    CancelOrQuit,
    OpenPicker,
    CloseTab,
    NextTab,
    PrevTab,
}

pub enum RunKey {
    CursorOrSelection,
    Buffer,
}

pub fn map_key(key: KeyEvent, focus: Focus) -> Option<AppCommand> {
    match (key.code, key.modifiers) {
        (KeyCode::Tab, _) => return Some(AppCommand::ToggleFocus),
        (KeyCode::Esc, _) if focus == Focus::Grid => return Some(AppCommand::FocusTree),
        // `Ctrl+C` is context-sensitive rather than a hardcoded Quit: it
        // cancels an in-flight run if one exists, and only quits otherwise
        // (see `App::on_key`'s `CancelOrQuit` handling).
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Some(AppCommand::CancelOrQuit),
        // Run is bound globally (not gated to `Focus::Editor`): a natural
        // workflow is "look at the grid, then run again" without first
        // tabbing back to the editor pane.
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
            return Some(AppCommand::Run(RunKey::CursorOrSelection));
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            return Some(AppCommand::Run(RunKey::Buffer));
        }
        // Tab management is bound globally, including in `Focus::Editor`
        // where bare printable characters otherwise insert -- these four
        // must never be shadowed by a session's own keymap.
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => return Some(AppCommand::OpenPicker),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => return Some(AppCommand::CloseTab),
        (KeyCode::Char('n'), KeyModifiers::CONTROL) => return Some(AppCommand::NextTab),
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => return Some(AppCommand::PrevTab),
        _ => {}
    }

    match focus {
        Focus::Tree => match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => Some(AppCommand::Quit),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                Some(AppCommand::Tree(TreeCommand::MoveUp))
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                Some(AppCommand::Tree(TreeCommand::MoveDown))
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                Some(AppCommand::Tree(TreeCommand::Expand))
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                Some(AppCommand::Tree(TreeCommand::Collapse))
            }
            (KeyCode::Enter, _) => Some(AppCommand::Activate),
            (KeyCode::PageUp, _) => Some(AppCommand::Tree(TreeCommand::PageUp)),
            (KeyCode::PageDown, _) => Some(AppCommand::Tree(TreeCommand::PageDown)),
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                Some(AppCommand::Tree(TreeCommand::First))
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                Some(AppCommand::Tree(TreeCommand::Last))
            }
            (KeyCode::Char('r'), _) => Some(AppCommand::Tree(TreeCommand::Refresh)),
            (KeyCode::Char('.'), _) => Some(AppCommand::Tree(TreeCommand::ToggleSystemSchemas)),
            _ => None,
        },
        Focus::Grid => match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => Some(AppCommand::Quit),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                Some(AppCommand::Grid(GridCommand::MoveUp))
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                Some(AppCommand::Grid(GridCommand::MoveDown))
            }
            (KeyCode::PageUp, _) => Some(AppCommand::Grid(GridCommand::PageUp)),
            (KeyCode::PageDown, _) => Some(AppCommand::Grid(GridCommand::PageDown)),
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                Some(AppCommand::Grid(GridCommand::First))
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                Some(AppCommand::Grid(GridCommand::Last))
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                Some(AppCommand::Grid(GridCommand::ScrollLeft))
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                Some(AppCommand::Grid(GridCommand::ScrollRight))
            }
            (KeyCode::Char('n'), _) => Some(AppCommand::Grid(GridCommand::NextPage)),
            (KeyCode::Char('p'), _) => Some(AppCommand::Grid(GridCommand::PrevPage)),
            (KeyCode::Char('r'), _) => Some(AppCommand::Grid(GridCommand::Refresh)),
            _ => None,
        },
        Focus::Editor => match (key.code, key.modifiers) {
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                Some(AppCommand::Editor(EditorCommand::Insert(c)))
            }
            (KeyCode::Enter, _) => Some(AppCommand::Editor(EditorCommand::Newline)),
            (KeyCode::Backspace, _) => Some(AppCommand::Editor(EditorCommand::DeleteBackward)),
            (KeyCode::Delete, _) => Some(AppCommand::Editor(EditorCommand::DeleteForward)),
            (KeyCode::Left, KeyModifiers::SHIFT) => {
                Some(AppCommand::Editor(EditorCommand::Left(Motion::Extend)))
            }
            (KeyCode::Left, _) => Some(AppCommand::Editor(EditorCommand::Left(Motion::Move))),
            (KeyCode::Right, KeyModifiers::SHIFT) => {
                Some(AppCommand::Editor(EditorCommand::Right(Motion::Extend)))
            }
            (KeyCode::Right, _) => Some(AppCommand::Editor(EditorCommand::Right(Motion::Move))),
            (KeyCode::Up, KeyModifiers::SHIFT) => {
                Some(AppCommand::Editor(EditorCommand::Up(Motion::Extend)))
            }
            (KeyCode::Up, _) => Some(AppCommand::Editor(EditorCommand::Up(Motion::Move))),
            (KeyCode::Down, KeyModifiers::SHIFT) => {
                Some(AppCommand::Editor(EditorCommand::Down(Motion::Extend)))
            }
            (KeyCode::Down, _) => Some(AppCommand::Editor(EditorCommand::Down(Motion::Move))),
            (KeyCode::Home, KeyModifiers::CONTROL) => {
                Some(AppCommand::Editor(EditorCommand::BufferStart(Motion::Move)))
            }
            (KeyCode::Home, _) => Some(AppCommand::Editor(EditorCommand::LineStart(Motion::Move))),
            (KeyCode::End, KeyModifiers::CONTROL) => {
                Some(AppCommand::Editor(EditorCommand::BufferEnd(Motion::Move)))
            }
            (KeyCode::End, _) => Some(AppCommand::Editor(EditorCommand::LineEnd(Motion::Move))),
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                Some(AppCommand::Editor(EditorCommand::SelectAll))
            }
            (KeyCode::Esc, _) => Some(AppCommand::Editor(EditorCommand::ClearSelection)),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    // The same physical key must route to different AppCommand variants
    // depending on which pane has focus -- this is the whole point of
    // threading `Focus` through `map_key` rather than having one global
    // keymap.
    #[test]
    fn same_key_dispatches_to_tree_or_grid_depending_on_focus() {
        assert!(matches!(
            map_key(key(KeyCode::Up), Focus::Tree),
            Some(AppCommand::Tree(TreeCommand::MoveUp))
        ));
        assert!(matches!(
            map_key(key(KeyCode::Up), Focus::Grid),
            Some(AppCommand::Grid(GridCommand::MoveUp))
        ));
    }

    #[test]
    fn next_and_prev_page_keys_are_grid_only() {
        assert!(matches!(
            map_key(key(KeyCode::Char('n')), Focus::Grid),
            Some(AppCommand::Grid(GridCommand::NextPage))
        ));
        assert!(
            map_key(key(KeyCode::Char('n')), Focus::Tree).is_none(),
            "'n' has no tree-focused meaning and must not fall through to some other command"
        );
        assert!(matches!(
            map_key(key(KeyCode::Char('p')), Focus::Grid),
            Some(AppCommand::Grid(GridCommand::PrevPage))
        ));
        assert!(map_key(key(KeyCode::Char('p')), Focus::Tree).is_none());
    }

    #[test]
    fn enter_only_activates_in_tree_focus() {
        assert!(matches!(
            map_key(key(KeyCode::Enter), Focus::Tree),
            Some(AppCommand::Activate)
        ));
        assert!(
            map_key(key(KeyCode::Enter), Focus::Grid).is_none(),
            "Enter has no grid-focused binding"
        );
    }

    #[test]
    fn esc_only_returns_to_tree_when_grid_is_focused() {
        assert!(matches!(
            map_key(key(KeyCode::Esc), Focus::Grid),
            Some(AppCommand::FocusTree)
        ));
        assert!(
            map_key(key(KeyCode::Esc), Focus::Tree).is_none(),
            "Esc is a no-op while already focused on the tree"
        );
    }

    #[test]
    fn tab_toggles_focus_regardless_of_focus() {
        for focus in [Focus::Tree, Focus::Grid, Focus::Editor] {
            assert!(matches!(
                map_key(key(KeyCode::Tab), focus),
                Some(AppCommand::ToggleFocus)
            ));
        }
    }

    #[test]
    fn q_only_quits_outside_the_editor() {
        for focus in [Focus::Tree, Focus::Grid] {
            assert!(matches!(
                map_key(key(KeyCode::Char('q')), focus),
                Some(AppCommand::Quit)
            ));
        }
        assert!(matches!(
            map_key(key(KeyCode::Char('q')), Focus::Editor),
            Some(AppCommand::Editor(EditorCommand::Insert('q')))
        ));
    }

    #[test]
    fn ctrl_c_is_cancel_or_quit_regardless_of_focus() {
        for focus in [Focus::Tree, Focus::Grid, Focus::Editor] {
            assert!(matches!(
                map_key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    focus
                ),
                Some(AppCommand::CancelOrQuit)
            ));
        }
    }

    #[test]
    fn ctrl_r_and_ctrl_e_run_regardless_of_focus() {
        for focus in [Focus::Tree, Focus::Grid, Focus::Editor] {
            assert!(matches!(
                map_key(
                    KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                    focus
                ),
                Some(AppCommand::Run(RunKey::CursorOrSelection))
            ));
            assert!(matches!(
                map_key(
                    KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                    focus
                ),
                Some(AppCommand::Run(RunKey::Buffer))
            ));
        }
    }

    #[test]
    fn editor_focus_inserts_printable_characters() {
        assert!(matches!(
            map_key(key(KeyCode::Char('x')), Focus::Editor),
            Some(AppCommand::Editor(EditorCommand::Insert('x')))
        ));
    }

    #[test]
    fn editor_focus_shift_arrow_extends_selection() {
        assert!(matches!(
            map_key(
                KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
                Focus::Editor
            ),
            Some(AppCommand::Editor(EditorCommand::Left(Motion::Extend)))
        ));
        assert!(matches!(
            map_key(key(KeyCode::Left), Focus::Editor),
            Some(AppCommand::Editor(EditorCommand::Left(Motion::Move)))
        ));
    }

    // Phase 2: Ctrl+T/W/N/P must map to the tab commands in all three
    // `Focus` values -- in particular they must never be shadowed by
    // `Focus::Editor`'s bare-printable-character insert branch, since 't',
    // 'w', 'n', and 'p' are all otherwise ordinary letters a user would type.
    #[test]
    fn tab_management_keys_map_regardless_of_focus() {
        for focus in [Focus::Tree, Focus::Grid, Focus::Editor] {
            assert!(
                matches!(
                    map_key(ctrl_key(KeyCode::Char('t')), focus),
                    Some(AppCommand::OpenPicker)
                ),
                "Ctrl+T must open the picker in {focus:?}"
            );
            assert!(
                matches!(
                    map_key(ctrl_key(KeyCode::Char('w')), focus),
                    Some(AppCommand::CloseTab)
                ),
                "Ctrl+W must close the tab in {focus:?}"
            );
            assert!(
                matches!(
                    map_key(ctrl_key(KeyCode::Char('n')), focus),
                    Some(AppCommand::NextTab)
                ),
                "Ctrl+N must switch to the next tab in {focus:?}"
            );
            assert!(
                matches!(
                    map_key(ctrl_key(KeyCode::Char('p')), focus),
                    Some(AppCommand::PrevTab)
                ),
                "Ctrl+P must switch to the previous tab in {focus:?}"
            );
        }
    }

    #[test]
    fn tab_management_letters_without_ctrl_still_insert_in_editor_focus() {
        // The direct contrast that makes the test above meaningful: the very
        // same letters, without Ctrl, must behave as ordinary editor input in
        // `Focus::Editor` -- Ctrl+T/W/N/P must not have quietly repurposed
        // 't'/'w'/'n'/'p' themselves.
        for c in ['t', 'w', 'n', 'p'] {
            assert!(
                matches!(
                    map_key(key(KeyCode::Char(c)), Focus::Editor),
                    Some(AppCommand::Editor(EditorCommand::Insert(got))) if got == c
                ),
                "bare {c:?} in Focus::Editor must insert, not be captured by tab management"
            );
        }
    }

    #[test]
    fn editor_focus_esc_clears_selection_instead_of_focus_tree() {
        assert!(matches!(
            map_key(key(KeyCode::Esc), Focus::Editor),
            Some(AppCommand::Editor(EditorCommand::ClearSelection))
        ));
    }
}
