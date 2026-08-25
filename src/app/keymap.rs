use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::tree::state::TreeCommand;

pub enum AppCommand {
    Quit,
    Tree(TreeCommand),
}

pub fn map_key(key: KeyEvent) -> Option<AppCommand> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) => Some(AppCommand::Quit),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(AppCommand::Quit),
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => Some(AppCommand::Tree(TreeCommand::MoveUp)),
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            Some(AppCommand::Tree(TreeCommand::MoveDown))
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
            Some(AppCommand::Tree(TreeCommand::Expand))
        }
        (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
            Some(AppCommand::Tree(TreeCommand::Collapse))
        }
        (KeyCode::Enter, _) => Some(AppCommand::Tree(TreeCommand::Toggle)),
        (KeyCode::PageUp, _) => Some(AppCommand::Tree(TreeCommand::PageUp)),
        (KeyCode::PageDown, _) => Some(AppCommand::Tree(TreeCommand::PageDown)),
        (KeyCode::Home, _) | (KeyCode::Char('g'), _) => Some(AppCommand::Tree(TreeCommand::First)),
        (KeyCode::End, _) | (KeyCode::Char('G'), _) => Some(AppCommand::Tree(TreeCommand::Last)),
        (KeyCode::Char('r'), _) => Some(AppCommand::Tree(TreeCommand::Refresh)),
        (KeyCode::Char('.'), _) => Some(AppCommand::Tree(TreeCommand::ToggleSystemSchemas)),
        _ => None,
    }
}
