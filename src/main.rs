use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    widgets::{Block, Borders, Cell, Gauge, LineGauge, Paragraph, Row, Sparkline, Table, TableState},
    Frame, Terminal,
};
use std::{
    io,
    time::{Duration, Instant},
};
use sysinfo::System;

struct App {
    system: System,
    last_tick: Instant,
    cpu_history: Vec<u64>,
    process_state: TableState,
    process_count: usize,
}

impl App {
    fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        Self {
            system,
            last_tick: Instant::now(),
            cpu_history: Vec::new(),
            process_state: TableState::default(),
            process_count: 0,
        }
    }

    fn update(&mut self) {
        self.system.refresh_all();
        let cpu_usage = self.system.global_cpu_usage() as u64;
        self.cpu_history.push(cpu_usage);
        if self.cpu_history.len() > 1000 {
            self.cpu_history.remove(0);
        }
        self.process_count = self.system.processes().len();
    }

    fn next_process(&mut self) {
        let i = match self.process_state.selected() {
            Some(i) => {
                if i >= self.process_count - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.process_state.select(Some(i));
    }

    fn previous_process(&mut self) {
        let i = match self.process_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.process_count - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.process_state.select(Some(i));
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;
    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // create app and run it
    let mut app = App::new();
    let res = run_app(&mut terminal, &mut app);

    // restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("{:?}", err);
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(1000);
    loop {
        terminal.draw(|f| ui(f, app))?;

        let timeout = tick_rate
            .checked_sub(app.last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => app.next_process(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous_process(),
                    _ => {}
                }
            }
        }

        if app.last_tick.elapsed() >= tick_rate {
            app.update();
            app.last_tick = Instant::now();
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // CPU Gauge
            Constraint::Length(3), // CPU Sparkline
            Constraint::Length(3), // Memory
            Constraint::Min(0),    // Processes
            Constraint::Length(1), // Help
        ])
        .split(f.area());

    // CPU Usage Gauge
    let cpu_usage = app.system.global_cpu_usage();
    let cpu_gauge = LineGauge::default()
        .block(Block::default().borders(Borders::ALL).title(" CPU Usage "))
        .filled_style(Style::default().fg(Color::Cyan))
        .filled_symbol(symbols::line::THICK_VERTICAL)
        .ratio(cpu_usage as f64 / 100.0);
    f.render_widget(cpu_gauge, chunks[0]);

    // CPU Sparkline
    let sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" CPU History "))
        .data(&app.cpu_history)
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(sparkline, chunks[1]);

    // Memory Usage
    let total_mem = app.system.total_memory();
    let used_mem = app.system.used_memory();
    let mem_ratio = if total_mem > 0 {
        used_mem as f64 / total_mem as f64
    } else {
        0.0
    };
    let mem_label = format!(
        " {:.2} / {:.2} GB ({:.1}%)",
        used_mem as f64 / 1_073_741_824.0,
        total_mem as f64 / 1_073_741_824.0,
        mem_ratio * 100.0
    );
    let mem_gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Memory "))
        .gauge_style(Style::default().fg(Color::Green))
        .label(mem_label)
        .ratio(mem_ratio);
    f.render_widget(mem_gauge, chunks[2]);

    // Processes
    let mut processes: Vec<_> = app.system.processes().values().collect();
    processes.sort_by(|a, b| b.cpu_usage().partial_cmp(&a.cpu_usage()).unwrap());

    let header_cells = ["PID", "Name", "CPU %", "Mem %"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells)
        .style(Style::default().bg(Color::Blue))
        .height(1);

    let rows = processes.iter().map(|p| {
        let cells = vec!(
            Cell::from(p.pid().to_string()),
            Cell::from(p.name().to_string_lossy().into_owned()),
            Cell::from(format!("{:.1}", p.cpu_usage())),
            Cell::from(format!(
                "{:.1}",
                (p.memory() as f64 / total_mem as f64) * 100.0
            )),
        );
        Row::new(cells)
    });

    let t = Table::new(
        rows,
        [
            Constraint::Percentage(10),
            Constraint::Percentage(50),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Processes "))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(t, chunks[3], &mut app.process_state);

    let help_message = Paragraph::new("Quit: q | Move: ↑/↓ or j/k")
        .style(Style::default().fg(Color::Gray));
    f.render_widget(help_message, chunks[4]);
}
