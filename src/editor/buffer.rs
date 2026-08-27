/// 0-based line, 0-based CHARACTER column (not byte offset, not display width).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

/// Whether a movement collapses the selection (Move) or extends it (Extend,
/// i.e. shift+motion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Move,
    Extend,
}

/// A plain-text edit buffer with cursor + optional selection.
///
/// Invariants, maintained after every mutating method:
/// - `lines` is never empty; no element contains `'\n'`.
/// - `cursor.line < lines.len()` and `cursor.col <= lines[cursor.line].chars().count()`.
/// - `anchor`, when `Some`, is a valid position in `lines` and is never equal
///   to `cursor`.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    lines: Vec<String>,
    cursor: Position,
    anchor: Option<Position>,
    goal_col: Option<usize>,
}

fn char_byte_index(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: Position::default(),
            anchor: None,
            goal_col: None,
        }
    }

    /// Splits on `'\n'` only (not `str::lines()`, which strips a trailing
    /// `'\r'` — sqlparser's tokenizer treats `'\r'` as ordinary line content,
    /// not a line terminator, so we must preserve it).
    pub fn from_text(text: &str) -> Self {
        let lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(String::from).collect()
        };
        Self {
            lines,
            cursor: Position::default(),
            anchor: None,
            goal_col: None,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// Normalized (start <= end); `None` if there's no anchor or the anchor
    /// coincides with the cursor (an empty selection is not a selection).
    pub fn selection(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn line_char_count(&self, line: usize) -> usize {
        self.lines[line].chars().count()
    }

    fn clamp_position(&self, pos: Position) -> Position {
        let line = pos.line.min(self.lines.len() - 1);
        let col = pos.col.min(self.line_char_count(line));
        Position { line, col }
    }

    /// Byte offset into the string `text()` returns, for the given
    /// (clamped) position.
    pub fn offset_of(&self, pos: Position) -> usize {
        let pos = self.clamp_position(pos);
        let mut offset = 0;
        for line in &self.lines[..pos.line] {
            offset += line.len() + 1;
        }
        offset + char_byte_index(&self.lines[pos.line], pos.col)
    }

    /// Inverse of `offset_of`. Clamps out-of-range input rather than
    /// panicking. If `offset` falls in the middle of a multi-byte codepoint,
    /// snaps down to the start of the character containing it (never splits
    /// a codepoint).
    pub fn position_of(&self, offset: usize) -> Position {
        let text_len = self.lines.iter().map(|l| l.len()).sum::<usize>() + self.lines.len() - 1;
        let offset = offset.min(text_len);

        let mut byte_pos = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            let line_len = line.len();
            if offset <= byte_pos + line_len {
                let col_bytes = offset - byte_pos;
                let mut col = 0;
                let mut consumed = 0usize;
                for ch in line.chars() {
                    let ch_len = ch.len_utf8();
                    if consumed + ch_len > col_bytes {
                        break;
                    }
                    consumed += ch_len;
                    col += 1;
                }
                return Position { line: i, col };
            }
            byte_pos += line_len + 1;
        }
        let last = self.lines.len() - 1;
        Position {
            line: last,
            col: self.line_char_count(last),
        }
    }

    /// Deletes the selection first if present, then inserts at the cursor.
    /// `c == '\n'` delegates to `insert_newline` (Enter can be routed
    /// through either method interchangeably).
    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            self.insert_newline();
            return;
        }
        self.delete_selection();
        let Position { line, col } = self.cursor;
        let byte_idx = char_byte_index(&self.lines[line], col);
        self.lines[line].insert(byte_idx, c);
        self.cursor.col = col + 1;
        self.goal_col = None;
    }

    /// Deletes the selection first if present, then inserts `s` at the
    /// cursor, splitting on `'\n'` as needed (the paste path).
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        if s.is_empty() {
            return;
        }
        let Position { line, col } = self.cursor;
        let byte_idx = char_byte_index(&self.lines[line], col);
        let tail = self.lines[line].split_off(byte_idx);

        let mut parts = s.split('\n');
        let first = parts.next().expect("split always yields at least one part");
        self.lines[line].push_str(first);

        let rest: Vec<&str> = parts.collect();
        if rest.is_empty() {
            self.lines[line].push_str(&tail);
            self.cursor = Position {
                line,
                col: col + first.chars().count(),
            };
        } else {
            let mut insert_at = line + 1;
            for part in &rest[..rest.len() - 1] {
                self.lines.insert(insert_at, part.to_string());
                insert_at += 1;
            }
            let last_part = rest[rest.len() - 1];
            let mut last_line = last_part.to_string();
            last_line.push_str(&tail);
            let last_col = last_part.chars().count();
            self.lines.insert(insert_at, last_line);
            self.cursor = Position {
                line: insert_at,
                col: last_col,
            };
        }
        self.goal_col = None;
    }

    /// Deletes the selection first if present, then splits the current line
    /// at the cursor into two lines, moving the cursor to the start of the
    /// new second line.
    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let Position { line, col } = self.cursor;
        let byte_idx = char_byte_index(&self.lines[line], col);
        let tail = self.lines[line].split_off(byte_idx);
        self.lines.insert(line + 1, tail);
        self.cursor = Position {
            line: line + 1,
            col: 0,
        };
        self.goal_col = None;
    }

    pub fn delete_backward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Position { line, col } = self.cursor;
        if col > 0 {
            let start = char_byte_index(&self.lines[line], col - 1);
            let end = char_byte_index(&self.lines[line], col);
            self.lines[line].replace_range(start..end, "");
            self.cursor.col = col - 1;
        } else if line > 0 {
            let prev_len = self.line_char_count(line - 1);
            let cur = self.lines.remove(line);
            self.lines[line - 1].push_str(&cur);
            self.cursor = Position {
                line: line - 1,
                col: prev_len,
            };
        }
        self.goal_col = None;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Position { line, col } = self.cursor;
        let char_count = self.line_char_count(line);
        if col < char_count {
            let start = char_byte_index(&self.lines[line], col);
            let end = char_byte_index(&self.lines[line], col + 1);
            self.lines[line].replace_range(start..end, "");
        } else if line + 1 < self.lines.len() {
            let next = self.lines.remove(line + 1);
            self.lines[line].push_str(&next);
        }
        self.goal_col = None;
    }

    /// If a selection exists, removes the selected text, moves the cursor to
    /// the (normalized) start of the former selection, clears the anchor,
    /// and returns `true`. Otherwise clears the anchor (if any) and returns
    /// `false`.
    ///
    /// Every mutating method routes through here first, and an edit
    /// invalidates any anchor -- including a stale one that merely coincides
    /// with the cursor (`selection()` reports `None` for that, but the edit
    /// then moves the cursor away from it, resurrecting a phantom
    /// one-character selection that the next keystroke would delete).
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let start_off = self.offset_of(start);
        let end_off = self.offset_of(end);
        let mut text = self.text();
        text.replace_range(start_off..end_off, "");

        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(String::from).collect()
        };
        self.cursor = self.clamp_position(start);
        self.anchor = None;
        self.goal_col = None;
        true
    }

    /// Canonical form: an anchor equal to the cursor is not a selection, so it
    /// must not be retained -- a later edit would move the cursor away from it
    /// and turn it back into a phantom selection.
    fn canonicalize_anchor(&mut self) {
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    pub fn move_to(&mut self, pos: Position, m: Motion) {
        let pos = self.clamp_position(pos);
        match m {
            Motion::Extend => {
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor);
                }
            }
            Motion::Move => self.anchor = None,
        }
        self.cursor = pos;
        self.goal_col = None;
        self.canonicalize_anchor();
    }

    pub fn move_left(&mut self, m: Motion) {
        let Position { line, col } = self.cursor;
        let target = if col > 0 {
            Position { line, col: col - 1 }
        } else if line > 0 {
            Position {
                line: line - 1,
                col: self.line_char_count(line - 1),
            }
        } else {
            Position { line, col }
        };
        self.move_to(target, m);
    }

    pub fn move_right(&mut self, m: Motion) {
        let Position { line, col } = self.cursor;
        let char_count = self.line_char_count(line);
        let target = if col < char_count {
            Position { line, col: col + 1 }
        } else if line + 1 < self.lines.len() {
            Position {
                line: line + 1,
                col: 0,
            }
        } else {
            Position { line, col }
        };
        self.move_to(target, m);
    }

    fn apply_vertical(&mut self, pos: Position, m: Motion) {
        match m {
            Motion::Extend => {
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor);
                }
            }
            Motion::Move => self.anchor = None,
        }
        self.cursor = pos;
        self.canonicalize_anchor();
    }

    pub fn move_up(&mut self, m: Motion) {
        let goal = self.goal_col.unwrap_or(self.cursor.col);
        self.goal_col = Some(goal);
        let new_line = self.cursor.line.saturating_sub(1);
        let target = Position {
            line: new_line,
            col: goal.min(self.line_char_count(new_line)),
        };
        self.apply_vertical(target, m);
    }

    pub fn move_down(&mut self, m: Motion) {
        let goal = self.goal_col.unwrap_or(self.cursor.col);
        self.goal_col = Some(goal);
        let new_line = (self.cursor.line + 1).min(self.lines.len() - 1);
        let target = Position {
            line: new_line,
            col: goal.min(self.line_char_count(new_line)),
        };
        self.apply_vertical(target, m);
    }

    pub fn move_line_start(&mut self, m: Motion) {
        let line = self.cursor.line;
        self.move_to(Position { line, col: 0 }, m);
    }

    pub fn move_line_end(&mut self, m: Motion) {
        let line = self.cursor.line;
        let col = self.line_char_count(line);
        self.move_to(Position { line, col }, m);
    }

    pub fn move_buffer_start(&mut self, m: Motion) {
        self.move_to(Position { line: 0, col: 0 }, m);
    }

    pub fn move_buffer_end(&mut self, m: Motion) {
        let line = self.lines.len() - 1;
        let col = self.line_char_count(line);
        self.move_to(Position { line, col }, m);
    }

    pub fn select_all(&mut self) {
        let end_line = self.lines.len() - 1;
        self.anchor = Some(Position { line: 0, col: 0 });
        self.cursor = Position {
            line: end_line,
            col: self.line_char_count(end_line),
        };
        self.goal_col = None;
        self.canonicalize_anchor();
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invariants(buf: &TextBuffer) {
        assert!(!buf.lines.is_empty());
        for line in &buf.lines {
            assert!(!line.contains('\n'));
        }
        assert!(buf.cursor.line < buf.lines.len());
        assert!(buf.cursor.col <= buf.line_char_count(buf.cursor.line));
        if let Some(anchor) = buf.anchor {
            assert!(anchor.line < buf.lines.len());
            assert!(anchor.col <= buf.line_char_count(anchor.line));
            assert_ne!(anchor, buf.cursor);
        }
    }

    #[test]
    fn text_round_trips_including_trailing_newline() {
        for s in ["", "a", "a\nb\n", "a\nb", "\n", "a\n\nb"] {
            assert_eq!(TextBuffer::from_text(s).text(), s);
        }
    }

    #[test]
    fn new_and_empty() {
        let buf = TextBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.lines(), &[String::new()]);
        assert_invariants(&buf);
    }

    #[test]
    fn insert_char_and_newline_delegate() {
        let mut buf = TextBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        buf.insert_char('\n');
        buf.insert_char('c');
        assert_eq!(buf.text(), "ab\nc");
        assert_eq!(buf.cursor(), Position { line: 1, col: 1 });
        assert_invariants(&buf);
    }

    #[test]
    fn insert_str_multiline_paste() {
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 5 }, Motion::Move);
        buf.insert_str(" cruel\nnew");
        assert_eq!(buf.text(), "hello cruel\nnew world");
        assert_invariants(&buf);
    }

    #[test]
    fn delete_backward_joins_lines_at_column_zero() {
        let mut buf = TextBuffer::from_text("a\nb");
        buf.move_to(Position { line: 1, col: 0 }, Motion::Move);
        buf.delete_backward();
        assert_eq!(buf.text(), "ab");
        assert_eq!(buf.cursor(), Position { line: 0, col: 1 });
        assert_invariants(&buf);
    }

    #[test]
    fn delete_backward_at_buffer_start_is_noop() {
        let mut buf = TextBuffer::from_text("abc");
        buf.delete_backward();
        assert_eq!(buf.text(), "abc");
        assert_invariants(&buf);
    }

    #[test]
    fn delete_forward_joins_lines_at_line_end() {
        let mut buf = TextBuffer::from_text("a\nb");
        buf.move_to(Position { line: 0, col: 1 }, Motion::Move);
        buf.delete_forward();
        assert_eq!(buf.text(), "ab");
        assert_invariants(&buf);
    }

    #[test]
    fn delete_forward_at_buffer_end_is_noop() {
        let mut buf = TextBuffer::from_text("abc");
        buf.move_buffer_end(Motion::Move);
        buf.delete_forward();
        assert_eq!(buf.text(), "abc");
        assert_invariants(&buf);
    }

    #[test]
    fn selection_delete_clears_anchor_and_moves_cursor_to_start() {
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 5 }, Motion::Extend);
        assert!(buf.delete_selection());
        assert_eq!(buf.text(), " world");
        assert_eq!(buf.cursor(), Position { line: 0, col: 0 });
        assert_eq!(buf.selection(), None);
        assert_invariants(&buf);
    }

    #[test]
    fn empty_selection_is_none() {
        let mut buf = TextBuffer::from_text("abc");
        buf.move_to(Position { line: 0, col: 1 }, Motion::Extend);
        buf.move_to(Position { line: 0, col: 1 }, Motion::Move);
        assert_eq!(buf.selection(), None);
    }

    #[test]
    fn insert_char_deletes_active_selection_first() {
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 5 }, Motion::Extend);
        buf.insert_char('X');
        assert_eq!(buf.text(), "X world");
        assert_invariants(&buf);
    }

    #[test]
    fn move_up_down_honor_goal_col() {
        let mut buf = TextBuffer::from_text("longline\nhi\nlongline");
        buf.move_to(Position { line: 0, col: 6 }, Motion::Move);
        buf.move_down(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 1, col: 2 }); // clamped to "hi"
        buf.move_down(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 2, col: 6 }); // restored via goal_col
        assert_invariants(&buf);
    }

    #[test]
    fn move_up_at_top_is_noop() {
        let mut buf = TextBuffer::from_text("abc");
        buf.move_to(Position { line: 0, col: 2 }, Motion::Move);
        buf.move_up(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 0, col: 2 });
        assert_invariants(&buf);
    }

    #[test]
    fn offset_of_and_position_of_round_trip_with_multibyte() {
        let buf = TextBuffer::from_text("a🦀b\nsecond");
        let pos = Position { line: 0, col: 3 };
        let offset = buf.offset_of(pos);
        assert_eq!(buf.position_of(offset), pos);
    }

    #[test]
    fn position_of_out_of_range_clamps_instead_of_panicking() {
        let buf = TextBuffer::from_text("abc");
        assert_eq!(buf.position_of(1000), Position { line: 0, col: 3 });
    }

    #[test]
    fn move_to_clamps_out_of_range_position() {
        let mut buf = TextBuffer::from_text("ab\nc");
        buf.move_to(Position { line: 99, col: 99 }, Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 1, col: 1 });
        assert_invariants(&buf);
    }

    #[test]
    fn select_all_selects_full_buffer() {
        let mut buf = TextBuffer::from_text("ab\ncd");
        buf.select_all();
        let (start, end) = buf.selection().unwrap();
        assert_eq!(start, Position { line: 0, col: 0 });
        assert_eq!(end, Position { line: 1, col: 2 });
        assert_invariants(&buf);
    }

    #[test]
    fn select_all_on_empty_buffer_does_not_leave_a_phantom_anchor() {
        // Buffer start == buffer end when the buffer is empty, so anchor and
        // cursor would coincide -- select_all must canonicalize that away
        // (anchor == None), same as every other cursor-moving method does,
        // rather than violating the "anchor is never equal to cursor"
        // invariant.
        let mut buf = TextBuffer::new();
        buf.select_all();
        assert_eq!(buf.selection(), None);
        assert_invariants(&buf);
    }

    #[test]
    fn select_all_resets_a_stale_goal_col() {
        let mut buf = TextBuffer::from_text("longline\nhi\nlongline");
        buf.move_to(Position { line: 0, col: 6 }, Motion::Move);
        buf.move_down(Motion::Move); // sets goal_col to 6
        buf.select_all();
        buf.move_down(Motion::Extend); // must be a no-op: already on the last line
        let (_, end) = buf.selection().unwrap();
        assert_eq!(
            end,
            Position { line: 2, col: 8 },
            "selection must still cover the whole buffer"
        );
        assert_invariants(&buf);
    }

    // ---- selection + edit interaction --------------------------------------
    //
    // `insert_char_deletes_active_selection_first` (above) already covers
    // "select then type a character". These cover the other two ordered
    // combinations: select then backspace, select then delete-forward.

    #[test]
    fn backspace_over_an_active_selection_deletes_only_the_selection() {
        // Regression guard: backspace with a selection must delete exactly
        // the selected range, not the selection PLUS one extra char before
        // it (a classic off-by-one if backspace didn't special-case an
        // active selection).
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 5 }, Motion::Extend);
        buf.delete_backward();
        assert_eq!(buf.text(), " world");
        assert_eq!(buf.cursor(), Position { line: 0, col: 0 });
        assert_invariants(&buf);
    }

    #[test]
    fn delete_forward_over_an_active_selection_deletes_only_the_selection() {
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 5 }, Motion::Extend);
        buf.delete_forward();
        assert_eq!(buf.text(), " world");
        assert_invariants(&buf);
    }

    // ---- cursor motion at buffer edges (no-ops, not panics) ----------------

    #[test]
    fn move_left_at_buffer_start_is_noop() {
        let mut buf = TextBuffer::from_text("ab\ncd");
        buf.move_left(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 0, col: 0 });
        assert_invariants(&buf);
    }

    #[test]
    fn move_right_at_buffer_end_is_noop() {
        let mut buf = TextBuffer::from_text("ab\ncd");
        buf.move_buffer_end(Motion::Move);
        let end = buf.cursor();
        buf.move_right(Motion::Move);
        assert_eq!(buf.cursor(), end);
        assert_invariants(&buf);
    }

    #[test]
    fn move_down_on_last_line_is_noop() {
        let mut buf = TextBuffer::from_text("ab\ncd");
        buf.move_to(Position { line: 1, col: 1 }, Motion::Move);
        buf.move_down(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 1, col: 1 });
        assert_invariants(&buf);
    }

    // ---- selection anchor semantics ----------------------------------------

    #[test]
    fn extend_with_no_existing_selection_anchors_at_pre_move_cursor() {
        let mut buf = TextBuffer::from_text("hello world");
        buf.move_to(Position { line: 0, col: 3 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 7 }, Motion::Extend);
        let (start, end) = buf.selection().unwrap();
        assert_eq!(start, Position { line: 0, col: 3 });
        assert_eq!(end, Position { line: 0, col: 7 });
    }

    #[test]
    fn consecutive_extends_keep_the_original_anchor_fixed() {
        let mut buf = TextBuffer::from_text("abcdefghij");
        buf.move_to(Position { line: 0, col: 2 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 4 }, Motion::Extend);
        buf.move_to(Position { line: 0, col: 6 }, Motion::Extend);
        buf.move_to(Position { line: 0, col: 8 }, Motion::Extend);
        let (start, end) = buf.selection().unwrap();
        assert_eq!(start, Position { line: 0, col: 2 });
        assert_eq!(end, Position { line: 0, col: 8 });
    }

    #[test]
    fn extend_then_retreat_to_a_zero_width_selection_does_not_leave_a_phantom_anchor() {
        // Bug 3 repro: extending right then left back onto the anchor
        // produces a coincident anchor/cursor, which `selection()` correctly
        // reports as `None` -- but the stale `anchor` used to survive
        // unmutated. The next edit (insert_char) would then move the cursor
        // away from it, resurrecting a phantom one-character selection that
        // the FOLLOWING edit would silently delete.
        let mut buf = TextBuffer::from_text("");
        buf.move_right(Motion::Extend);
        buf.move_left(Motion::Extend);
        assert_eq!(buf.selection(), None);
        buf.insert_char('X');
        buf.insert_char('Y');
        assert_eq!(buf.text(), "XY");
        assert_invariants(&buf);
    }

    #[test]
    fn switching_from_extend_to_move_collapses_the_selection() {
        let mut buf = TextBuffer::from_text("abcdefghij");
        buf.move_to(Position { line: 0, col: 2 }, Motion::Move);
        buf.move_to(Position { line: 0, col: 6 }, Motion::Extend);
        assert!(buf.selection().is_some());
        buf.move_to(Position { line: 0, col: 6 }, Motion::Move);
        assert_eq!(buf.selection(), None);
    }

    // ---- goal_col ------------------------------------------------------------

    #[test]
    fn goal_col_restores_through_short_then_long_lines() {
        let mut buf = TextBuffer::from_text("longline\nhi\nlongline");
        buf.move_to(Position { line: 0, col: 6 }, Motion::Move);
        buf.move_down(Motion::Move);
        assert_eq!(
            buf.cursor(),
            Position { line: 1, col: 2 },
            "clamped to short line's length"
        );
        buf.move_down(Motion::Move);
        assert_eq!(
            buf.cursor(),
            Position { line: 2, col: 6 },
            "goal_col restored once a long-enough line is reached again"
        );
        assert_invariants(&buf);
    }

    #[test]
    fn goal_col_is_cleared_by_a_subsequent_horizontal_move() {
        let mut buf = TextBuffer::from_text("longline\nhi\nlongline");
        buf.move_to(Position { line: 0, col: 6 }, Motion::Move);
        buf.move_down(Motion::Move);
        assert_eq!(
            buf.cursor(),
            Position { line: 1, col: 2 },
            "clamped to \"hi\"'s length, goal_col recorded as 6"
        );
        buf.move_left(Motion::Move);
        assert_eq!(buf.cursor(), Position { line: 1, col: 1 });
        // A further move_down must NOT still honor the stale goal_col (6)
        // from before the horizontal move -- move_left must have cleared it,
        // so the new goal is the post-move_left column (1).
        buf.move_down(Motion::Move);
        assert_eq!(
            buf.cursor(),
            Position { line: 2, col: 1 },
            "goal_col must have been reset to 1 by move_left, not left at the stale 6"
        );
        assert_invariants(&buf);
    }

    // ---- UTF-8 multi-byte stress --------------------------------------------

    #[test]
    fn move_right_over_emoji_lands_after_the_whole_character() {
        let mut buf = TextBuffer::from_text("a🦀b");
        buf.move_to(Position { line: 0, col: 0 }, Motion::Move);
        buf.move_right(Motion::Move); // past 'a'
        assert_eq!(buf.cursor(), Position { line: 0, col: 1 });
        buf.move_right(Motion::Move); // past the whole crab, not mid-codepoint
        assert_eq!(buf.cursor(), Position { line: 0, col: 2 });
        let offset = buf.offset_of(buf.cursor());
        assert!(buf.text().is_char_boundary(offset));
        assert_eq!(&buf.text()[offset..], "b");
        assert_invariants(&buf);
    }

    #[test]
    fn offset_and_position_round_trip_with_cjk_and_emoji_mixed() {
        let buf = TextBuffer::from_text("日本🦀語\nsecond");
        for col in 0..=4 {
            let pos = Position { line: 0, col };
            let offset = buf.offset_of(pos);
            assert!(buf.text().is_char_boundary(offset));
            assert_eq!(buf.position_of(offset), pos);
        }
    }
}
