use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget};

use crate::editor::Position;
use crate::ui::editor::state::EditorState;

pub struct EditorWidget<'a> {
    block: Option<Block<'a>>,
    selection_style: Style,
}

impl<'a> Default for EditorWidget<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> EditorWidget<'a> {
    pub fn new() -> Self {
        Self {
            block: None,
            selection_style: Style::default().add_modifier(Modifier::REVERSED),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }
}

impl StatefulWidget for EditorWidget<'_> {
    type State = EditorState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut EditorState) {
        let inner = self.block.as_ref().map_or(area, |b| b.inner(area));
        state.set_viewport(inner.width.max(1), inner.height.max(1));

        let selection = state.buffer().selection();
        let scroll_top = state.scroll_top();
        let scroll_left = state.scroll_left();
        let width = inner.width as usize;

        let lines: Vec<Line> = state
            .buffer()
            .lines()
            .iter()
            .enumerate()
            .skip(scroll_top)
            .take(inner.height as usize)
            .map(|(line_idx, text)| {
                render_line(
                    line_idx,
                    text,
                    selection,
                    scroll_left,
                    width,
                    self.selection_style,
                )
            })
            .collect();

        let mut para = Paragraph::new(lines);
        if let Some(block) = self.block {
            para = para.block(block);
        }
        Widget::render(para, area, buf);
    }
}

/// Windows `text` to the visible horizontal range `[scroll_left,
/// scroll_left + width)` BY CHARACTER (never by byte, to stay panic-free and
/// correct on non-ASCII text), then, if `selection` overlaps this line,
/// splits the windowed text into up to 3 spans at the selection's char-column
/// boundaries within this line, styling the middle span with
/// `selection_style`.
fn render_line(
    line_idx: usize,
    text: &str,
    selection: Option<(Position, Position)>,
    scroll_left: usize,
    width: usize,
    selection_style: Style,
) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let start = scroll_left.min(total);
    let end = (scroll_left + width).min(total);
    let windowed: String = chars[start..end].iter().collect();

    let Some((sel_start, sel_end)) = selection else {
        return Line::from(windowed);
    };
    if line_idx < sel_start.line || line_idx > sel_end.line {
        return Line::from(windowed);
    }

    let line_sel_start = if line_idx == sel_start.line {
        sel_start.col
    } else {
        0
    };
    let line_sel_end = if line_idx == sel_end.line {
        sel_end.col
    } else {
        total
    };

    let clip_start = line_sel_start.clamp(start, end);
    let clip_end = line_sel_end.clamp(start, end);
    if clip_start >= clip_end {
        return Line::from(windowed);
    }

    let local_start = clip_start - start;
    let local_end = clip_end - start;
    let windowed_chars: Vec<char> = windowed.chars().collect();

    let before: String = windowed_chars[..local_start].iter().collect();
    let middle: String = windowed_chars[local_start..local_end].iter().collect();
    let after: String = windowed_chars[local_end..].iter().collect();

    let mut spans = Vec::with_capacity(3);
    if !before.is_empty() {
        spans.push(Span::raw(before));
    }
    if !middle.is_empty() {
        spans.push(Span::styled(middle, selection_style));
    }
    if !after.is_empty() {
        spans.push(Span::raw(after));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Position;
    use ratatui::style::Color;

    fn plain_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_line_without_selection_is_a_single_plain_span() {
        let line = render_line(0, "hello world", None, 0, 80, Style::default());
        assert_eq!(plain_text(&line), "hello world");
        assert_eq!(line.spans.len(), 1);
    }

    #[test]
    fn render_line_windows_to_the_visible_horizontal_range() {
        let line = render_line(0, "0123456789", None, 3, 4, Style::default());
        assert_eq!(plain_text(&line), "3456");
    }

    #[test]
    fn render_line_on_empty_line_does_not_panic() {
        let line = render_line(0, "", None, 0, 80, Style::default());
        assert_eq!(plain_text(&line), "");
    }

    #[test]
    fn render_line_scroll_past_line_end_renders_empty_not_panicking() {
        let line = render_line(0, "abc", None, 100, 10, Style::default());
        assert_eq!(plain_text(&line), "");
    }

    #[test]
    fn render_line_splits_into_three_spans_for_an_interior_selection() {
        let style = Style::default().fg(Color::Red);
        let sel = (Position { line: 0, col: 2 }, Position { line: 0, col: 5 });
        let line = render_line(0, "0123456789", Some(sel), 0, 80, style);
        assert_eq!(plain_text(&line), "0123456789");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content.as_ref(), "01");
        assert_eq!(line.spans[1].content.as_ref(), "234");
        assert_eq!(line.spans[1].style, style);
        assert_eq!(line.spans[2].content.as_ref(), "56789");
    }

    #[test]
    fn render_line_selection_from_the_very_start_omits_the_leading_span() {
        let sel = (Position { line: 0, col: 0 }, Position { line: 0, col: 3 });
        let line = render_line(0, "abcdef", Some(sel), 0, 80, Style::default());
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "abc");
    }

    #[test]
    fn render_line_selection_to_the_very_end_omits_the_trailing_span() {
        let sel = (Position { line: 0, col: 3 }, Position { line: 0, col: 6 });
        let line = render_line(0, "abcdef", Some(sel), 0, 80, Style::default());
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[1].content.as_ref(), "def");
    }

    #[test]
    fn render_line_full_line_selected_across_multi_line_range() {
        // line_idx is strictly between sel_start.line and sel_end.line: the
        // whole line is selected.
        let sel = (Position { line: 0, col: 2 }, Position { line: 2, col: 1 });
        let line = render_line(1, "middle", Some(sel), 0, 80, Style::default());
        assert_eq!(line.spans.len(), 1);
        assert_eq!(plain_text(&line), "middle");
    }

    #[test]
    fn render_line_selection_outside_visible_window_highlights_nothing() {
        let sel = (Position { line: 0, col: 20 }, Position { line: 0, col: 25 });
        let line = render_line(0, "0123456789", Some(sel), 0, 5, Style::default());
        assert_eq!(plain_text(&line), "01234");
        assert_eq!(line.spans.len(), 1);
    }

    #[test]
    fn render_line_does_not_panic_on_non_ascii_text_with_selection() {
        let sel = (Position { line: 0, col: 1 }, Position { line: 0, col: 3 });
        let line = render_line(0, "a🦀b日", Some(sel), 0, 80, Style::default());
        assert_eq!(plain_text(&line), "a🦀b日");
    }
}
