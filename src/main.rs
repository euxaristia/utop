use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    widgets::{Block, Borders, Cell, Clear, Gauge, LineGauge, Paragraph, Row, Sparkline, Table},
};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};
use sysinfo::System;

const TICK_RATE: Duration = Duration::from_millis(1000);
const MAX_CPU_HISTORY: usize = 240;

#[derive(Clone)]
struct ProcessRowData {
    pid: String,
    name: String,
    cpu_percent: f32,
    mem_percent: f64,
}

struct App {
    system: System,
    last_tick: Instant,
    cpu_history: VecDeque<u64>,
    process_rows: Vec<ProcessRowData>,
    selected_process: usize,
    process_scroll: usize,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            system: System::new_all(),
            last_tick: Instant::now(),
            cpu_history: VecDeque::with_capacity(MAX_CPU_HISTORY),
            process_rows: Vec::new(),
            selected_process: 0,
            process_scroll: 0,
        };
        app.update();
        app
    }

    fn update(&mut self) {
        self.system.refresh_all();

        let cpu_usage = self.system.global_cpu_usage().clamp(0.0, 100.0);
        self.cpu_history.push_back(cpu_usage as u64);
        while self.cpu_history.len() > MAX_CPU_HISTORY {
            self.cpu_history.pop_front();
        }

        let total_mem = self.system.total_memory().max(1) as f64;
        let mut rows: Vec<ProcessRowData> = self
            .system
            .processes()
            .values()
            .map(|p| ProcessRowData {
                pid: p.pid().to_string(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_percent: p.cpu_usage().clamp(0.0, 100.0),
                mem_percent: (p.memory() as f64 / total_mem) * 100.0,
            })
            .collect();

        rows.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then_with(|| b.mem_percent.total_cmp(&a.mem_percent))
        });

        self.process_rows = rows;
        self.clamp_selection();
    }

    fn process_count(&self) -> usize {
        self.process_rows.len()
    }

    fn clamp_selection(&mut self) {
        let count = self.process_count();
        if count == 0 {
            self.selected_process = 0;
            self.process_scroll = 0;
            return;
        }
        if self.selected_process >= count {
            self.selected_process = count - 1;
        }
        if self.process_scroll >= count {
            self.process_scroll = self.selected_process;
        }
    }

    fn next_process(&mut self) {
        let count = self.process_count();
        if count == 0 {
            return;
        }
        self.selected_process = (self.selected_process + 1).min(count - 1);
    }

    fn previous_process(&mut self) {
        if self.process_count() == 0 {
            return;
        }
        self.selected_process = self.selected_process.saturating_sub(1);
    }

    fn page_down(&mut self, page_size: usize) {
        let count = self.process_count();
        if count == 0 {
            return;
        }
        self.selected_process = (self.selected_process + page_size.max(1)).min(count - 1);
    }

    fn page_up(&mut self, page_size: usize) {
        if self.process_count() == 0 {
            return;
        }
        self.selected_process = self.selected_process.saturating_sub(page_size.max(1));
    }

    fn jump_top(&mut self) {
        self.selected_process = 0;
    }

    fn jump_bottom(&mut self) {
        let count = self.process_count();
        if count > 0 {
            self.selected_process = count - 1;
        }
    }

    fn align_scroll_to_selection(&mut self, visible_rows: usize) {
        let page = visible_rows.max(1);
        if self.selected_process < self.process_scroll {
            self.process_scroll = self.selected_process;
        } else if self.selected_process >= self.process_scroll + page {
            self.process_scroll = self.selected_process + 1 - page;
        }
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("{err:?}");
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        terminal.draw(|f| ui(f, app))?;

        let timeout = TICK_RATE
            .checked_sub(app.last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                let page_size = terminal.size()?.height.saturating_sub(12) as usize;
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.next_process(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous_process(),
                    KeyCode::PageDown => app.page_down(page_size),
                    KeyCode::PageUp => app.page_up(page_size),
                    KeyCode::Home => app.jump_top(),
                    KeyCode::End => app.jump_bottom(),
                    _ => {}
                }
            }
        }

        if app.last_tick.elapsed() >= TICK_RATE {
            app.update();
            app.last_tick = Instant::now();
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let cpu_usage = app.system.global_cpu_usage().clamp(0.0, 100.0);
    let refresh_left_ms = TICK_RATE
        .saturating_sub(app.last_tick.elapsed())
        .as_millis()
        .min(9999);
    let topbar = Paragraph::new(format!(
        " rtop | CPU {:>5.1}% | procs {:>4} | refresh {:>4}ms ",
        cpu_usage,
        app.process_count(),
        refresh_left_ms
    ))
    .style(
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(topbar, root[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(root[1]);

    let left_panels = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Min(0),
        ])
        .split(body[0]);

    let cpu_panel = LineGauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" CPU ")
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .filled_style(Style::default().fg(Color::LightGreen))
        .filled_symbol(symbols::line::THICK_HORIZONTAL)
        .ratio((cpu_usage as f64 / 100.0).clamp(0.0, 1.0));
    f.render_widget(cpu_panel, left_panels[0]);

    let cpu_points: Vec<u64> = app.cpu_history.iter().copied().collect();
    let cpu_history = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" CPU History ")
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .style(Style::default().fg(Color::Yellow))
        .max(100)
        .data(&cpu_points);
    f.render_widget(cpu_history, left_panels[1]);

    let total_mem = app.system.total_memory();
    let used_mem = app.system.used_memory();
    let mem_ratio = if total_mem > 0 {
        used_mem as f64 / total_mem as f64
    } else {
        0.0
    };
    let mem_panel = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Memory ")
                .border_style(Style::default().fg(Color::Green)),
        )
        .gauge_style(Style::default().fg(Color::Green))
        .label(format!(
            " {:.2} / {:.2} GiB ({:.1}%) ",
            used_mem as f64 / 1_073_741_824.0,
            total_mem as f64 / 1_073_741_824.0,
            mem_ratio * 100.0
        ))
        .ratio(mem_ratio.clamp(0.0, 1.0));
    f.render_widget(mem_panel, left_panels[2]);

    let table_visible_rows = body[1].height.saturating_sub(3) as usize;
    app.align_scroll_to_selection(table_visible_rows);

    let start = app.process_scroll.min(app.process_count());
    let end = (start + table_visible_rows).min(app.process_count());

    let header = Row::new(
        ["Pid", "Program", "Cpu%", "Mem%"]
            .into_iter()
            .map(Cell::from),
    )
    .style(
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows = app.process_rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let absolute_index = start + i;
            let mut base_style = if i % 2 == 0 {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::White)
            };
            if absolute_index == app.selected_process {
                base_style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
            }

            let cpu_style = if p.cpu_percent >= 60.0 {
                Style::default().fg(Color::Red)
            } else if p.cpu_percent >= 20.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::LightGreen)
            };

            Row::new(vec![
                Cell::from(p.pid.clone()),
                Cell::from(p.name.clone()),
                Cell::from(format!("{:>5.1}", p.cpu_percent)).style(cpu_style),
                Cell::from(format!("{:>5.1}", p.mem_percent)),
            ])
            .style(base_style)
        });

    let process_title = format!(
        " Processes [{}/{}] ",
        if app.process_count() == 0 {
            0
        } else {
            app.selected_process + 1
        },
        app.process_count()
    );

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(15),
            Constraint::Percentage(55),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(process_title)
            .border_style(Style::default().fg(Color::Magenta)),
    );

    f.render_widget(Clear, body[1]);
    f.render_widget(table, body[1]);

    let footer =
        Paragraph::new(" q quit | Ctrl+C quit | j/k move | PgUp/PgDn scroll | Home/End jump ")
            .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, root[2]);
}
