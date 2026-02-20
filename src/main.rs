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
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table},
};
use std::{
    collections::VecDeque,
    io,
    net::UdpSocket,
    path::Path,
    time::{Duration, Instant},
};
use sysinfo::{
    CpuRefreshKind, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind,
};

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
    name_lc: String,
    command: String,
    user: String,
    threads: usize,
    cpu_total_percent: f32,
    mem_percent: f64,
    mem_bytes: u64,
}

enum InputMode {
    Normal,
    Filter,
}

struct App {
    system: System,
    disks: Disks,
    networks: Networks,
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
    net_iface: String,
    net_rx_rate: f64,
    net_tx_rate: f64,
    net_rx_top: f64,
    net_tx_top: f64,
    net_rx_total: u64,
    net_tx_total: u64,
    net_ip: String,
    last_disk_refresh: Instant,
    last_process_refresh: Instant,
    last_ip_refresh: Instant,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            system: System::new_all(),
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
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
            net_iface: "-".to_string(),
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            net_rx_top: 0.0,
            net_tx_top: 0.0,
            net_rx_total: 0,
            net_tx_total: 0,
            net_ip: "-".to_string(),
            last_disk_refresh: Instant::now(),
            last_process_refresh: Instant::now(),
            last_ip_refresh: Instant::now() - Duration::from_secs(30),
        };
        app.update();
        app
    }

    fn update(&mut self) {
        let elapsed = self.last_tick.elapsed().as_secs_f64().max(0.001);
        self.system
            .refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
        self.system.refresh_memory();
        let mut process_refreshed = false;
        if self.all_process_rows.is_empty()
            || self.last_process_refresh.elapsed() >= Duration::from_millis(1500)
        {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
                    .with_user(UpdateKind::OnlyIfNotSet)
                    .with_cmd(UpdateKind::OnlyIfNotSet)
                    .without_tasks(),
            );
            process_refreshed = true;
            self.last_process_refresh = Instant::now();
        }
        if self.last_disk_refresh.elapsed() >= Duration::from_secs(5) {
            self.disks.refresh(false);
            self.last_disk_refresh = Instant::now();
        }
        self.networks.refresh(true);
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

        if process_refreshed {
            let total_mem = self.system.total_memory().max(1) as f64;
            self.all_process_rows = self
                .system
                .processes()
                .values()
                .map(|p| {
                    let name = trim_text(&p.name().to_string_lossy(), 18);
                    ProcessRowData {
                        pid: p.pid().to_string(),
                        pid_num: p.pid().as_u32(),
                        name_lc: name.to_lowercase(),
                        name,
                        command: p
                            .cmd()
                            .iter()
                            .take(4)
                            .map(|s| s.to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join(" "),
                        user: p
                            .user_id()
                            .map(|uid| uid.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        threads: p.tasks().map_or(1, |tasks| tasks.len().max(1)),
                        cpu_total_percent: (p.cpu_usage() / self.cpu_count as f32)
                            .clamp(0.0, 100.0),
                        mem_percent: (p.memory() as f64 / total_mem * 100.0).clamp(0.0, 100.0),
                        mem_bytes: p.memory(),
                    }
                })
                .collect();
            for row in &mut self.all_process_rows {
                if row.command.is_empty() {
                    row.command = row.name.clone();
                } else {
                    row.command = trim_text(&row.command, 54);
                }
            }
            self.rebuild_process_rows();
        }

        let selected = self
            .networks
            .iter()
            .filter(|(name, _)| !name.starts_with("lo"))
            .max_by_key(|(_, data)| data.total_received() + data.total_transmitted())
            .or_else(|| self.networks.iter().next());
        if let Some((name, data)) = selected {
            self.net_iface = name.clone();
            self.net_rx_rate = data.received() as f64 / elapsed;
            self.net_tx_rate = data.transmitted() as f64 / elapsed;
            self.net_rx_top = self.net_rx_top.max(self.net_rx_rate);
            self.net_tx_top = self.net_tx_top.max(self.net_tx_rate);
            self.net_rx_total = data.total_received();
            self.net_tx_total = data.total_transmitted();
        }
        if self.last_ip_refresh.elapsed() >= Duration::from_secs(15) {
            if let Some(ip) = detect_local_ip() {
                self.net_ip = ip;
            }
            self.last_ip_refresh = Instant::now();
        }
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
                    || row.name_lc.contains(&query)
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
            loop {
                if let Event::Key(key) = event::read()? {
                    match app.input_mode {
                        InputMode::Filter => app.handle_filter_key(key.code),
                        InputMode::Normal => {
                            let page_size = terminal.size()?.height.saturating_sub(14) as usize;
                            match key.code {
                                KeyCode::Char('q') => return Ok(()),
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
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
                if !event::poll(Duration::from_millis(0))? {
                    break;
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
    let avg_freq_mhz = if app.system.cpus().is_empty() {
        0
    } else {
        app.system.cpus().iter().map(|c| c.frequency()).sum::<u64>()
            / app.system.cpus().len() as u64
    };

    let top_block = Block::default()
        .borders(Borders::ALL)
        .title("┌1 cpu┐┌menu┐┌preset *")
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
    f.render_widget(
        Paragraph::new(format!(
            "{:02}:{:02}:{:02}",
            (uptime / 3600) % 24,
            (uptime / 60) % 60,
            uptime % 60
        ))
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .centered(),
        top_inner,
    );

    let mini = Rect {
        x: top_inner.x + top_inner.width.saturating_sub(top_inner.width.min(38)),
        y: top_inner.y + top_inner.height.saturating_sub(top_inner.height.min(9)),
        width: top_inner.width.min(38),
        height: top_inner.height.min(9),
    };
    let graph_width = top_inner.width.saturating_sub(mini.width + 1);
    if graph_width > 2 && top_inner.height > 2 {
        let graph = Rect {
            x: top_inner.x,
            y: top_inner.y,
            width: graph_width,
            height: top_inner.height,
        };
        let history = app.cpu_history.iter().copied().collect::<Vec<_>>();
        f.render_widget(
            Sparkline::default()
                .data(&history)
                .max(100)
                .style(Style::default().fg(Color::Green)),
            graph,
        );
    }
    f.render_widget(Clear, mini);
    let mini_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{}ms +", refresh_ms))
        .border_style(Style::default().fg(Color::DarkGray));
    let mini_inner = mini_block.inner(mini);
    f.render_widget(mini_block, mini);
    let mut lines = vec![format!(
        "CPU {:<12} {:>5.1}% {:>4.1} GHz",
        bar(cpu_usage, 8),
        cpu_usage,
        avg_freq_mhz as f64 / 1000.0
    )];
    for (i, u) in app.core_usages.iter().enumerate() {
        lines.push(format!("C{:<2} {:<18} {:>5.1}%", i, bar(*u, 12), u));
    }
    lines.push(format!(
        "Load AVG: {:>4.2} {:>4.2} {:>4.2}",
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
    let avail_mem = app.system.available_memory();
    let free_mem = app.system.free_memory();
    let cached_mem = total_mem.saturating_sub(used_mem).saturating_sub(free_mem);

    f.render_widget(Clear, upper_left[0]);
    let mem_block = Block::default()
        .borders(Borders::ALL)
        .title("┌2 mem┐")
        .border_style(Style::default().fg(Color::Red));
    let mem_inner = mem_block.inner(upper_left[0]);
    f.render_widget(mem_block, upper_left[0]);
    f.render_widget(
        Paragraph::new(format!(
            "Total:{:>12.2} GiB\nUsed:{:>13.2} GiB\n{:>18.0}%\n\nAvailable:{:>8.2} GiB\n{:>18.0}%\n\nCached:{:>11.2} GiB\n{:>18.0}%\n\nFree:{:>13.2} GiB\n{:>18.0}%",
            gib(total_mem),
            gib(used_mem),
            pct(used_mem, total_mem),
            gib(avail_mem),
            pct(avail_mem, total_mem),
            gib(cached_mem),
            pct(cached_mem, total_mem),
            gib(free_mem),
            pct(free_mem, total_mem),
        ))
        .style(Style::default().fg(Color::White)),
        mem_inner,
    );

    f.render_widget(Clear, upper_left[1]);
    let disks_block = Block::default()
        .borders(Borders::ALL)
        .title("┌disks┐")
        .border_style(Style::default().fg(Color::Yellow));
    let disks_inner = disks_block.inner(upper_left[1]);
    f.render_widget(disks_block, upper_left[1]);
    let disk_lines = app
        .disks
        .list()
        .iter()
        .take(4)
        .map(|disk| {
            let total = disk.total_space();
            let avail = disk.available_space();
            let used = total.saturating_sub(avail);
            let used_pct = pct(used, total) as f32;
            format!(
                "{}\nUsed:{:>4.0}% {:<14} {:>6.1}G\nFree:{:>4.0}% {:<14} {:>6.1}G",
                short_mount(disk.mount_point()),
                used_pct,
                bar(used_pct, 10),
                gib(used),
                100.0 - used_pct,
                bar(100.0 - used_pct, 10),
                gib(avail)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    f.render_widget(
        Paragraph::new(if disk_lines.is_empty() {
            "No disk data".to_string()
        } else {
            disk_lines
        })
        .style(Style::default().fg(Color::White)),
        disks_inner,
    );

    f.render_widget(Clear, lower_left[0]);
    let net_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("┌3 net┐ {} {}", app.net_iface, app.net_ip))
        .border_style(Style::default().fg(Color::Blue));
    let net_inner = net_block.inner(lower_left[0]);
    f.render_widget(Clear, lower_left[0]);
    f.render_widget(net_block, lower_left[0]);
    f.render_widget(
        Paragraph::new(format!(
            "download {:>8}/s {:<16}\nup top  {:>8}/s\n\ntotal rx {:>10}\n\nupload   {:>8}/s {:<16}\nup top  {:>8}/s\n\ntotal tx {:>10}",
            human_rate(app.net_rx_rate),
            bar(rate_to_pct(app.net_rx_rate, app.net_rx_top), 10),
            human_rate(app.net_rx_top),
            human_bytes(app.net_rx_total),
            human_rate(app.net_tx_rate),
            bar(rate_to_pct(app.net_tx_rate, app.net_tx_top), 10),
            human_rate(app.net_tx_top),
            human_bytes(app.net_tx_total),
        ))
        .style(Style::default().fg(Color::White)),
        net_inner,
    );

    f.render_widget(Clear, lower_left[1]);
    let sync_block = Block::default()
        .borders(Borders::ALL)
        .title("sync auto zero <b eth0 n>")
        .border_style(Style::default().fg(Color::Blue));
    let sync_inner = sync_block.inner(lower_left[1]);
    f.render_widget(sync_block, lower_left[1]);
    f.render_widget(
        Paragraph::new(format!(
            "download\n{:>8}/s\nTop:{:>9}/s\nTotal:{:>9}\n\nupload\n{:>8}/s\nTop:{:>9}/s\nTotal:{:>9}\n\n{}",
            human_rate(app.net_rx_rate),
            human_rate(app.net_rx_top),
            human_bytes(app.net_rx_total),
            human_rate(app.net_tx_rate),
            human_rate(app.net_tx_top),
            human_bytes(app.net_tx_total),
            format!("sort={} rev={}", sort_name(app.sort_mode), if app.sort_reverse { "on" } else { "off" }),
        ))
        .style(Style::default().fg(Color::White)),
        sync_inner,
    );

    let proc_area = lower[1];
    let table_visible_rows = proc_area.height.saturating_sub(3) as usize;
    app.align_scroll_to_selection(table_visible_rows);
    let start = app.process_scroll.min(app.process_count());
    let end = (start + table_visible_rows).min(app.process_count());

    let cpu_header = if matches!(app.sort_mode, SortMode::Cpu) {
        if app.sort_reverse {
            "Cpu%↑"
        } else {
            "Cpu%↓"
        }
    } else {
        "Cpu%"
    };
    let mem_header = if matches!(app.sort_mode, SortMode::Mem) {
        if app.sort_reverse {
            "MemB↑"
        } else {
            "MemB↓"
        }
    } else {
        "MemB"
    };
    let pid_header = if matches!(app.sort_mode, SortMode::Pid) {
        if app.sort_reverse { "Pid↓" } else { "Pid↑" }
    } else {
        "Pid"
    };
    let header = Row::new(
        [
            pid_header, "Program", "Command", "Threads", "User", mem_header, cpu_header,
        ]
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
                Cell::from(if p.command.is_empty() {
                    p.name.clone()
                } else {
                    p.command.clone()
                }),
                Cell::from(format!("{}", p.threads)),
                Cell::from(p.user.clone()),
                Cell::from(format!("{:>6}", human_bytes(p.mem_bytes))),
                Cell::from(format!(
                    "{} {:>4.1}",
                    bar(p.cpu_total_percent, 5),
                    p.cpu_total_percent
                )),
            ])
            .style(row_style)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(10),
            Constraint::Percentage(16),
            Constraint::Percentage(38),
            Constraint::Percentage(8),
            Constraint::Percentage(12),
            Constraint::Percentage(8),
            Constraint::Percentage(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "┌4 proc┐{}┐per-core {}┐reverse {}┐tree┐< {} {} >",
                filter_mode_label(app),
                "on",
                if app.sort_reverse { "on" } else { "off" },
                sort_name(app.sort_mode),
                "lazy"
            ))
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(Clear, proc_area);
    f.render_widget(table, proc_area);

    let footer_left = match app.input_mode {
        InputMode::Filter => " ↑↓ select  / search: ACTIVE  Esc/Enter done  x clear  q quit ",
        InputMode::Normal => {
            " ↑↓ select  / search: inactive  x clear  t terminate  k kill  s signals  q quit "
        }
    };
    let current = if app.process_count() == 0 {
        0
    } else {
        app.selected_process + 1
    };
    let footer_right = format!("{current}/{}", app.process_count());
    let width = root[2].width as usize;
    let spacer = width.saturating_sub(footer_left.chars().count() + footer_right.len());
    f.render_widget(
        Paragraph::new(format!("{footer_left}{}{footer_right}", " ".repeat(spacer)))
            .style(Style::default().fg(Color::DarkGray)),
        root[2],
    );
}

fn sort_name(mode: SortMode) -> &'static str {
    match mode {
        SortMode::Cpu => "cpu",
        SortMode::Mem => "mem",
        SortMode::Pid => "pid",
        SortMode::Name => "name",
    }
}

fn filter_mode_label(app: &App) -> String {
    match app.input_mode {
        InputMode::Filter => {
            if app.filter_query.is_empty() {
                "f _◄".to_string()
            } else {
                format!("f {}◄", app.filter_query)
            }
        }
        InputMode::Normal => {
            if app.filter_query.is_empty() {
                "filter off".to_string()
            } else {
                format!("filter {}", app.filter_query)
            }
        }
    }
}

fn bar(value: f32, width: usize) -> String {
    let fill = ((value.clamp(0.0, 100.0) / 100.0) * width as f32).round() as usize;
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < fill { '█' } else { '·' });
    }
    s
}

fn short_mount(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.len() <= 10 {
        s.to_string()
    } else {
        format!("..{}", &s[s.len() - 10..])
    }
}

fn human_rate(bytes_per_sec: f64) -> String {
    let b = bytes_per_sec.max(0.0);
    if b >= 1_073_741_824.0 {
        format!("{:.1} GiB", b / 1_073_741_824.0)
    } else if b >= 1_048_576.0 {
        format!("{:.1} MiB", b / 1_048_576.0)
    } else if b >= 1024.0 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{:.0} B", b)
    }
}

fn rate_to_pct(current: f64, top: f64) -> f32 {
    if top <= 0.0 {
        0.0
    } else {
        ((current / top) * 100.0).clamp(0.0, 100.0) as f32
    }
}

fn trim_text(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{out}..")
    } else {
        out
    }
}

fn detect_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
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

fn human_bytes(b: u64) -> String {
    let kib = 1024.0;
    let mib = kib * 1024.0;
    let gib = mib * 1024.0;
    let bf = b as f64;
    if bf >= gib {
        format!("{:.1}G", bf / gib)
    } else if bf >= mib {
        format!("{:.0}M", bf / mib)
    } else if bf >= kib {
        format!("{:.0}K", bf / kib)
    } else {
        format!("{}B", b)
    }
}
