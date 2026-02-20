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

const MAX_CPU_HISTORY: usize = 240;
const MIN_TICK_MS: u64 = 200;
const MAX_TICK_MS: u64 = 5000;
const TICK_STEP_MS: u64 = 100;

#[derive(Clone, Copy)]
enum SortMode {
    Cpu,
    Mem,
    Pid,
    Name,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Pid => "pid",
            Self::Name => "name",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Cpu => Self::Mem,
            Self::Mem => Self::Pid,
            Self::Pid => Self::Name,
            Self::Name => Self::Cpu,
        }
    }
}

#[derive(Clone)]
struct ProcessRowData {
    pid: String,
    pid_num: u32,
    name: String,
    cpu_total_percent: f32,
    mem_percent: f64,
}

enum InputMode {
    Normal,
    Filter,
}

struct App {
    system: System,
    last_tick: Instant,
    tick_rate: Duration,
    cpu_count: usize,
    cpu_history: VecDeque<u64>,
    all_process_rows: Vec<ProcessRowData>,
    process_rows: Vec<ProcessRowData>,
    selected_process: usize,
    process_scroll: usize,
    sort_mode: SortMode,
    sort_reverse: bool,
    filter_query: String,
    input_mode: InputMode,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            system: System::new_all(),
            last_tick: Instant::now(),
            tick_rate: Duration::from_millis(1000),
            cpu_count: 1,
            cpu_history: VecDeque::with_capacity(MAX_CPU_HISTORY),
            all_process_rows: Vec::new(),
            process_rows: Vec::new(),
            selected_process: 0,
            process_scroll: 0,
            sort_mode: SortMode::Cpu,
            sort_reverse: false,
            filter_query: String::new(),
            input_mode: InputMode::Normal,
        };
        app.update();
        app
    }

    fn update(&mut self) {
        self.system.refresh_all();
        self.cpu_count = self.system.cpus().len().max(1);

        let cpu_usage = self.system.global_cpu_usage().clamp(0.0, 100.0);
        self.cpu_history.push_back(cpu_usage as u64);
        while self.cpu_history.len() > MAX_CPU_HISTORY {
            self.cpu_history.pop_front();
        }

        let total_mem = self.system.total_memory().max(1) as f64;
        self.all_process_rows = self
            .system
            .processes()
            .values()
            .map(|p| ProcessRowData {
                pid: p.pid().to_string(),
                pid_num: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_total_percent: (p.cpu_usage() / self.cpu_count as f32).clamp(0.0, 100.0),
                mem_percent: (p.memory() as f64 / total_mem * 100.0).clamp(0.0, 100.0),
            })
            .collect();

        self.rebuild_process_rows();
    }

    fn rebuild_process_rows(&mut self) {
        let selected_pid = self
            .process_rows
            .get(self.selected_process)
            .map(|p| p.pid_num);

        let query = self.filter_query.to_lowercase();
        let mut rows: Vec<ProcessRowData> = self
            .all_process_rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.name.to_lowercase().contains(&query)
                    || row.pid.contains(&self.filter_query)
            })
            .cloned()
            .collect();

        rows.sort_by(|a, b| match self.sort_mode {
            SortMode::Cpu => b
                .cpu_total_percent
                .total_cmp(&a.cpu_total_percent)
                .then_with(|| b.mem_percent.total_cmp(&a.mem_percent)),
            SortMode::Mem => b
                .mem_percent
                .total_cmp(&a.mem_percent)
                .then_with(|| b.cpu_total_percent.total_cmp(&a.cpu_total_percent)),
            SortMode::Pid => a.pid_num.cmp(&b.pid_num),
            SortMode::Name => a.name.cmp(&b.name),
        });

        if self.sort_reverse {
            rows.reverse();
        }

        self.process_rows = rows;
        self.clamp_selection();

        if let Some(pid) = selected_pid {
            if let Some(idx) = self.process_rows.iter().position(|r| r.pid_num == pid) {
                self.selected_process = idx;
            }
        }
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
        if count > 0 {
            self.selected_process = (self.selected_process + 1).min(count - 1);
        }
    }

    fn previous_process(&mut self) {
        if self.process_count() > 0 {
            self.selected_process = self.selected_process.saturating_sub(1);
        }
    }

    fn page_down(&mut self, page_size: usize) {
        let count = self.process_count();
        if count > 0 {
            self.selected_process = (self.selected_process + page_size.max(1)).min(count - 1);
        }
    }

    fn page_up(&mut self, page_size: usize) {
        if self.process_count() > 0 {
            self.selected_process = self.selected_process.saturating_sub(page_size.max(1));
        }
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

    fn toggle_reverse(&mut self) {
        self.sort_reverse = !self.sort_reverse;
        self.rebuild_process_rows();
    }

    fn set_sort_mode(&mut self, mode: SortMode) {
        self.sort_mode = mode;
        self.rebuild_process_rows();
    }

    fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.rebuild_process_rows();
    }

    fn speed_up_refresh(&mut self) {
        let ms = self.tick_rate.as_millis() as u64;
        let next = ms.saturating_sub(TICK_STEP_MS).max(MIN_TICK_MS);
        self.tick_rate = Duration::from_millis(next);
    }

    fn slow_down_refresh(&mut self) {
        let ms = self.tick_rate.as_millis() as u64;
        let next = (ms + TICK_STEP_MS).min(MAX_TICK_MS);
        self.tick_rate = Duration::from_millis(next);
    }

    fn start_filter_input(&mut self) {
        self.input_mode = InputMode::Filter;
    }

    fn clear_filter(&mut self) {
        self.filter_query.clear();
        self.rebuild_process_rows();
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Enter => self.input_mode = InputMode::Normal,
            KeyCode::Backspace => {
                self.filter_query.pop();
                self.rebuild_process_rows();
            }
            KeyCode::Char(c) => {
                if !c.is_control() {
                    self.filter_query.push(c);
                    self.rebuild_process_rows();
                }
            }
            _ => {}
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

        let timeout = app
            .tick_rate
            .checked_sub(app.last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match app.input_mode {
                    InputMode::Filter => app.handle_filter_key(key.code),
                    InputMode::Normal => {
                        let page_size = terminal.size()?.height.saturating_sub(14) as usize;
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
                            KeyCode::Char('/') => app.start_filter_input(),
                            KeyCode::Char('x') => app.clear_filter(),
                            KeyCode::Char('s') => app.cycle_sort_mode(),
                            KeyCode::Char('r') => app.toggle_reverse(),
                            KeyCode::Char('c') => app.set_sort_mode(SortMode::Cpu),
                            KeyCode::Char('m') => app.set_sort_mode(SortMode::Mem),
                            KeyCode::Char('p') => app.set_sort_mode(SortMode::Pid),
                            KeyCode::Char('n') => app.set_sort_mode(SortMode::Name),
                            KeyCode::Char('+') | KeyCode::Char('=') => app.speed_up_refresh(),
                            KeyCode::Char('-') | KeyCode::Char('_') => app.slow_down_refresh(),
                            _ => {}
                        }
                    }
                }
            }
        }

        if app.last_tick.elapsed() >= app.tick_rate {
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
    let refresh_left_ms = app
        .tick_rate
        .saturating_sub(app.last_tick.elapsed())
        .as_millis()
        .min(9999);
    let topbar = Paragraph::new(format!(
        " rtop | CPU {:>5.1}% | cores {} | procs {:>4} | sort {}{} | refresh {:>4}ms ",
        cpu_usage,
        app.cpu_count,
        app.process_count(),
        app.sort_mode.label(),
        if app.sort_reverse { " desc" } else { " asc" },
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
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .split(body[0]);

    let cpu_panel = LineGauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" CPU (all cores) ")
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

    let load = System::load_average();
    let filter_title = match app.input_mode {
        InputMode::Normal => " Filter ",
        InputMode::Filter => " Filter (typing) ",
    };
    let filter_text = if app.filter_query.is_empty() {
        String::from(" / to search process names, x to clear ")
    } else {
        format!(" {} ", app.filter_query)
    };
    let filter_panel = Paragraph::new(format!(
        "{}\nload avg: {:.2} {:.2} {:.2}",
        filter_text, load.one, load.five, load.fifteen
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(filter_title)
            .border_style(Style::default().fg(Color::LightBlue)),
    )
    .style(Style::default().fg(Color::White));
    f.render_widget(filter_panel, left_panels[3]);

    let table_visible_rows = body[1].height.saturating_sub(3) as usize;
    app.align_scroll_to_selection(table_visible_rows);

    let start = app.process_scroll.min(app.process_count());
    let end = (start + table_visible_rows).min(app.process_count());

    let header = Row::new(
        ["Pid", "Program", "Cpu% (all)", "Mem%"]
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

            let cpu_style = if p.cpu_total_percent >= 40.0 {
                Style::default().fg(Color::Red)
            } else if p.cpu_total_percent >= 15.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::LightGreen)
            };

            Row::new(vec![
                Cell::from(p.pid.clone()),
                Cell::from(p.name.clone()),
                Cell::from(format!("{:>7.1}", p.cpu_total_percent)).style(cpu_style),
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
            Constraint::Percentage(14),
            Constraint::Percentage(54),
            Constraint::Percentage(18),
            Constraint::Percentage(14),
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

    let mode = match app.input_mode {
        InputMode::Normal => "normal",
        InputMode::Filter => "filter",
    };
    let footer = Paragraph::new(format!(
        " mode {} | q/Ctrl+C quit | j/k PgUp/PgDn Home/End move | / filter, x clear | s cycle-sort | c/m/p/n sort | r reverse | +/- refresh ",
        mode
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, root[2]);
}
