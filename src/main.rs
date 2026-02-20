use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table},
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
    core_usages: Vec<f32>,
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
            core_usages: Vec::new(),
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
        self.core_usages = self
            .system
            .cpus()
            .iter()
            .map(|cpu| cpu.cpu_usage().clamp(0.0, 100.0))
            .collect();

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
            Constraint::Length(14),
            Constraint::Min(12),
            Constraint::Length(1),
        ])
        .split(f.area());

    let cpu_usage = app.system.global_cpu_usage().clamp(0.0, 100.0);
    let load = System::load_average();
    let refresh_ms = app.tick_rate.as_millis().min(9999);

    let top_block = Block::default()
        .borders(Borders::ALL)
        .title("1 cpu menu preset *")
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Clear, root[0]);
    f.render_widget(top_block.clone(), root[0]);
    let top_inner = top_block.inner(root[0]);
    let uptime = System::uptime();
    f.render_widget(
        Paragraph::new(format!("up {}h {}m", uptime / 3600, (uptime % 3600) / 60))
            .style(Style::default().fg(Color::Gray)),
        top_inner,
    );

    let mini = Rect {
        x: top_inner.x + top_inner.width.saturating_sub(top_inner.width.min(38)),
        y: top_inner.y + top_inner.height.saturating_sub(top_inner.height.min(9)),
        width: top_inner.width.min(38),
        height: top_inner.height.min(9),
    };
    f.render_widget(Clear, mini);
    let mini_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{}ms", refresh_ms))
        .border_style(Style::default().fg(Color::DarkGray));
    let mini_inner = mini_block.inner(mini);
    f.render_widget(mini_block, mini);
    let mut lines = vec![format!(
        "CPU {:>5.1}%  ({} cores)",
        cpu_usage, app.cpu_count
    )];
    for (i, u) in app.core_usages.iter().enumerate() {
        lines.push(format!("C{:<2} {:>5.1}%", i, u));
    }
    lines.push(format!(
        "Load AVG {:>4.2} {:>4.2} {:>4.2}",
        load.one, load.five, load.fifteen
    ));
    f.render_widget(
        Paragraph::new(lines.join("\n")).style(Style::default().fg(Color::White)),
        mini_inner,
    );

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(root[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(8)])
        .split(lower[0]);
    let upper_left = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(53), Constraint::Percentage(47)])
        .split(left[0]);
    let lower_left = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(left[1]);

    let total_mem = app.system.total_memory();
    let used_mem = app.system.used_memory();
    let total_swap = app.system.total_swap();
    let used_swap = app.system.used_swap();
    let avail_mem = app.system.available_memory();
    let free_mem = app.system.free_memory();

    f.render_widget(Clear, upper_left[0]);
    let mem_block = Block::default()
        .borders(Borders::ALL)
        .title("2 mem")
        .border_style(Style::default().fg(Color::Blue));
    let mem_inner = mem_block.inner(upper_left[0]);
    f.render_widget(mem_block, upper_left[0]);
    f.render_widget(
        Paragraph::new(format!(
            "Total: {:>7.2} GiB\nUsed:  {:>7.2} GiB  {:>4.0}%\nAvail: {:>7.2} GiB  {:>4.0}%\nFree:  {:>7.2} GiB  {:>4.0}%",
            gib(total_mem),
            gib(used_mem),
            pct(used_mem, total_mem),
            gib(avail_mem),
            pct(avail_mem, total_mem),
            gib(free_mem),
            pct(free_mem, total_mem),
        ))
        .style(Style::default().fg(Color::White)),
        mem_inner,
    );

    f.render_widget(Clear, upper_left[1]);
    let disks_block = Block::default()
        .borders(Borders::ALL)
        .title("disks")
        .border_style(Style::default().fg(Color::Yellow));
    let disks_inner = disks_block.inner(upper_left[1]);
    f.render_widget(disks_block, upper_left[1]);
    f.render_widget(
        Paragraph::new(format!(
            "root\nUsed: {:>4.0}%\nFree: {:>4.0}%\nswap\nUsed: {:>4.0}%\nFree: {:>4.0}%",
            pct(used_mem, total_mem),
            100.0 - pct(used_mem, total_mem),
            pct(used_swap, total_swap),
            100.0 - pct(used_swap, total_swap),
        ))
        .style(Style::default().fg(Color::White)),
        disks_inner,
    );

    f.render_widget(Clear, lower_left[0]);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("3 net")
            .border_style(Style::default().fg(Color::Blue)),
        lower_left[0],
    );

    f.render_widget(Clear, lower_left[1]);
    let sync_block = Block::default()
        .borders(Borders::ALL)
        .title("sync auto zero")
        .border_style(Style::default().fg(Color::Blue));
    let sync_inner = sync_block.inner(lower_left[1]);
    f.render_widget(sync_block, lower_left[1]);
    f.render_widget(
        Paragraph::new(format!(
            "download\n0 B/s\n\nupload\n0 B/s\n\nfilter: {}",
            if app.filter_query.is_empty() {
                "<none>".to_string()
            } else {
                app.filter_query.clone()
            }
        ))
        .style(Style::default().fg(Color::White)),
        sync_inner,
    );

    let proc_area = lower[1];
    let table_visible_rows = proc_area.height.saturating_sub(3) as usize;
    app.align_scroll_to_selection(table_visible_rows);
    let start = app.process_scroll.min(app.process_count());
    let end = (start + table_visible_rows).min(app.process_count());

    let header = Row::new(
        ["Pid", "Program", "Mem%", "Cpu%"]
            .into_iter()
            .map(Cell::from),
    )
    .style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    let rows = app.process_rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let absolute_index = start + i;
            let mut row_style = if i % 2 == 0 {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::White)
            };
            if absolute_index == app.selected_process {
                row_style = Style::default().fg(Color::Black).bg(Color::Yellow);
            }
            Row::new(vec![
                Cell::from(p.pid.clone()),
                Cell::from(p.name.clone()),
                Cell::from(format!("{:>5.1}", p.mem_percent)),
                Cell::from(format!("{:>5.1}", p.cpu_total_percent)),
            ])
            .style(row_style)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(18),
            Constraint::Percentage(50),
            Constraint::Percentage(16),
            Constraint::Percentage(16),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "4 proc filter <{}>",
                if app.filter_query.is_empty() {
                    "none".to_string()
                } else {
                    app.filter_query.clone()
                }
            ))
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(Clear, proc_area);
    f.render_widget(table, proc_area);

    f.render_widget(
        Paragraph::new(
            " q/Ctrl+C quit | j/k/PgUp/PgDn/Home/End move | / filter | x clear | s sort | r reverse | c/m/p/n | +/- refresh ",
        )
        .style(Style::default().fg(Color::DarkGray)),
        root[2],
    );
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1_073_741_824.0
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}
