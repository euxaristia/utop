use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
type AppResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table},
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::UdpSocket,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use sysinfo::{
    CpuRefreshKind, Disks, LoadAvg, Networks, ProcessRefreshKind, ProcessesToUpdate, System,
    UpdateKind,
};

const MAX_CPU_HISTORY: usize = 240;
const MAX_NET_HISTORY: usize = 180;
const MIN_TICK_MS: u64 = 200;
const MAX_TICK_MS: u64 = 5000;
const TICK_STEP_MS: u64 = 100;
const TARGET_FPS: u64 = 60;
const FRAME_BUDGET_NS: u64 = 1_000_000_000 / TARGET_FPS;
const MAX_INPUT_EVENTS_PER_CYCLE: usize = 64;

const SIGNAL_OPTIONS: [(&str, &str); 6] = [
    ("TERM", "-TERM"),
    ("KILL", "-KILL"),
    ("INT", "-INT"),
    ("HUP", "-HUP"),
    ("USR1", "-USR1"),
    ("USR2", "-USR2"),
];

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

    fn prev(self) -> Self {
        match self {
            Self::Cpu => Self::Name,
            Self::Mem => Self::Cpu,
            Self::Pid => Self::Mem,
            Self::Name => Self::Pid,
        }
    }
}

#[derive(Clone)]
struct ProcessRowData {
    pid: String,
    pid_num: u32,
    parent_pid: Option<u32>,
    name: String,
    name_lc: String,
    command: String,
    user: String,
    cpu_raw_percent: f32,
    cpu_total_percent: f32,
    mem_percent: f64,
    tree_prefix: String,
    display_name: String,
    threads_text: String,
    mem_text: String,
    cpu_cell_per_core: String,
    cpu_cell_total: String,
}

impl ProcessRowData {
    fn display_cpu(&self, per_core: bool) -> f32 {
        if per_core {
            self.cpu_total_percent
        } else {
            self.cpu_raw_percent
        }
    }
}

enum InputMode {
    Normal,
    Filter,
}

enum ModalState {
    Confirm {
        pid: u32,
        name: String,
        signal_name: &'static str,
        signal_arg: &'static str,
    },
    SignalPicker {
        selected: usize,
    },
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
    per_core: bool,
    tree_mode: bool,
    proc_lazy: bool,
    filter_query: String,
    input_mode: InputMode,
    net_iface: String,
    net_rx_rate: f64,
    net_tx_rate: f64,
    net_rx_top: f64,
    net_tx_top: f64,
    net_rx_total: u64,
    net_tx_total: u64,
    net_rx_history: VecDeque<u64>,
    net_tx_history: VecDeque<u64>,
    net_ip: String,
    last_disk_refresh: Instant,
    last_process_refresh: Instant,
    last_ip_refresh: Instant,
    modal: Option<ModalState>,
    status_msg: String,
    dirty: bool,
    last_table_visible_rows: usize,
    avg_freq_mhz: u64,
    load_avg: LoadAvg,
    uptime_secs: u64,
    last_input: Instant,
    perf_update_ms_ema: f64,
    perf_draw_ms_ema: f64,
    pending_scroll: i32,
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
            per_core: true,
            tree_mode: false,
            proc_lazy: true,
            filter_query: String::new(),
            input_mode: InputMode::Normal,
            net_iface: "-".to_string(),
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            net_rx_top: 0.0,
            net_tx_top: 0.0,
            net_rx_total: 0,
            net_tx_total: 0,
            net_rx_history: VecDeque::with_capacity(MAX_NET_HISTORY),
            net_tx_history: VecDeque::with_capacity(MAX_NET_HISTORY),
            net_ip: "-".to_string(),
            last_disk_refresh: Instant::now(),
            last_process_refresh: Instant::now(),
            last_ip_refresh: Instant::now() - Duration::from_secs(30),
            modal: None,
            status_msg: String::new(),
            dirty: true,
            last_table_visible_rows: 24,
            avg_freq_mhz: 0,
            load_avg: LoadAvg {
                one: 0.0,
                five: 0.0,
                fifteen: 0.0,
            },
            uptime_secs: 0,
            last_input: Instant::now(),
            perf_update_ms_ema: 0.0,
            perf_draw_ms_ema: 0.0,
            pending_scroll: 0,
        };
        app.update();
        app
    }

    fn update(&mut self) {
        self.system
            .refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
        self.system.refresh_memory();
        let process_interval = if self.proc_lazy { 1500 } else { 600 };
        let recent_input = self.last_input.elapsed() < Duration::from_millis(300);
        let max_staleness = Duration::from_secs(5);
        let mut process_refreshed = false;
        if self.all_process_rows.is_empty()
            || (self.last_process_refresh.elapsed() >= Duration::from_millis(process_interval)
                && (!recent_input || self.last_process_refresh.elapsed() >= max_staleness))
        {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
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
        self.avg_freq_mhz = if self.system.cpus().is_empty() {
            0
        } else {
            self.system.cpus().iter().map(|c| c.frequency()).sum::<u64>() / self.cpu_count as u64
        };
        self.load_avg = System::load_average();
        self.uptime_secs = System::uptime();

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
                    let threads = p.tasks().map_or(1, |tasks| tasks.len().max(1));
                    let cpu_usage =
                        p.cpu_usage()
                            .clamp(0.0, (self.cpu_count as f32 * 100.0).max(100.0));
                    let cpu_total_percent = (cpu_usage / self.cpu_count as f32).clamp(0.0, 100.0);
                    let mem_bytes = p.memory();
                    ProcessRowData {
                        pid: p.pid().to_string(),
                        pid_num: p.pid().as_u32(),
                        parent_pid: p.parent().map(|pp| pp.as_u32()),
                        name_lc: name.to_lowercase(),
                        name: name.clone(),
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
                        cpu_raw_percent: cpu_usage,
                        cpu_total_percent,
                        mem_percent: (mem_bytes as f64 / total_mem * 100.0).clamp(0.0, 100.0),
                        tree_prefix: String::new(),
                        display_name: name.clone(),
                        threads_text: threads.to_string(),
                        mem_text: format!("{:>6}", human_bytes(mem_bytes)),
                        cpu_cell_per_core: format!(
                            "{} {:>4.1}",
                            bar(cpu_total_percent, 5),
                            cpu_total_percent
                        ),
                        cpu_cell_total: format!(
                            "{} {:>4.1}",
                            bar((cpu_usage / self.cpu_count as f32).clamp(0.0, 100.0), 5),
                            cpu_usage
                        ),
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
            let elapsed = self.last_tick.elapsed().as_secs_f64().max(0.001);
            self.net_rx_rate = data.received() as f64 / elapsed;
            self.net_tx_rate = data.transmitted() as f64 / elapsed;
            self.net_rx_top = self.net_rx_top.max(self.net_rx_rate);
            self.net_tx_top = self.net_tx_top.max(self.net_tx_rate);
            self.net_rx_total = data.total_received();
            self.net_tx_total = data.total_transmitted();
            self.net_rx_history.push_back(self.net_rx_rate as u64);
            self.net_tx_history.push_back(self.net_tx_rate as u64);
            while self.net_rx_history.len() > MAX_NET_HISTORY {
                self.net_rx_history.pop_front();
            }
            while self.net_tx_history.len() > MAX_NET_HISTORY {
                self.net_tx_history.pop_front();
            }
        }
        if self.last_ip_refresh.elapsed() >= Duration::from_secs(15) {
            if let Some(ip) = detect_local_ip() {
                self.net_ip = ip;
            }
            self.last_ip_refresh = Instant::now();
        }
        self.dirty = true;
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

        if self.tree_mode {
            rows = tree_order_rows(rows);
        } else {
            rows.sort_by(|a, b| match self.sort_mode {
                SortMode::Cpu => b
                    .display_cpu(self.per_core)
                    .total_cmp(&a.display_cpu(self.per_core))
                    .then_with(|| b.mem_percent.total_cmp(&a.mem_percent)),
                SortMode::Mem => b
                    .mem_percent
                    .total_cmp(&a.mem_percent)
                    .then_with(|| b.cpu_total_percent.total_cmp(&a.cpu_total_percent)),
                SortMode::Pid => a.pid_num.cmp(&b.pid_num),
                SortMode::Name => a.name.cmp(&b.name),
            });
        }

        if self.sort_reverse {
            rows.reverse();
        }
        for row in &mut rows {
            row.display_name = if self.tree_mode {
                format!("{}{}", row.tree_prefix, row.name)
            } else {
                row.name.clone()
            };
        }

        self.process_rows = rows;
        self.clamp_selection();

        if let Some(pid) = selected_pid {
            if let Some(idx) = self.process_rows.iter().position(|r| r.pid_num == pid) {
                self.selected_process = idx;
            }
        }
        self.clamp_selection();
        self.dirty = true;
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

    fn jump_top(&mut self) {
        if self.selected_process != 0 {
            self.selected_process = 0;
            self.dirty = true;
        }
    }

    fn jump_bottom(&mut self) {
        let count = self.process_count();
        if count > 0 && self.selected_process != count - 1 {
            self.selected_process = count - 1;
            self.dirty = true;
        }
    }

    fn queue_scroll(&mut self, delta: i32) {
        self.pending_scroll = self.pending_scroll.saturating_add(delta).clamp(-100_000, 100_000);
        self.dirty = true;
    }

    fn flush_pending_scroll(&mut self, max_step: usize) {
        if self.pending_scroll == 0 || self.process_count() == 0 || max_step == 0 {
            return;
        }
        let step = max_step as i32;
        let applied = if self.pending_scroll > 0 {
            self.pending_scroll.min(step)
        } else {
            self.pending_scroll.max(-step)
        };
        self.pending_scroll -= applied;
        let count = self.process_count();
        let next = (self.selected_process as i64 + applied as i64).clamp(0, (count - 1) as i64);
        let next = next as usize;
        if next != self.selected_process {
            self.selected_process = next;
            self.dirty = true;
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

    fn cycle_sort_mode_prev(&mut self) {
        self.sort_mode = self.sort_mode.prev();
        self.rebuild_process_rows();
    }

    fn speed_up_refresh(&mut self) {
        let ms = self.tick_rate.as_millis() as u64;
        let next = ms.saturating_sub(TICK_STEP_MS).max(MIN_TICK_MS);
        if next != ms {
            self.tick_rate = Duration::from_millis(next);
            self.dirty = true;
        }
    }

    fn slow_down_refresh(&mut self) {
        let ms = self.tick_rate.as_millis() as u64;
        let next = (ms + TICK_STEP_MS).min(MAX_TICK_MS);
        if next != ms {
            self.tick_rate = Duration::from_millis(next);
            self.dirty = true;
        }
    }

    fn toggle_per_core(&mut self) {
        self.per_core = !self.per_core;
        self.status_msg = format!("per-core {}", if self.per_core { "on" } else { "off" });
        self.rebuild_process_rows();
    }

    fn toggle_tree_mode(&mut self) {
        self.tree_mode = !self.tree_mode;
        self.status_msg = format!("tree {}", if self.tree_mode { "on" } else { "off" });
        self.rebuild_process_rows();
    }

    fn toggle_proc_lazy(&mut self) {
        self.proc_lazy = !self.proc_lazy;
        self.status_msg = format!("lazy {}", if self.proc_lazy { "on" } else { "off" });
        self.dirty = true;
    }

    fn start_filter_input(&mut self) {
        self.input_mode = InputMode::Filter;
        self.dirty = true;
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
        self.dirty = true;
    }

    fn selected_process_row(&self) -> Option<&ProcessRowData> {
        self.process_rows.get(self.selected_process)
    }

    fn open_confirm_for_selected(&mut self, signal_name: &'static str, signal_arg: &'static str) {
        if let Some(row) = self.selected_process_row() {
            self.modal = Some(ModalState::Confirm {
                pid: row.pid_num,
                name: row.name.clone(),
                signal_name,
                signal_arg,
            });
        } else {
            self.status_msg = "No process selected".to_string();
        }
        self.dirty = true;
    }

    fn apply_signal(&mut self, pid: u32, signal_name: &str, signal_arg: &str) {
        let status = Command::new("kill")
            .arg(signal_arg)
            .arg(pid.to_string())
            .status();
        self.status_msg = match status {
            Ok(s) if s.success() => format!("Sent SIG{signal_name} to pid {pid}"),
            Ok(s) => format!("kill exited with status {s}"),
            Err(e) => format!("Failed to send signal: {e}"),
        };
        self.update();
        self.dirty = true;
    }

    fn handle_modal_key(&mut self, code: KeyCode) {
        let mut next_modal = self.modal.take();
        if let Some(modal) = next_modal.take() {
            match modal {
                ModalState::Confirm {
                    pid,
                    name,
                    signal_name,
                    signal_arg,
                } => match code {
                    KeyCode::Esc | KeyCode::Char('n') => {}
                    KeyCode::Enter | KeyCode::Char('y') => {
                        self.apply_signal(pid, signal_name, signal_arg);
                    }
                    _ => {
                        self.modal = Some(ModalState::Confirm {
                            pid,
                            name,
                            signal_name,
                            signal_arg,
                        });
                    }
                },
                ModalState::SignalPicker { mut selected } => match code {
                    KeyCode::Esc => {}
                    KeyCode::Down => {
                        selected = (selected + 1).min(SIGNAL_OPTIONS.len() - 1);
                        self.modal = Some(ModalState::SignalPicker { selected });
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        self.modal = Some(ModalState::SignalPicker { selected });
                    }
                    KeyCode::Enter => {
                        let (name, arg) = SIGNAL_OPTIONS[selected];
                        self.open_confirm_for_selected(name, arg);
                    }
                    _ => {
                        self.modal = Some(ModalState::SignalPicker { selected });
                    }
                },
            }
        }
        self.dirty = true;
    }
}

fn main() -> AppResult<()> {
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

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> AppResult<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let frame_budget = Duration::from_nanos(FRAME_BUDGET_NS);
    let mut last_draw = Instant::now() - frame_budget;
    loop {
        let tick_timeout = app
            .tick_rate
            .checked_sub(app.last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));
        let timeout = if app.dirty {
            let frame_timeout = frame_budget
                .checked_sub(last_draw.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));
            tick_timeout.min(frame_timeout)
        } else {
            tick_timeout
        };

        if event::poll(timeout)? {
            let mut handled = 0usize;
            loop {
                if let Event::Key(key) = event::read()? {
                    app.last_input = Instant::now();
                    if app.modal.is_some() {
                        app.handle_modal_key(key.code);
                    } else {
                        match app.input_mode {
                            InputMode::Filter => app.handle_filter_key(key.code),
                            InputMode::Normal => {
                                let page_size = app.last_table_visible_rows.max(1);
                                match key.code {
                                    KeyCode::Char('q') => return Ok(()),
                                    KeyCode::Char('c')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        return Ok(());
                                    }
                                    KeyCode::Down => app.queue_scroll(1),
                                    KeyCode::Up => app.queue_scroll(-1),
                                    KeyCode::PageDown => app.queue_scroll(page_size as i32),
                                    KeyCode::PageUp => app.queue_scroll(-(page_size as i32)),
                                    KeyCode::Home => {
                                        app.pending_scroll = 0;
                                        app.jump_top();
                                    }
                                    KeyCode::End => {
                                        app.pending_scroll = 0;
                                        app.jump_bottom();
                                    }
                                    KeyCode::Char('/') => app.start_filter_input(),
                                    KeyCode::Char('x') => app.clear_filter(),
                                    KeyCode::Char('t') => {
                                        app.open_confirm_for_selected("TERM", "-TERM")
                                    }
                                    KeyCode::Char('k') => {
                                        app.open_confirm_for_selected("KILL", "-KILL")
                                    }
                                    KeyCode::Char('s') => {
                                        app.modal = Some(ModalState::SignalPicker { selected: 0 })
                                    }
                                    KeyCode::Char('o') => app.cycle_sort_mode(),
                                    KeyCode::Left => app.cycle_sort_mode_prev(),
                                    KeyCode::Right => app.cycle_sort_mode(),
                                    KeyCode::Char('r') => app.toggle_reverse(),
                                    KeyCode::Char('e') => app.toggle_per_core(),
                                    KeyCode::Char('w') => app.toggle_tree_mode(),
                                    KeyCode::Char('l') => app.toggle_proc_lazy(),
                                    KeyCode::Char('c') => app.set_sort_mode(SortMode::Cpu),
                                    KeyCode::Char('m') => app.set_sort_mode(SortMode::Mem),
                                    KeyCode::Char('p') => app.set_sort_mode(SortMode::Pid),
                                    KeyCode::Char('n') => app.set_sort_mode(SortMode::Name),
                                    KeyCode::Char('+') | KeyCode::Char('=') => {
                                        app.speed_up_refresh()
                                    }
                                    KeyCode::Char('-') | KeyCode::Char('_') => {
                                        app.slow_down_refresh()
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                handled += 1;
                if handled >= MAX_INPUT_EVENTS_PER_CYCLE {
                    break;
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        let max_scroll_step = (app.last_table_visible_rows / 3).max(1);
        app.flush_pending_scroll(max_scroll_step);

        if app.last_tick.elapsed() >= app.tick_rate {
            let update_started = Instant::now();
            app.update();
            let update_ms = update_started.elapsed().as_secs_f64() * 1000.0;
            app.perf_update_ms_ema = ema(app.perf_update_ms_ema, update_ms, 0.2);
            app.last_tick = Instant::now();
        }

        if app.dirty && last_draw.elapsed() >= frame_budget {
            let draw_started = Instant::now();
            terminal.draw(|f| ui(f, app))?;
            let draw_ms = draw_started.elapsed().as_secs_f64() * 1000.0;
            app.perf_draw_ms_ema = ema(app.perf_draw_ms_ema, draw_ms, 0.2);
            app.dirty = false;
            last_draw = Instant::now();
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
    let load = &app.load_avg;
    let refresh_ms = app.tick_rate.as_millis().min(9999);

    let top_block = Block::default()
        .borders(Borders::ALL)
        .title("┌1 cpu┐ ┌menu┐ ┌preset *┐")
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(top_block.clone(), root[0]);
    let top_inner = top_block.inner(root[0]);
    let uptime = app.uptime_secs;
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
        let history = app.cpu_history.make_contiguous();
        f.render_widget(
            Sparkline::default()
                .data(history.iter())
                .max(100)
                .style(Style::default().fg(Color::Green)),
            graph,
        );
    }
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
        app.avg_freq_mhz as f64 / 1000.0
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
    let total_swap = app.system.total_swap();
    let used_swap = app.system.used_swap();
    let free_swap = app.system.free_swap();

    let mem_block = Block::default()
        .borders(Borders::ALL)
        .title("┌2 mem┐")
        .border_style(Style::default().fg(Color::Red));
    let mem_inner = mem_block.inner(upper_left[0]);
    f.render_widget(mem_block, upper_left[0]);
    f.render_widget(
        Paragraph::new(format!(
            "MEM  {:>5.1}% {:<10} {:>6.2}/{:>6.2} GiB\nSWAP {:>5.1}% {:<10} {:>6.2}/{:>6.2} GiB\n\nAvailable:{:>8.2} GiB\nCached:{:>11.2} GiB\nFree:{:>13.2} GiB\nSwap free:{:>8.2} GiB",
            pct(used_mem, total_mem),
            bar(pct(used_mem, total_mem) as f32, 8),
            gib(used_mem),
            gib(total_mem),
            pct(used_swap, total_swap),
            bar(pct(used_swap, total_swap) as f32, 8),
            gib(used_swap),
            gib(total_swap),
            gib(avail_mem),
            gib(cached_mem),
            gib(free_mem),
            gib(free_swap),
        ))
        .style(Style::default().fg(Color::White)),
        mem_inner,
    );

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
                friendly_mount(disk.mount_point()),
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

    let net_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("┌3 net┐ {} {}", app.net_iface, app.net_ip))
        .border_style(Style::default().fg(Color::Blue));
    let net_inner = net_block.inner(lower_left[0]);
    f.render_widget(net_block, lower_left[0]);
    let net_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
        ])
        .split(net_inner);
    {
        let rx_hist = app.net_rx_history.make_contiguous();
        let rx_max = rx_hist.iter().copied().max().unwrap_or(1).max(1);
        f.render_widget(
            Sparkline::default()
                .data(rx_hist.iter())
                .max(rx_max)
                .style(Style::default().fg(Color::Cyan)),
            net_chunks[0],
        );
    }
    {
        let tx_hist = app.net_tx_history.make_contiguous();
        let tx_max = tx_hist.iter().copied().max().unwrap_or(1).max(1);
        f.render_widget(
            Sparkline::default()
                .data(tx_hist.iter())
                .max(tx_max)
                .style(Style::default().fg(Color::Green)),
            net_chunks[1],
        );
    }
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
        net_chunks[2],
    );

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
            format!(
                "sort={} rev={} tree={} lazy={} upd={:.1}ms draw={:.1}ms",
                sort_name(app.sort_mode),
                if app.sort_reverse { "on" } else { "off" },
                if app.tree_mode { "on" } else { "off" },
                if app.proc_lazy { "on" } else { "off" },
                app.perf_update_ms_ema,
                app.perf_draw_ms_ema
            ),
        ))
        .style(Style::default().fg(Color::White)),
        sync_inner,
    );

    let proc_area = lower[1];
    let table_visible_rows = proc_area.height.saturating_sub(3) as usize;
    app.last_table_visible_rows = table_visible_rows.max(1);
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
            pid_header, "Program:", "Command:", "Threads:", "User:", mem_header, cpu_header,
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
            Row::new([
                Cell::from(p.pid.as_str()),
                Cell::from(p.display_name.as_str()),
                Cell::from(p.command.as_str()),
                Cell::from(p.threads_text.as_str()),
                Cell::from(p.user.as_str()),
                Cell::from(p.mem_text.as_str()),
                Cell::from(if app.per_core {
                    p.cpu_cell_per_core.as_str()
                } else {
                    p.cpu_cell_total.as_str()
                }),
            ])
            .style(row_style)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(10),
            Constraint::Percentage(15),
            Constraint::Percentage(35),
            Constraint::Percentage(8),
            Constraint::Percentage(12),
            Constraint::Percentage(8),
            Constraint::Percentage(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "┌4 proc┐ {} ┌per-core {}┐ ┌reverse {}┐ ┌tree {}┐ < {} {} >",
                filter_mode_label(app),
                if app.per_core { "on" } else { "off" },
                if app.sort_reverse { "on" } else { "off" },
                if app.tree_mode { "on" } else { "off" },
                sort_name(app.sort_mode),
                if app.proc_lazy { "lazy" } else { "resp" }
            ))
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(table, proc_area);

    let footer_left = match app.input_mode {
        InputMode::Filter => " ↑↓ select  / search: ACTIVE  Esc/Enter done  x clear  q quit ",
        InputMode::Normal => {
            " ↑/↓ select  / search: inactive  x clear  t term  k kill  s signals  ←/→ sort  r rev  e per-core  w tree  l lazy  q quit "
        }
    };
    let current = if app.process_count() == 0 {
        0
    } else {
        app.selected_process + 1
    };
    let footer_right = format!(
        "u:{:.1} d:{:.1} {current}/{}",
        app.perf_update_ms_ema,
        app.perf_draw_ms_ema,
        app.process_count()
    );
    let width = root[2].width as usize;
    let right_len = footer_right.chars().count();
    if width <= right_len + 1 {
        f.render_widget(
            Paragraph::new(trim_text(&footer_right, width))
                .style(Style::default().fg(Color::DarkGray)),
            root[2],
        );
    } else {
        let left_width = width - right_len - 1;
        let left_rect = Rect {
            x: root[2].x,
            y: root[2].y,
            width: left_width as u16,
            height: 1,
        };
        let right_rect = Rect {
            x: root[2].x + left_width as u16 + 1,
            y: root[2].y,
            width: right_len as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(trim_text(footer_left, left_width))
                .style(Style::default().fg(Color::DarkGray)),
            left_rect,
        );
        f.render_widget(
            Paragraph::new(footer_right).style(Style::default().fg(Color::DarkGray)),
            right_rect,
        );
    }

    if !app.status_msg.is_empty() {
        let status_area = Rect {
            x: root[2].x,
            y: root[2].y.saturating_sub(1),
            width: root[2].width.min(f.area().width),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(format!(" {}", app.status_msg))
                .style(Style::default().fg(Color::Yellow)),
            status_area,
        );
    }

    if let Some(modal) = &app.modal {
        let area = centered_rect(56, 8, f.area());
        f.render_widget(Clear, area);
        match modal {
            ModalState::Confirm {
                pid,
                name,
                signal_name,
                ..
            } => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title("confirm signal")
                    .border_style(Style::default().fg(Color::Red));
                let inner = block.inner(area);
                f.render_widget(block, area);
                f.render_widget(
                    Paragraph::new(format!(
                        "Send SIG{} to {} (pid {})?\n\n[y]/Enter = yes   [n]/Esc = no",
                        signal_name, name, pid
                    ))
                    .style(Style::default().fg(Color::White)),
                    inner,
                );
            }
            ModalState::SignalPicker { selected } => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title("select signal")
                    .border_style(Style::default().fg(Color::Red));
                let inner = block.inner(area);
                f.render_widget(block, area);
                let lines = SIGNAL_OPTIONS
                    .iter()
                    .enumerate()
                    .map(|(i, (name, _))| {
                        if i == *selected {
                            format!("> SIG{name}")
                        } else {
                            format!("  SIG{name}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                f.render_widget(
                    Paragraph::new(format!(
                        "{lines}\n\n↑/↓ choose   Enter confirm   Esc cancel"
                    ))
                    .style(Style::default().fg(Color::White)),
                    inner,
                );
            }
        }
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(percent_x).saturating_div(100);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let h = height.min(area.height.saturating_sub(2)).max(3);
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width,
        height: h,
    }
}

fn tree_order_rows(rows: Vec<ProcessRowData>) -> Vec<ProcessRowData> {
    let mut by_pid: HashMap<u32, ProcessRowData> =
        rows.into_iter().map(|row| (row.pid_num, row)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut roots: Vec<u32> = Vec::new();

    for row in by_pid.values() {
        if let Some(parent) = row.parent_pid {
            if parent != row.pid_num && by_pid.contains_key(&parent) {
                children.entry(parent).or_default().push(row.pid_num);
            } else {
                roots.push(row.pid_num);
            }
        } else {
            roots.push(row.pid_num);
        }
    }

    roots.sort_unstable();
    roots.dedup();
    for siblings in children.values_mut() {
        siblings.sort_unstable();
    }

    let mut ordered = Vec::with_capacity(by_pid.len());
    for root in roots {
        walk_tree(root, "", true, &children, &mut by_pid, &mut ordered);
    }

    if !by_pid.is_empty() {
        let mut leftovers: Vec<u32> = by_pid.keys().copied().collect();
        leftovers.sort_unstable();
        for pid in leftovers {
            walk_tree(pid, "", true, &children, &mut by_pid, &mut ordered);
        }
    }

    ordered
}

fn walk_tree(
    pid: u32,
    parent_prefix: &str,
    is_last: bool,
    children: &HashMap<u32, Vec<u32>>,
    by_pid: &mut HashMap<u32, ProcessRowData>,
    ordered: &mut Vec<ProcessRowData>,
) {
    let Some(mut row) = by_pid.remove(&pid) else {
        return;
    };

    let has_parent = row.parent_pid.is_some_and(|p| p != pid);
    row.tree_prefix = if has_parent {
        format!("{}{}", parent_prefix, if is_last { "└─ " } else { "├─ " })
    } else {
        String::new()
    };
    ordered.push(row);

    if let Some(kids) = children.get(&pid) {
        let child_prefix = if has_parent {
            format!("{}{}", parent_prefix, if is_last { "   " } else { "│  " })
        } else {
            String::new()
        };
        let mut present_kids = kids
            .iter()
            .copied()
            .filter(|child_pid| by_pid.contains_key(child_pid))
            .collect::<Vec<_>>();
        let last = present_kids.len().saturating_sub(1);
        for (idx, child_pid) in present_kids.drain(..).enumerate() {
            walk_tree(
                child_pid,
                &child_prefix,
                idx == last,
                children,
                by_pid,
                ordered,
            );
        }
    }
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

fn friendly_mount(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s == "/" {
        "/".to_string()
    } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.is_empty() {
            s.to_string()
        } else {
            name.to_string()
        }
    } else if s.len() <= 12 {
        s.to_string()
    } else {
        format!("..{}", &s[s.len() - 12..])
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

fn ema(current: f64, sample: f64, alpha: f64) -> f64 {
    if current <= 0.0 {
        sample
    } else {
        (alpha * sample) + ((1.0 - alpha) * current)
    }
}
