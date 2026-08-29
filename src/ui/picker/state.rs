use ratatui::widgets::ListState;

use crate::config::Config;

pub enum PickerRow {
    GroupHeader { label: Option<String> },
    Connection { name: String },
}

pub enum PickerCommand {
    MoveUp,
    MoveDown,
    First,
    Last,
}

pub struct PickerState {
    rows: Vec<PickerRow>,
    selected: usize,
    list: ListState,
}

impl PickerState {
    /// Flattens `Config::grouped()` into owned rows. Owned, not borrowed:
    /// `ConnectionGroup<'a>` borrows the `Config` and cannot be held across
    /// frames without a self-referential struct.
    pub fn from_config(config: &Config) -> PickerState {
        let mut rows = Vec::new();
        for group in config.grouped() {
            rows.push(PickerRow::GroupHeader {
                label: group.label.map(str::to_string),
            });
            for connection in group.connections {
                rows.push(PickerRow::Connection {
                    name: connection.name.clone(),
                });
            }
        }

        let selected = rows
            .iter()
            .position(|row| matches!(row, PickerRow::Connection { .. }))
            .unwrap_or(0);

        let mut list = ListState::default();
        if rows
            .iter()
            .any(|row| matches!(row, PickerRow::Connection { .. }))
        {
            list.select(Some(selected));
        }

        PickerState {
            rows,
            selected,
            list,
        }
    }

    pub fn command(&mut self, cmd: PickerCommand) {
        match cmd {
            PickerCommand::MoveUp => self.move_by(-1),
            PickerCommand::MoveDown => self.move_by(1),
            PickerCommand::First => self.jump_first(),
            PickerCommand::Last => self.jump_last(),
        }
    }

    pub fn selected_connection(&self) -> Option<&str> {
        match self.rows.get(self.selected) {
            Some(PickerRow::Connection { name }) => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self
            .rows
            .iter()
            .any(|row| matches!(row, PickerRow::Connection { .. }))
    }

    pub(crate) fn rows(&self) -> &[PickerRow] {
        &self.rows
    }

    pub(crate) fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let mut idx = self.selected as isize;
        loop {
            idx += delta;
            if idx < 0 || idx as usize >= self.rows.len() {
                return;
            }
            if matches!(self.rows[idx as usize], PickerRow::Connection { .. }) {
                self.select(idx as usize);
                return;
            }
        }
    }

    fn jump_first(&mut self) {
        if let Some(idx) = self
            .rows
            .iter()
            .position(|row| matches!(row, PickerRow::Connection { .. }))
        {
            self.select(idx);
        }
    }

    fn jump_last(&mut self) {
        if let Some(idx) = self
            .rows
            .iter()
            .rposition(|row| matches!(row, PickerRow::Connection { .. }))
        {
            self.select(idx);
        }
    }

    fn select(&mut self, idx: usize) {
        self.selected = idx;
        self.list.select(Some(idx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Connection;

    fn conn(name: &str, group: Option<&str>) -> Connection {
        Connection {
            name: name.to_string(),
            group: group.map(str::to_string),
            host: "localhost".to_string(),
            port: 5432,
            database: "app".to_string(),
            user: "app_user".to_string(),
            password: None,
            tunnel: None,
        }
    }

    // Groups: ungrouped [c1, c3], "g1" [c2, c4] -- bucket order is first
    // appearance (ungrouped first, since c1 appears before c2), member order
    // is file order within a bucket. Flattened rows:
    //   0: Header(None)
    //   1: Connection(c1)
    //   2: Connection(c3)
    //   3: Header(Some("g1"))
    //   4: Connection(c2)
    //   5: Connection(c4)
    fn two_group_config() -> Config {
        Config {
            connections: vec![
                conn("c1", None),
                conn("c2", Some("g1")),
                conn("c3", None),
                conn("c4", Some("g1")),
            ],
        }
    }

    #[test]
    fn from_config_with_no_connections_has_no_selection_and_is_empty() {
        let state = PickerState::from_config(&Config::default());
        assert!(state.is_empty());
        assert_eq!(state.selected_connection(), None);
        assert_eq!(state.list.selected(), None);
    }

    #[test]
    fn from_config_lands_the_initial_selection_on_the_first_connection_row() {
        let state = PickerState::from_config(&two_group_config());
        assert!(!state.is_empty());
        assert_eq!(state.selected_connection(), Some("c1"));
        assert_eq!(state.list.selected(), Some(1));
    }

    #[test]
    fn single_connection_config_selects_it_and_stays_put_on_any_navigation() {
        let config = Config {
            connections: vec![conn("only", None)],
        };
        let mut state = PickerState::from_config(&config);
        assert_eq!(state.selected_connection(), Some("only"));

        for cmd in [
            PickerCommand::MoveUp,
            PickerCommand::MoveDown,
            PickerCommand::First,
            PickerCommand::Last,
        ] {
            state.command(cmd);
            assert_eq!(state.selected_connection(), Some("only"));
        }
    }

    #[test]
    fn move_down_skips_group_headers_and_stops_at_the_last_connection() {
        let mut state = PickerState::from_config(&two_group_config());
        assert_eq!(state.selected_connection(), Some("c1"));

        state.command(PickerCommand::MoveDown);
        assert_eq!(state.selected_connection(), Some("c3"));

        // Crossing the "g1" header must land on c2, not on the header itself.
        state.command(PickerCommand::MoveDown);
        assert_eq!(state.selected_connection(), Some("c2"));

        state.command(PickerCommand::MoveDown);
        assert_eq!(state.selected_connection(), Some("c4"));

        // Already on the last connection row -- must not move further.
        state.command(PickerCommand::MoveDown);
        assert_eq!(state.selected_connection(), Some("c4"));
    }

    #[test]
    fn move_up_skips_group_headers_and_stops_at_the_first_connection() {
        let mut state = PickerState::from_config(&two_group_config());
        state.command(PickerCommand::Last);
        assert_eq!(state.selected_connection(), Some("c4"));

        state.command(PickerCommand::MoveUp);
        assert_eq!(state.selected_connection(), Some("c2"));

        // Crossing the "g1" header going up must land on c3, not the header.
        state.command(PickerCommand::MoveUp);
        assert_eq!(state.selected_connection(), Some("c3"));

        state.command(PickerCommand::MoveUp);
        assert_eq!(state.selected_connection(), Some("c1"));

        // Already on the first connection row -- must not move further.
        state.command(PickerCommand::MoveUp);
        assert_eq!(state.selected_connection(), Some("c1"));
    }

    #[test]
    fn first_and_last_jump_directly_to_the_bounding_connection_rows() {
        let mut state = PickerState::from_config(&two_group_config());
        state.command(PickerCommand::MoveDown);
        assert_eq!(state.selected_connection(), Some("c3"));

        state.command(PickerCommand::Last);
        assert_eq!(state.selected_connection(), Some("c4"));

        state.command(PickerCommand::First);
        assert_eq!(state.selected_connection(), Some("c1"));
    }

    #[test]
    fn headers_are_never_reachable_as_the_selected_connection() {
        let mut state = PickerState::from_config(&two_group_config());
        for _ in 0..10 {
            state.command(PickerCommand::MoveDown);
            assert!(
                state.selected_connection().is_some(),
                "every navigation step must keep the selection on a Connection row, never a \
                 GroupHeader, got rows[{}] = {:?}",
                state.selected,
                state.rows.get(state.selected).map(row_to_debug_tag)
            );
        }
    }

    #[test]
    fn commands_on_an_empty_picker_do_not_panic_and_stay_unselected() {
        let mut state = PickerState::from_config(&Config::default());
        for cmd in [
            PickerCommand::MoveUp,
            PickerCommand::MoveDown,
            PickerCommand::First,
            PickerCommand::Last,
        ] {
            state.command(cmd);
            assert_eq!(state.selected_connection(), None);
        }
    }

    fn row_to_debug_tag(row: &PickerRow) -> &'static str {
        match row {
            PickerRow::GroupHeader { .. } => "GroupHeader",
            PickerRow::Connection { .. } => "Connection",
        }
    }
}
