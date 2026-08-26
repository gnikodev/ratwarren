use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Paragraph, Row, StatefulWidget, Table, TableState, Widget};

use crate::ui::Load;
use crate::ui::grid::page::Page;
use crate::ui::grid::state::DataGridState;

const MIN_COL_WIDTH: u16 = 3;
const MAX_COL_WIDTH: u16 = 40;
const COLUMN_SPACING: u16 = 1;
const NULL_DISPLAY: &str = "NULL";

pub struct DataGridWidget<'a> {
    block: Option<Block<'a>>,
    row_highlight_style: Style,
}

impl<'a> Default for DataGridWidget<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> DataGridWidget<'a> {
    pub fn new() -> Self {
        Self {
            block: None,
            row_highlight_style: Style::default(),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn row_highlight_style<S: Into<Style>>(mut self, style: S) -> Self {
        self.row_highlight_style = style.into();
        self
    }
}

impl StatefulWidget for DataGridWidget<'_> {
    type State = DataGridState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut DataGridState) {
        let block = attach_titles(self.block.unwrap_or_default(), state);
        let inner = block.inner(area);
        state.set_viewport_height(inner.height.saturating_sub(1).max(1));

        let (page_load, table_state, col_offset) = state.parts();

        let page = match page_load {
            Load::NotLoaded | Load::Loading { .. } => {
                render_message(block, area, buf, "loading…", Style::default());
                return;
            }
            Load::Failed { message } => {
                let text = format!("✗ {}", crate::ui::first_line(message));
                render_message(block, area, buf, &text, Style::default().fg(Color::Red));
                return;
            }
            Load::Loaded(page) if page.is_empty() => {
                render_message(block, area, buf, "(no rows)", Style::default());
                return;
            }
            Load::Loaded(page) => page,
        };
        render_table(
            page,
            self.row_highlight_style,
            block,
            inner,
            area,
            buf,
            table_state,
            col_offset,
        );
    }
}

fn attach_titles<'a>(block: Block<'a>, state: &DataGridState) -> Block<'a> {
    let Some((schema, table)) = state.target() else {
        return block;
    };
    let top = format!(" {schema}.{table} ");

    let bottom = match state.page() {
        Load::Loaded(page) => {
            let offset = state.offset();
            let mut hints = vec!["(unordered)".to_string()];
            if offset > 0 {
                hints.push("[p prev]".to_string());
            }
            if page.has_next {
                hints.push("[n next]".to_string());
            }
            let range = if page.rows.is_empty() {
                "rows 0".to_string()
            } else {
                let start = offset + 1;
                let end = offset + page.rows.len() as u64;
                format!("rows {start}-{end}")
            };
            format!(" {range} · {} ", hints.join(" · "))
        }
        _ => " (unordered) ".to_string(),
    };

    block.title(top).title_bottom(bottom)
}

fn render_message(block: Block<'_>, area: Rect, buf: &mut Buffer, text: &str, style: Style) {
    Paragraph::new(text)
        .style(style)
        .block(block)
        .render(area, buf);
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    page: &Page,
    row_highlight_style: Style,
    block: Block<'_>,
    inner: Rect,
    area: Rect,
    buf: &mut Buffer,
    table_state: &mut TableState,
    col_offset: usize,
) {
    let widths = column_widths(page);
    let range = visible_range(&widths, col_offset, inner.width, COLUMN_SPACING);

    let header = Row::new(
        page.columns[range.clone()]
            .iter()
            .map(|c| Cell::new(sanitize_cell(c))),
    )
    .style(Style::default().add_modifier(Modifier::BOLD));

    // `row.get(i)` rather than `row[i]`: `Page` is a `pub` struct with `pub`
    // fields, so a malformed row shorter than `page.columns` (not producible
    // by `fetch_page` today, but not statically ruled out either) must
    // render as a blank cell instead of panicking the whole TUI. A missing
    // cell is deliberately rendered as blank, not as `NULL_DISPLAY`, so it
    // stays visually distinguishable from an actual SQL NULL.
    let rows = page.rows.iter().map(|row| {
        Row::new(range.clone().map(|i| match row.get(i) {
            Some(Some(text)) => Cell::new(sanitize_cell(text)),
            Some(None) => Cell::new(Span::styled(
                NULL_DISPLAY,
                Style::default().add_modifier(Modifier::DIM),
            )),
            None => Cell::new(""),
        }))
    });

    let constraints: Vec<Constraint> = widths[range.clone()]
        .iter()
        .map(|w| Constraint::Length(*w))
        .collect();

    let table = Table::new(rows, constraints)
        .header(header)
        .row_highlight_style(row_highlight_style)
        .column_spacing(COLUMN_SPACING)
        .block(block);

    StatefulWidget::render(table, area, buf, table_state);
}

fn column_widths(page: &Page) -> Vec<u16> {
    page.columns
        .iter()
        .enumerate()
        .map(|(i, header)| {
            let header_width = Span::raw(header.as_str()).width();
            let cell_width = page
                .rows
                .iter()
                .map(|row| match row.get(i) {
                    Some(Some(text)) => Span::raw(sanitize_cell(text)).width(),
                    Some(None) => NULL_DISPLAY.len(),
                    None => 0,
                })
                .max()
                .unwrap_or(0);
            header_width
                .max(cell_width)
                .clamp(MIN_COL_WIDTH as usize, MAX_COL_WIDTH as usize) as u16
        })
        .collect()
}

fn visible_range(
    widths: &[u16],
    col_offset: usize,
    available: u16,
    spacing: u16,
) -> std::ops::Range<usize> {
    if widths.is_empty() {
        return 0..0;
    }
    let start = col_offset.min(widths.len() - 1);
    let mut used: u32 = widths[start] as u32;
    let mut end = start + 1;
    for (i, w) in widths.iter().enumerate().skip(end) {
        let additional = spacing as u32 + *w as u32;
        if used + additional > available as u32 {
            break;
        }
        used += additional;
        end = i + 1;
    }
    start..end
}

fn sanitize_cell(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::grid::message::{GridRequest, GridResponse};
    use crate::ui::grid::state::DataGridState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn page(columns: &[&str], rows: Vec<Vec<Option<&str>>>) -> Page {
        Page {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(|v| v.map(str::to_string)).collect())
                .collect(),
            has_next: false,
        }
    }

    #[test]
    fn column_widths_uses_header_when_wider_than_cells() {
        let p = page(&["identifier"], vec![vec![Some("1")]]);
        assert_eq!(column_widths(&p), vec!["identifier".len() as u16]);
    }

    #[test]
    fn column_widths_uses_widest_cell_when_wider_than_header() {
        let long = "x".repeat(MAX_COL_WIDTH as usize + 10);
        let p = page(&["a"], vec![vec![Some(long.as_str())]]);
        assert_eq!(column_widths(&p)[0], MAX_COL_WIDTH);
    }

    #[test]
    fn column_widths_respects_minimum() {
        let p = page(&["a"], vec![vec![Some("x")]]);
        assert_eq!(column_widths(&p)[0], MIN_COL_WIDTH);
    }

    #[test]
    fn column_widths_accounts_for_null_display() {
        let p = page(&["a"], vec![vec![None]]);
        assert_eq!(column_widths(&p)[0], NULL_DISPLAY.len() as u16);
    }

    // A short row (fewer cells than columns, not producible by fetch_page
    // today but not statically ruled out) must be treated as blank/absent
    // for width purposes, not as a NULL — otherwise a defensively-blank
    // cell would skew the column width as if it were the "NULL" placeholder
    // text, even when every other row's actual data is much shorter.
    #[test]
    fn column_widths_treats_a_missing_cell_as_blank_not_null() {
        let p = Page {
            columns: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec![Some("x".to_string())], vec![Some("y".to_string())]],
            has_next: false,
        };
        assert_eq!(
            column_widths(&p)[1],
            MIN_COL_WIDTH,
            "a column with only absent cells (short rows) must fall back to the minimum width, \
             not NULL_DISPLAY's length"
        );
    }

    #[test]
    fn visible_range_includes_columns_that_fit() {
        let widths = [5u16, 5, 5, 5];
        let range = visible_range(&widths, 0, 16, 1);
        // 5 + 1+5 + 1+5 = 17 > 16, so only first two columns fit alongside
        // the third partially exceeding budget.
        assert_eq!(range, 0..2);
    }

    #[test]
    fn visible_range_always_includes_at_least_one_column() {
        let widths = [40u16, 40, 40];
        let range = visible_range(&widths, 0, 5, 1);
        assert_eq!(range, 0..1);
    }

    #[test]
    fn visible_range_starts_at_col_offset() {
        let widths = [5u16, 5, 5, 5];
        let range = visible_range(&widths, 2, 100, 1);
        assert_eq!(range, 2..4);
    }

    #[test]
    fn visible_range_clamps_offset_past_last_column() {
        let widths = [5u16, 5];
        let range = visible_range(&widths, 10, 100, 1);
        assert_eq!(range, 1..2);
    }

    #[test]
    fn sanitize_cell_replaces_control_characters() {
        assert_eq!(sanitize_cell("a\nb\tc"), "a·b·c");
    }

    #[test]
    fn sanitize_cell_leaves_normal_text_untouched() {
        assert_eq!(sanitize_cell("hello world"), "hello world");
    }

    fn buffer_text(buf: &Buffer) -> String {
        buf.content
            .chunks(buf.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Confirms the NULL/empty-string distinction survives all the way to the
    // rendered buffer, not just at the Page/state level: a SQL NULL must
    // render the "NULL" placeholder while an empty string renders as nothing
    // (an empty cell), and the two must not be visually indistinguishable.
    #[test]
    fn rendered_buffer_shows_null_placeholder_only_for_the_null_row() {
        let mut state = DataGridState::new();
        let GridRequest::Page { id, .. } = state.open("public".into(), "t".into());
        state.apply(GridResponse::Page {
            id,
            schema: "public".into(),
            table: "t".into(),
            offset: 0,
            result: Ok(Page {
                columns: vec!["a".into()],
                rows: vec![vec![None], vec![Some(String::new())]],
                has_next: false,
            }),
        });

        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).expect("TestBackend init is infallible");
        terminal
            .draw(|f| {
                let widget = DataGridWidget::new();
                f.render_stateful_widget(widget, f.area(), &mut state);
            })
            .expect("render must not panic");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains(NULL_DISPLAY),
            "the NULL row should render the NULL placeholder, got:\n{text}"
        );
        assert_eq!(
            text.matches(NULL_DISPLAY).count(),
            1,
            "NULL placeholder should render exactly once (for the None row only), not once per \
             row, got:\n{text}"
        );
    }
}
