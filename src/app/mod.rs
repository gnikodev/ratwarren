pub mod keymap;
pub mod worker;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::ui::tree::message::{TreeRequest, TreeResponse};
use crate::ui::tree::state::ObjectTreeState;
use crate::ui::tree::widget::ObjectTreeWidget;
use keymap::{AppCommand, map_key};

const FOOTER: &str = "↑/↓ move  →/← expand/collapse  ⏎ toggle  r refresh  . system  q quit";

pub struct App {
    tree: ObjectTreeState,
    requests: UnboundedSender<TreeRequest>,
    responses: UnboundedReceiver<TreeResponse>,
    connection_name: String,
    should_quit: bool,
}

impl App {
    pub fn new(
        connection_name: String,
        requests: UnboundedSender<TreeRequest>,
        responses: UnboundedReceiver<TreeResponse>,
    ) -> Self {
        Self {
            tree: ObjectTreeState::new(),
            requests,
            responses,
            connection_name,
            should_quit: false,
        }
    }

    pub fn start(&mut self) {
        let req = self.tree.refresh_root();
        let _ = self.requests.send(req);
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        match map_key(key) {
            Some(AppCommand::Quit) => self.should_quit = true,
            Some(AppCommand::Tree(cmd)) => {
                if let Some(req) = self.tree.command(cmd) {
                    let _ = self.requests.send(req);
                }
            }
            None => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

        let block = Block::bordered().title(format!(" ratwarren — {} ", self.connection_name));
        let widget = ObjectTreeWidget::new()
            .block(block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(widget, chunks[0], &mut self.tree);

        let footer = Paragraph::new(FOOTER);
        frame.render_widget(footer, chunks[1]);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

pub async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    use futures_util::StreamExt;

    let mut events = crossterm::event::EventStream::new();
    app.start();
    loop {
        terminal.draw(|f| app.render(f))?;

        tokio::select! {
            event = events.next() => match event {
                Some(Ok(crossterm::event::Event::Key(k)))
                    if k.kind == crossterm::event::KeyEventKind::Press =>
                {
                    app.on_key(k)
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            response = app.responses.recv() => match response {
                Some(r) => app.tree.apply(r),
                None => return Err(std::io::Error::other("datasource worker stopped")),
            },
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}
