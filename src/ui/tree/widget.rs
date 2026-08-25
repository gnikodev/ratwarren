use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, StatefulWidget};

use crate::datasource::TableKind;
use crate::ui::tree::state::{ObjectTreeState, StatusKind, TreeRow, TreeRowKind};

pub struct ObjectTreeWidget<'a> {
    block: Option<Block<'a>>,
    highlight_style: Style,
}

impl<'a> Default for ObjectTreeWidget<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> ObjectTreeWidget<'a> {
    pub fn new() -> Self {
        Self {
            block: None,
            highlight_style: Style::default(),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn highlight_style<S: Into<Style>>(mut self, style: S) -> Self {
        self.highlight_style = style.into();
        self
    }
}

impl StatefulWidget for ObjectTreeWidget<'_> {
    type State = ObjectTreeState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut ObjectTreeState) {
        let inner = self.block.as_ref().map_or(area, |b| b.inner(area));
        state.set_viewport_height(inner.height.max(1));

        let items: Vec<_> = state.rows().iter().map(row_to_item).collect();
        let mut list = List::new(items).highlight_style(self.highlight_style);
        if let Some(block) = self.block {
            list = list.block(block);
        }
        StatefulWidget::render(list, area, buf, state.list_state_mut());
    }
}

fn row_to_item(row: &TreeRow) -> ListItem<'static> {
    let line = match &row.kind {
        TreeRowKind::Schema { name, expanded } => {
            let arrow = if *expanded { '▾' } else { '▸' };
            Line::from(format!("{arrow} {name}"))
        }
        TreeRowKind::Table {
            name,
            kind,
            expanded,
        } => {
            let arrow = if *expanded { '▾' } else { '▸' };
            let mut spans = vec![Span::raw(format!("  {arrow} {name}"))];
            let suffix = table_kind_suffix(*kind);
            if !suffix.is_empty() {
                spans.push(Span::styled(
                    suffix,
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        }
        TreeRowKind::Column {
            name,
            data_type,
            is_nullable,
            is_primary_key,
        } => {
            let mut tags = format!(" {data_type}");
            if !is_nullable {
                tags.push_str(" NOT NULL");
            }
            if *is_primary_key {
                tags.push_str(" PK");
            }
            Line::from(vec![
                Span::raw(format!("      {name}")),
                Span::styled(tags, Style::default().add_modifier(Modifier::DIM)),
            ])
        }
        TreeRowKind::Status(status) => {
            let indent = "  ".repeat((row.depth + 1) as usize);
            match status {
                StatusKind::Loading => Line::from(format!("{indent}loading…")),
                StatusKind::Empty => Line::from(format!("{indent}(empty)")),
                StatusKind::Error(message) => Line::from(Span::styled(
                    format!("{indent}✗ {}", crate::ui::first_line(message)),
                    Style::default().fg(Color::Red),
                )),
            }
        }
    };
    ListItem::new(line)
}

fn table_kind_suffix(kind: TableKind) -> &'static str {
    match kind {
        TableKind::Table => "",
        TableKind::View => " [view]",
        TableKind::MaterializedView => " [mview]",
        TableKind::PartitionedTable => " [partitioned]",
        TableKind::ForeignTable => " [foreign]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::Schema;
    use crate::ui::tree::message::{TreeRequest, TreeResponse};
    use crate::ui::tree::model::NodeKey;
    use crate::ui::tree::state::{TreeCommand, TreeRowKey};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    #[test]
    fn schema_row_prefix_reflects_expansion() {
        let row = TreeRow {
            key: TreeRowKey::Node(NodeKey::Schema {
                schema: "public".into(),
            }),
            depth: 0,
            kind: TreeRowKind::Schema {
                name: "public".into(),
                expanded: true,
            },
        };
        let item = row_to_item(&row);
        assert!(format!("{item:?}").contains("▾ public"));
    }

    #[test]
    fn status_error_row_shows_first_line_only() {
        let row = TreeRow {
            key: TreeRowKey::Status(None),
            depth: 0,
            kind: TreeRowKind::Status(StatusKind::Error("boom\nmore detail".into())),
        };
        let item = row_to_item(&row);
        let debug = format!("{item:?}");
        assert!(debug.contains("boom"));
        assert!(!debug.contains("more detail"));
    }

    // Sanity check that ObjectTreeWidget survives a degenerate 1x1 render
    // area (mirrors Phase 0's too-small-terminal coverage, now against the
    // real widget instead of the placeholder screen).
    #[test]
    fn render_survives_a_one_by_one_area() {
        let mut state = ObjectTreeState::new();
        let backend = TestBackend::new(1, 1);
        let mut terminal = Terminal::new(backend).expect("TestBackend init is infallible");
        terminal
            .draw(|f| {
                let widget = ObjectTreeWidget::new().block(Block::bordered());
                f.render_stateful_widget(widget, f.area(), &mut state);
            })
            .expect("rendering into a 1x1 area must not panic");
    }

    fn buffer_text(buf: &Buffer) -> String {
        buf.content
            .chunks(buf.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Regression test for a bug found in manual testing: rebuilding rows
    // after a scroll-deep selection, where the row count shrinks (e.g.
    // toggling off system schemas) and the previous selection doesn't
    // survive as an exact key match, left the `ListState` offset from
    // before the shrink in place. `List`'s render-time scroll logic clamps
    // a stale offset to `rows.len() - 1` rather than backing it off to fit
    // the viewport, so only the single row at that clamped offset was
    // rendered even though the shrunk row set still had several rows.
    #[test]
    fn rebuild_resets_offset_when_row_count_shrinks_past_stale_scroll_position() {
        let mut state = ObjectTreeState::new();
        let TreeRequest::Schemas { id } = state.refresh_root() else {
            unreachable!("refresh_root always issues a Schemas request")
        };

        let mut schemas: Vec<Schema> = (0..5)
            .map(|i| Schema {
                name: format!("keep{i}"),
                is_system: false,
            })
            .collect();
        schemas.extend((0..20).map(|i| Schema {
            name: format!("sys{i}"),
            is_system: true,
        }));
        state.apply(TreeResponse::Schemas {
            id,
            result: Ok(schemas),
        });

        // System schemas are hidden by default; show them so the row set is
        // large enough (25 rows) to scroll past a 10-row viewport.
        assert!(state.command(TreeCommand::ToggleSystemSchemas).is_none());
        assert_eq!(state.rows().len(), 25);

        for _ in 0..20 {
            assert!(state.command(TreeCommand::MoveDown).is_none());
        }
        assert_eq!(state.selected_index(), Some(20));

        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init is infallible");
        terminal
            .draw(|f| {
                let widget = ObjectTreeWidget::new();
                f.render_stateful_widget(widget, f.area(), &mut state);
            })
            .expect("first render must not panic");
        // Sanity check: the deep selection actually scrolled the viewport
        // past the first row, so the regression this test guards against
        // (a stale offset from a genuinely scrolled position) is reachable.
        assert!(
            !buffer_text(terminal.backend().buffer()).contains("keep0"),
            "test setup must scroll far enough that the first row is off-screen"
        );

        // Shrinks the row set back down to the 5 non-system schemas. The
        // previously selected row (a system schema, index 20) no longer
        // exists and has no fallback key, so `restore_selection` clamps to
        // `previous_index.min(rows.len() - 1)` == the last surviving row.
        assert!(state.command(TreeCommand::ToggleSystemSchemas).is_none());
        assert_eq!(state.rows().len(), 5);

        terminal
            .draw(|f| {
                let widget = ObjectTreeWidget::new();
                f.render_stateful_widget(widget, f.area(), &mut state);
            })
            .expect("second render must not panic");

        let content = buffer_text(terminal.backend().buffer());
        assert!(
            content.contains("keep0"),
            "the first surviving row must still be visible after the shrink \
             (offset must be reset, not left stale from before the rebuild); \
             rendered buffer:\n{content}"
        );
    }
}
