use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Widget, Wrap,
};

use super::state::{PagesPromptState, PendingAction};

pub struct PagesPromptWidget;

impl PagesPromptWidget {
    pub fn new() -> PagesPromptWidget {
        PagesPromptWidget
    }
}

impl Default for PagesPromptWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl StatefulWidget for PagesPromptWidget {
    type State = PagesPromptState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut PagesPromptState) {
        let popup = centered_rect(area);
        Clear.render(popup, buf);

        match state {
            PagesPromptState::Open { rows, selected } => {
                let items: Vec<ListItem> = rows
                    .iter()
                    .map(|name| ListItem::new(Line::from(format!("  {}", name.as_str()))))
                    .collect();
                let list = List::new(items)
                    .block(
                        Block::bordered().title(" open page — Enter open, d delete, Esc cancel "),
                    )
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
                let mut list_state = ListState::default();
                if !rows.is_empty() {
                    list_state.select(Some(*selected));
                }
                StatefulWidget::render(list, popup, buf, &mut list_state);
            }
            PagesPromptState::Discard {
                titles,
                then,
                error,
            } => {
                let is_delete = matches!(then, PendingAction::DeletePage(_));
                let mut text = if let PendingAction::DeletePage(name) = then {
                    format!("Delete {}? This cannot be undone.\n\n", name.as_str())
                } else {
                    String::from("Discard unsaved changes?\n\n")
                };
                for title in titles.iter() {
                    text.push_str("  • ");
                    text.push_str(title);
                    text.push('\n');
                }
                if let Some(error) = error {
                    text.push('\n');
                    text.push_str(error);
                    text.push('\n');
                }
                text.push('\n');
                text.push_str(if is_delete {
                    "y/Enter delete   n/Esc cancel"
                } else {
                    "s save   y/Enter discard   n/Esc cancel"
                });
                let title = if is_delete {
                    " delete page "
                } else {
                    " unsaved changes "
                };
                let block = Block::bordered().title(title);
                let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
                Widget::render(paragraph, popup, buf);
            }
            PagesPromptState::SaveAs {
                input,
                error,
                rename,
                ..
            } => {
                let title = if *rename {
                    " rename page — Enter confirm, Esc cancel "
                } else {
                    " save page as — Enter confirm, Esc cancel "
                };
                let mut text = format!("{input}_\n");
                if let Some(error) = error {
                    text.push('\n');
                    text.push_str(error);
                }
                let block = Block::bordered().title(title);
                let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
                Widget::render(paragraph, popup, buf);
            }
        }
    }
}

// Same shape as `ui::picker::widget::centered_rect` -- floored at 40x10 and
// clamped to `area` so this never asks for a rect larger than the frame.
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
