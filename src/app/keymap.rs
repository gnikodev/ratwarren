use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Focus;
use crate::ui::grid::state::GridCommand;
use crate::ui::tree::state::TreeCommand;

pub enum AppCommand {
    Quit,
    ToggleFocus,
    FocusTree,
    Activate,
    Tree(TreeCommand),
    Grid(GridCommand),
}

pub fn map_key(key: KeyEvent, focus: Focus) -> Option<AppCommand> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) => return Some(AppCommand::Quit),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Some(AppCommand::Quit),
        (KeyCode::Tab, _) => return Some(AppCommand::ToggleFocus),
        (KeyCode::Esc, _) if focus == Focus::Grid => return Some(AppCommand::FocusTree),
        _ => {}
    }

    match focus {
        Focus::Tree => match (key.code, key.modifiers) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
    fn quit_and_toggle_focus_are_global_regardless_of_focus() {
        for focus in [Focus::Tree, Focus::Grid] {
            assert!(matches!(
                map_key(key(KeyCode::Char('q')), focus),
                Some(AppCommand::Quit)
            ));
            assert!(matches!(
                map_key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    focus
                ),
                Some(AppCommand::Quit)
            ));
            assert!(matches!(
                map_key(key(KeyCode::Tab), focus),
                Some(AppCommand::ToggleFocus)
            ));
        }
    }
}
