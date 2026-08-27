use crate::editor::{Motion, TextBuffer};

pub enum EditorCommand {
    Insert(char),
    Newline,
    DeleteBackward,
    DeleteForward,
    Left(Motion),
    Right(Motion),
    Up(Motion),
    Down(Motion),
    LineStart(Motion),
    LineEnd(Motion),
    BufferStart(Motion),
    BufferEnd(Motion),
    SelectAll,
    ClearSelection,
}

pub struct EditorState {
    buffer: TextBuffer,
    scroll_top: usize,
    scroll_left: usize,
    viewport: (u16, u16),
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            buffer: TextBuffer::new(),
            scroll_top: 0,
            scroll_left: 0,
            viewport: (80, 10),
        }
    }

    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    pub fn buffer_mut(&mut self) -> &mut TextBuffer {
        &mut self.buffer
    }

    pub fn command(&mut self, cmd: EditorCommand) {
        match cmd {
            EditorCommand::Insert(c) => self.buffer.insert_char(c),
            EditorCommand::Newline => self.buffer.insert_newline(),
            EditorCommand::DeleteBackward => self.buffer.delete_backward(),
            EditorCommand::DeleteForward => self.buffer.delete_forward(),
            EditorCommand::Left(m) => self.buffer.move_left(m),
            EditorCommand::Right(m) => self.buffer.move_right(m),
            EditorCommand::Up(m) => self.buffer.move_up(m),
            EditorCommand::Down(m) => self.buffer.move_down(m),
            EditorCommand::LineStart(m) => self.buffer.move_line_start(m),
            EditorCommand::LineEnd(m) => self.buffer.move_line_end(m),
            EditorCommand::BufferStart(m) => self.buffer.move_buffer_start(m),
            EditorCommand::BufferEnd(m) => self.buffer.move_buffer_end(m),
            EditorCommand::SelectAll => self.buffer.select_all(),
            EditorCommand::ClearSelection => self.buffer.clear_selection(),
        }
        self.scroll_to_cursor();
    }

    pub(crate) fn set_viewport(&mut self, w: u16, h: u16) {
        self.viewport = (w, h);
        self.scroll_to_cursor();
    }

    pub(crate) fn scroll_top(&self) -> usize {
        self.scroll_top
    }

    pub(crate) fn scroll_left(&self) -> usize {
        self.scroll_left
    }

    fn scroll_to_cursor(&mut self) {
        let cursor = self.buffer.cursor();
        let (w, h) = self.viewport;
        if cursor.line < self.scroll_top {
            self.scroll_top = cursor.line;
        } else if cursor.line >= self.scroll_top + h as usize {
            self.scroll_top = cursor.line + 1 - h as usize;
        }
        if cursor.col < self.scroll_left {
            self.scroll_left = cursor.col;
        } else if cursor.col >= self.scroll_left + w as usize {
            self.scroll_left = cursor.col + 1 - w as usize;
        }
    }

    /// Screen cell for Frame::set_cursor_position, relative to `inner`'s
    /// origin, or None if scrolled out of view.
    pub fn cursor_screen_pos(
        &self,
        inner: ratatui::layout::Rect,
    ) -> Option<ratatui::layout::Position> {
        let cursor = self.buffer.cursor();
        if cursor.line < self.scroll_top || cursor.col < self.scroll_left {
            return None;
        }
        let row = cursor.line - self.scroll_top;
        let col = cursor.col - self.scroll_left;
        if row as u16 >= inner.height || col as u16 >= inner.width {
            return None;
        }
        Some(ratatui::layout::Position::new(
            inner.x + col as u16,
            inner.y + row as u16,
        ))
    }
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Position;

    #[test]
    fn scroll_follows_cursor_down_past_the_viewport() {
        let mut state = EditorState::new();
        state.set_viewport(80, 3);
        for _ in 0..10 {
            state.command(EditorCommand::Newline);
        }
        assert_eq!(state.buffer().cursor(), Position { line: 10, col: 0 });
        assert!(state.scroll_top() > 0);
        assert!(state.buffer().cursor().line >= state.scroll_top());
        assert!(state.buffer().cursor().line < state.scroll_top() + 3);
    }

    #[test]
    fn cursor_screen_pos_is_none_when_the_rect_is_smaller_than_the_cursor_row() {
        let mut state = EditorState::new();
        state.set_viewport(80, 10);
        for _ in 0..5 {
            state.command(EditorCommand::Newline);
        }
        // Cursor is on line 5, within the 10-row viewport used for scroll
        // tracking, but the rect passed to `cursor_screen_pos` here is
        // smaller -- e.g. the pane shrank between the render call that set
        // the viewport and the cursor-position lookup.
        let inner = ratatui::layout::Rect::new(0, 0, 80, 2);
        assert_eq!(state.cursor_screen_pos(inner), None);
    }

    #[test]
    fn cursor_screen_pos_maps_into_the_inner_rect_when_visible() {
        let mut state = EditorState::new();
        state.set_viewport(80, 10);
        state.command(EditorCommand::Insert('a'));
        let inner = ratatui::layout::Rect::new(2, 1, 80, 10);
        let pos = state
            .cursor_screen_pos(inner)
            .expect("cursor should be visible");
        assert_eq!(pos, ratatui::layout::Position::new(3, 1));
    }
}
