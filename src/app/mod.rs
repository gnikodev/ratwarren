pub mod keymap;
pub mod message;
pub mod worker;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::ui::grid::state::DataGridState;
use crate::ui::grid::widget::DataGridWidget;
use crate::ui::tree::model::NodeKey;
use crate::ui::tree::state::{ObjectTreeState, TreeCommand, TreeRowKey};
use crate::ui::tree::widget::ObjectTreeWidget;
use keymap::{AppCommand, map_key};
use message::{WorkerRequest, WorkerResponse};

const TREE_FOOTER: &str =
    "↑/↓ move  →/← expand/collapse  ⏎ open/toggle  r refresh  . system  Tab grid  q quit";
const GRID_FOOTER: &str = "↑/↓ move  ←/→ scroll cols  PgUp/PgDn page  n/p next/prev page  r refresh  Esc/Tab tree  q quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Grid,
}

pub struct App {
    tree: ObjectTreeState,
    grid: DataGridState,
    focus: Focus,
    requests: UnboundedSender<WorkerRequest>,
    responses: UnboundedReceiver<WorkerResponse>,
    connection_name: String,
    should_quit: bool,
}

impl App {
    pub fn new(
        connection_name: String,
        requests: UnboundedSender<WorkerRequest>,
        responses: UnboundedReceiver<WorkerResponse>,
    ) -> Self {
        Self {
            tree: ObjectTreeState::new(),
            grid: DataGridState::new(),
            focus: Focus::Tree,
            requests,
            responses,
            connection_name,
            should_quit: false,
        }
    }

    pub fn start(&mut self) {
        let req = self.tree.refresh_root();
        let _ = self.requests.send(WorkerRequest::Tree(req));
    }

    pub fn apply(&mut self, response: WorkerResponse) {
        match response {
            WorkerResponse::Tree(r) => self.tree.apply(r),
            WorkerResponse::Grid(r) => self.grid.apply(r),
        }
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        match map_key(key, self.focus) {
            Some(AppCommand::Quit) => self.should_quit = true,
            Some(AppCommand::ToggleFocus) => {
                if self.grid.is_open() {
                    self.focus = match self.focus {
                        Focus::Tree => Focus::Grid,
                        Focus::Grid => Focus::Tree,
                    };
                }
            }
            Some(AppCommand::FocusTree) => self.focus = Focus::Tree,
            Some(AppCommand::Activate) => self.activate(),
            Some(AppCommand::Tree(cmd)) => {
                if let Some(req) = self.tree.command(cmd) {
                    let _ = self.requests.send(WorkerRequest::Tree(req));
                }
            }
            Some(AppCommand::Grid(cmd)) => {
                if let Some(req) = self.grid.command(cmd) {
                    let _ = self.requests.send(WorkerRequest::Grid(req));
                }
            }
            None => {}
        }
    }

    fn activate(&mut self) {
        if let Some(row) = self.tree.selected()
            && let TreeRowKey::Node(NodeKey::Table { schema, table }) = &row.key
        {
            let (schema, table) = (schema.clone(), table.clone());
            let req = self.grid.open(schema, table);
            self.focus = Focus::Grid;
            let _ = self.requests.send(WorkerRequest::Grid(req));
            return;
        }
        if let Some(req) = self.tree.command(TreeCommand::Toggle) {
            let _ = self.requests.send(WorkerRequest::Tree(req));
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

        let tree_area;
        let grid_area;
        if self.grid.is_open() {
            let panes =
                Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
                    .split(chunks[0]);
            tree_area = panes[0];
            grid_area = Some(panes[1]);
        } else {
            tree_area = chunks[0];
            grid_area = None;
        }

        let tree_style = pane_border_style(self.focus == Focus::Tree);
        let tree_block = Block::bordered()
            .border_style(tree_style)
            .title(format!(" ratwarren — {} ", self.connection_name));
        let tree_widget = ObjectTreeWidget::new()
            .block(tree_block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(tree_widget, tree_area, &mut self.tree);

        if let Some(grid_area) = grid_area {
            let grid_style = pane_border_style(self.focus == Focus::Grid);
            let grid_block = Block::bordered().border_style(grid_style);
            let grid_widget = DataGridWidget::new()
                .block(grid_block)
                .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            frame.render_stateful_widget(grid_widget, grid_area, &mut self.grid);
        }

        let footer_text = match self.focus {
            Focus::Tree => TREE_FOOTER,
            Focus::Grid => GRID_FOOTER,
        };
        let footer = Paragraph::new(footer_text);
        frame.render_widget(footer, chunks[1]);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
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
                Some(r) => app.apply(r),
                None => return Err(std::io::Error::other("datasource worker stopped")),
            },
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}
