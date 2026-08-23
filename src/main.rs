use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::Alignment,
    widgets::{Block, Borders, Paragraph},
};

fn main() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    loop {
        terminal.draw(render)?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('q')
        {
            return Ok(());
        }
    }
}

fn render(frame: &mut Frame) {
    let block = Block::default().title(" ratwarren ").borders(Borders::ALL);
    let paragraph = Paragraph::new("ratwarren — press 'q' to quit")
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, frame.area());
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::render;

    fn rendered_lines(width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend init is infallible");
        terminal
            .draw(render)
            .expect("TestBackend draw is infallible");

        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).expect("in bounds").symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn shows_title_in_border_and_quit_hint_in_body() {
        let lines = rendered_lines(40, 3);

        assert!(
            lines[0].contains("ratwarren"),
            "top border should carry the ` ratwarren ` title, got: {:?}",
            lines[0]
        );
        let body = lines[1].trim_matches(|c: char| c == '│' || c.is_whitespace());
        assert_eq!(
            body, "ratwarren — press 'q' to quit",
            "body row should show the quit hint, got: {:?}",
            lines[1]
        );
    }

    #[test]
    fn survives_a_terminal_too_small_to_fit_the_text() {
        // 1x1 leaves no room for the border or the paragraph; render() must not panic
        // even though every widget it draws would need more space than it's given.
        let lines = rendered_lines(1, 1);
        assert_eq!(lines.len(), 1);
    }
}
