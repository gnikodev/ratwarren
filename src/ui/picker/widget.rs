use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, StatefulWidget, Widget};

use super::state::{PickerRow, PickerState};

pub struct PickerWidget;

impl PickerWidget {
    pub fn new() -> PickerWidget {
        PickerWidget
    }
}

impl Default for PickerWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl StatefulWidget for PickerWidget {
    type State = PickerState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut PickerState) {
        let popup = centered_rect(area);
        Clear.render(popup, buf);

        let items: Vec<ListItem> = state.rows().iter().map(row_to_item).collect();
        let list = List::new(items)
            .block(Block::bordered().title(" open connection "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        StatefulWidget::render(list, popup, buf, state.list_state_mut());
    }
}

fn row_to_item(row: &PickerRow) -> ListItem<'static> {
    match row {
        PickerRow::GroupHeader { label } => {
            let text = label.clone().unwrap_or_default();
            ListItem::new(Line::styled(
                text,
                Style::default().add_modifier(Modifier::DIM),
            ))
        }
        PickerRow::Connection { name } => ListItem::new(Line::from(format!("  {name}"))),
    }
}

// Centred at ~60% x 60% of `area`, floored at 40x10 and clamped to `area` so
// this can never ask for a rect larger than the frame -- required to not
// panic on a tiny terminal.
fn centered_rect(area: Rect) -> Rect {
    let width = ((area.width as u32 * 60) / 100)
        .max(40)
        .min(area.width as u32) as u16;
    let height = ((area.height as u32 * 60) / 100)
        .max(10)
        .min(area.height as u32) as u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_within(area: Rect, popup: Rect) {
        assert!(
            popup.x >= area.x
                && popup.y >= area.y
                && popup.x + popup.width <= area.x + area.width
                && popup.y + popup.height <= area.y + area.height,
            "popup {popup:?} must stay within area {area:?}"
        );
    }

    #[test]
    fn centered_rect_never_exceeds_a_tiny_frame() {
        for (w, h) in [(0u16, 0u16), (1, 1), (5, 3), (10, 5), (39, 9)] {
            let area = Rect::new(0, 0, w, h);
            let popup = centered_rect(area);
            assert_within(area, popup);
        }
    }

    #[test]
    fn centered_rect_floors_at_40x10_on_a_large_enough_frame() {
        let area = Rect::new(0, 0, 200, 100);
        let popup = centered_rect(area);
        assert!(popup.width >= 40);
        assert!(popup.height >= 10);
        assert_within(area, popup);
    }

    #[test]
    fn centered_rect_is_actually_centered() {
        let area = Rect::new(0, 0, 100, 50);
        let popup = centered_rect(area);
        let left_margin = popup.x - area.x;
        let right_margin = (area.x + area.width) - (popup.x + popup.width);
        assert!(
            left_margin.abs_diff(right_margin) <= 1,
            "left margin {left_margin} and right margin {right_margin} should be within 1 of \
             each other"
        );
    }
}
