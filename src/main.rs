use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::time::{Duration, Instant};

use libc::{
    c_int, fcntl, ioctl, poll, pollfd, signal, tcgetattr, tcsetattr, termios, winsize,
    ECHO, F_SETFL, ICANON, ISIG, O_NONBLOCK, POLLIN, SIGINT, SIGQUIT, SIGTERM, SIGHUP,
    SIG_DFL, STDIN_FILENO, STDOUT_FILENO, TCSAFLUSH, TIOCGWINSZ,
};

// --- Data Structures ---

#[derive(Debug, Clone, Default)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTimes {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
    fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }
}

#[derive(Debug, Clone, Default)]
struct MemorySnapshot {
    used_bytes: u64,
    total_bytes: u64,
    swap_used_bytes: u64,
    swap_total_bytes: u64,
}

impl MemorySnapshot {
    fn combined_used(&self) -> u64 { self.used_bytes + self.swap_used_bytes }
    fn combined_total(&self) -> u64 { self.total_bytes + self.swap_total_bytes }

    fn swap_percent(&self) -> f64 {
        if self.swap_total_bytes == 0 { return 0.0; }
        ((self.swap_used_bytes as f64 / self.swap_total_bytes as f64) * 100.0).min(100.0)
    }
    fn combined_percent(&self) -> f64 {
        let total = self.combined_total();
        if total == 0 { return 0.0; }
        ((self.combined_used() as f64 / total as f64) * 100.0).min(100.0)
    }
}

#[derive(Debug, Clone)]
struct NetworkSnapshot {
    iface: String,
    rx_rate: f64,
    tx_rate: f64,
}

#[derive(Debug, Clone)]
struct GpuSnapshot {
    name: String,
    usage: Option<f64>,
    mem_used: Option<u64>,
    mem_total: Option<u64>,
    temp: Option<f64>,
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    pid: i32,
    name: String,
    cpu_percent: f64,
    mem_bytes: u64,
    threads: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SortMode {
    Cpu,
    Memory,
}

#[derive(Debug, Clone, Copy)]
enum Key {
    Quit,
    Up, Down, Left, Right,
    Backspace, Enter, Esc,
    Char(char),
}

// --- Terminal Control ---

static mut G_TERMIOS_ORIGINAL: termios = unsafe { std::mem::zeroed() };
static mut G_TERMIOS_FLAGS: c_int = -1;
static mut G_TERMIOS_ACTIVE: i32 = 0;
static mut G_ALT_SCREEN_ACTIVE: i32 = 0;

fn write_esc(sequence: &str) {
    let bytes = sequence.as_bytes();
    unsafe {
        libc::write(STDOUT_FILENO, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

extern "C" fn utop_restore_terminal() {
    unsafe {
        if G_TERMIOS_ACTIVE == 0 { return; }
        let restore = G_TERMIOS_ORIGINAL;
        tcsetattr(STDIN_FILENO, TCSAFLUSH, &restore);
        if G_TERMIOS_FLAGS != -1 {
            fcntl(STDIN_FILENO, F_SETFL, G_TERMIOS_FLAGS);
        }
        if G_ALT_SCREEN_ACTIVE == 1 {
            write_esc("\x1B[?1049l");
            G_ALT_SCREEN_ACTIVE = 0;
        }
        write_esc("\x1B[?25h\x1B[0m");
        G_TERMIOS_ACTIVE = 0;
    }
}

extern "C" fn utop_signal_handler(sig: c_int) {
    utop_restore_terminal();
    unsafe {
        signal(sig, SIG_DFL);
        libc::kill(libc::getpid(), sig);
    }
}

struct TerminalRawMode {
    active: bool,
}

impl TerminalRawMode {
    fn new() -> Option<Self> {
        let mut original: termios = unsafe { std::mem::zeroed() };
        if unsafe { tcgetattr(STDIN_FILENO, &mut original) } != 0 { return None; }
        let original_flags = unsafe { fcntl(STDIN_FILENO, libc::F_GETFL, 0) };
        if original_flags == -1 { return None; }

        let mut raw = original;
        raw.c_lflag &= !(ECHO | ICANON | ISIG);
        if unsafe { tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) } != 0 { return None; }
        if unsafe { fcntl(STDIN_FILENO, F_SETFL, original_flags | O_NONBLOCK) } == -1 {
            let mut restore = original;
            unsafe { tcsetattr(STDIN_FILENO, TCSAFLUSH, &mut restore) };
            return None;
        }

        unsafe {
            G_TERMIOS_ORIGINAL = original;
            G_TERMIOS_FLAGS = original_flags;
            G_TERMIOS_ACTIVE = 1;
            G_ALT_SCREEN_ACTIVE = 1;
        }

        write_esc("\x1B[?1049h\x1B[2J\x1B[H\x1B[?25l");

        Some(TerminalRawMode { active: true })
    }

    fn restore_now(&mut self) {
        if !self.active { return; }
        self.active = false;
        utop_restore_terminal();
    }
}

impl Drop for TerminalRawMode {
    fn drop(&mut self) {
        self.restore_now();
    }
}

// --- Sampler ---

struct Sampler {
    previous_cpu: Option<CpuTimes>,
    previous_total_per_pid: HashMap<i32, u64>,
    previous_rx_by_iface: HashMap<String, u64>,
    previous_tx_by_iface: HashMap<String, u64>,
    last_sample_at: Instant,
    page_size: u64,
    cpu_count: usize,
    last_nvidia_sample_at: Instant,
    last_nvidia_gpu: Option<GpuSnapshot>,
    last_v3d_stats: HashMap<String, (u64, u64)>,
}

impl Sampler {
    fn new() -> Self {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let cpu_count = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

        Sampler {
            previous_cpu: None,
            previous_total_per_pid: HashMap::new(),
            previous_rx_by_iface: HashMap::new(),
            previous_tx_by_iface: HashMap::new(),
            last_sample_at: Instant::now(),
            page_size,
            cpu_count,
            last_nvidia_sample_at: Instant::now() - Duration::from_secs(10),
            last_nvidia_gpu: None,
            last_v3d_stats: HashMap::new(),
        }
    }

    fn sample(&mut self, sort_mode: SortMode, filter: &str) -> (f64, Option<f64>, Option<f64>, MemorySnapshot, NetworkSnapshot, Option<GpuSnapshot>, Vec<ProcessInfo>, usize) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_at).as_secs_f64().max(0.001);
        self.last_sample_at = now;

        let current_cpu = self.read_cpu_times();
        let (cpu_percent, delta) = self.compute_cpu_delta(current_cpu);
        let cpu_temp = self.read_cpu_temp();
        let cpu_freq = self.read_cpu_freq();
        let memory = self.read_memory();
        let network = self.read_network(elapsed);
        let gpu = self.read_gpu();
        let processes = self.read_processes(delta, sort_mode, filter);

        (cpu_percent, cpu_temp, cpu_freq, memory, network, gpu, processes, self.cpu_count)
    }

    fn read_cpu_times(&mut self) -> Option<CpuTimes> {
        let content = fs::read_to_string("/proc/stat").ok()?;
        
        // Find aggregate line: starts with "cpu" and a whitespace
        let cpu_line = content.lines().find(|l| {
            l.starts_with("cpu") && l.as_bytes().get(3).map_or(true, |&b| b == b' ' || b == b'\t')
        })?;
        
        let fields: Vec<&str> = cpu_line.split_whitespace().collect();
        if fields.len() < 9 { return None; }

        let nums: Vec<u64> = fields[1..9].iter().filter_map(|s| s.parse().ok()).collect();
        if nums.len() < 8 { return None; }

        let cpu_count = content.lines().filter(|l| {
            l.starts_with("cpu") && l.as_bytes().get(3).map_or(false, |&b| b >= b'0' && b <= b'9')
        }).count();
        if cpu_count > 0 { self.cpu_count = cpu_count; }

        Some(CpuTimes {
            user: nums[0], nice: nums[1], system: nums[2], idle: nums[3],
            iowait: nums[4], irq: nums[5], softirq: nums[6], steal: nums[7],
        })
    }

    fn compute_cpu_delta(&mut self, current: Option<CpuTimes>) -> (f64, u64) {
        let current = match current { Some(c) => c, None => return (0.0, 0) };
        let prev = match &self.previous_cpu {
            Some(p) => p,
            None => { self.previous_cpu = Some(current); return (0.0, 0); }
        };

        let total_delta = if current.total() > prev.total() { current.total() - prev.total() } else { 0 };
        let idle_delta = if current.idle_total() > prev.idle_total() { current.idle_total() - prev.idle_total() } else { 0 };
        self.previous_cpu = Some(current);

        if total_delta == 0 { return (0.0, 0); }
        let used = if total_delta > idle_delta { total_delta - idle_delta } else { 0 };
        let percent = ((used as f64 / total_delta as f64) * 100.0).min(100.0);
        (percent, total_delta)
    }

    fn read_cpu_temp(&self) -> Option<f64> {
        if let Ok(entries) = fs::read_dir("/sys/class/thermal") {
            for entry in entries.flatten() {
                let name = entry.file_name().into_string().unwrap_or_default();
                if name.starts_with("thermal_zone") {
                    let type_path = entry.path().join("type");
                    if let Ok(t_type) = fs::read_to_string(type_path) {
                        let t_type = t_type.trim().to_lowercase();
                        if t_type.contains("pkg") || t_type.contains("cpu") || t_type.contains("core") {
                            if let Ok(temp_str) = fs::read_to_string(entry.path().join("temp")) {
                                if let Ok(t) = temp_str.trim().parse::<f64>() { return Some(t / 1000.0); }
                            }
                        }
                    }
                }
            }
        }
        if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(hw_name) = fs::read_to_string(path.join("name")) {
                    let hw_name = hw_name.trim().to_lowercase();
                    if hw_name.contains("coretemp") || hw_name.contains("cpu") || hw_name.contains("k10temp") {
                        let mut best_temp: Option<f64> = None;
                        if let Ok(h_entries) = fs::read_dir(&path) {
                            for h_entry in h_entries.flatten() {
                                let h_name = h_entry.file_name().into_string().unwrap_or_default();
                                if h_name.starts_with("temp") && h_name.ends_with("_input") {
                                    if let Ok(t_str) = fs::read_to_string(h_entry.path()) {
                                        if let Ok(t) = t_str.trim().parse::<f64>() { best_temp = Some(best_temp.unwrap_or(0.0).max(t / 1000.0)); }
                                    }
                                }
                            }
                        }
                        if let Some(bt) = best_temp { return Some(bt); }
                    }
                }
            }
        }
        None
    }

    fn read_cpu_freq(&self) -> Option<f64> {
        if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
            let freqs: Vec<f64> = content.lines()
                .filter(|l| l.starts_with("cpu MHz"))
                .filter_map(|l| l.split(':').nth(1))
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect();
            if !freqs.is_empty() {
                return Some(freqs.iter().sum::<f64>() / freqs.len() as f64);
            }
        }
        None
    }

    fn read_memory(&self) -> MemorySnapshot {
        let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut snapshot = MemorySnapshot::default();
        let (mut total_kb, mut avail_kb, mut swap_total_kb, mut swap_free_kb) = (0, 0, 0, 0);
        for line in content.lines() {
            if line.starts_with("MemTotal:") { total_kb = parse_meminfo_kb(line); }
            else if line.starts_with("MemAvailable:") { avail_kb = parse_meminfo_kb(line); }
            else if line.starts_with("SwapTotal:") { swap_total_kb = parse_meminfo_kb(line); }
            else if line.starts_with("SwapFree:") { swap_free_kb = parse_meminfo_kb(line); }
        }
        snapshot.total_bytes = total_kb * 1024;
        snapshot.used_bytes = if total_kb > avail_kb { (total_kb - avail_kb) * 1024 } else { 0 };
        snapshot.swap_total_bytes = swap_total_kb * 1024;
        snapshot.swap_used_bytes = if swap_total_kb > swap_free_kb { (swap_total_kb - swap_free_kb) * 1024 } else { 0 };
        snapshot
    }

    fn read_network(&mut self, elapsed: f64) -> NetworkSnapshot {
        let content = fs::read_to_string("/proc/net/dev").unwrap_or_default();
        let mut best = NetworkSnapshot { iface: "-".to_string(), rx_rate: 0.0, tx_rate: 0.0 };
        let mut best_total: u64 = 0;
        for line in content.lines().skip(2) {
            let line_replaced = line.replace(':', " ");
            let parts: Vec<&str> = line_replaced.split_whitespace().collect();
            if parts.len() < 17 { continue; }
            let iface = parts[0].to_string();
            if iface == "lo" { continue; }
            let rx_total: u64 = parts[1].parse().unwrap_or(0);
            let tx_total: u64 = parts[9].parse().unwrap_or(0);
            let prev_rx = *self.previous_rx_by_iface.get(&iface).unwrap_or(&rx_total);
            let prev_tx = *self.previous_tx_by_iface.get(&iface).unwrap_or(&tx_total);
            self.previous_rx_by_iface.insert(iface.clone(), rx_total);
            self.previous_tx_by_iface.insert(iface.clone(), tx_total);
            let rx_rate = (if rx_total >= prev_rx { rx_total - prev_rx } else { 0 }) as f64 / elapsed;
            let tx_rate = (if tx_total >= prev_tx { tx_total - prev_tx } else { 0 }) as f64 / elapsed;
            let sum = rx_total + tx_total;
            if sum > best_total {
                best_total = sum;
                best = NetworkSnapshot { iface, rx_rate, tx_rate };
            }
        }
        best
    }

    fn read_gpu(&mut self) -> Option<GpuSnapshot> {
        let now = Instant::now();
        if now.duration_since(self.last_nvidia_sample_at) >= Duration::from_millis(800) {
            self.last_nvidia_sample_at = now;
            if let Ok(output) = std::process::Command::new("/usr/bin/nvidia-smi")
                .args(["--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"])
                .output() {
                if output.status.success() {
                    let out_str = String::from_utf8_lossy(&output.stdout);
                    let parts: Vec<&str> = out_str.trim().split(',').map(|s| s.trim()).collect();
                    if parts.len() >= 4 {
                        let usage = parts[0].parse().ok();
                        let mem_used = parts[1].parse::<u64>().ok().map(|v| v * 1024 * 1024);
                        let mem_total = parts[2].parse::<u64>().ok().map(|v| v * 1024 * 1024);
                        let temp = parts[3].parse().ok();
                        self.last_nvidia_gpu = Some(GpuSnapshot { name: "NVIDIA GPU".to_string(), usage, mem_used, mem_total, temp });
                        return self.last_nvidia_gpu.clone();
                    }
                }
            }
        } else if self.last_nvidia_gpu.is_some() { return self.last_nvidia_gpu.clone(); }

        if let Ok(entries) = fs::read_dir("/sys/class/drm") {
            for entry in entries.flatten() {
                let name = entry.file_name().into_string().unwrap_or_default();
                if name.starts_with("card") && !name.contains('-') {
                    let card_path = entry.path();
                    let device_path = card_path.join("device");
                    let mut usage: Option<f64> = None;
                    let usage_paths = [device_path.join("gpu_busy_percent"), card_path.join("gt/gt0/usage"), device_path.join("usage"), device_path.join("load")];
                    for p in usage_paths {
                        if let Ok(val) = fs::read_to_string(p) {
                            if let Some(first) = val.trim().split('@').next() {
                                if let Ok(u) = first.parse() { usage = Some(u); break; }
                            }
                        }
                    }
                    if usage.is_none() {
                        if let Ok(stats) = fs::read_to_string(device_path.join("gpu_stats")) {
                            for line in stats.lines().skip(1) {
                                let parts: Vec<&str> = line.split_whitespace().collect();
                                if parts.len() >= 4 {
                                    if let (Ok(ts), Ok(rt)) = (parts[1].parse::<u64>(), parts[3].parse::<u64>()) {
                                        let queue = parts[0].to_string();
                                        if let Some(&(last_ts, last_rt)) = self.last_v3d_stats.get(&queue) {
                                            if ts > last_ts { usage = Some(usage.unwrap_or(0.0).max(((rt - last_rt) as f64 / (ts - last_ts) as f64) * 100.0)); }
                                        }
                                        self.last_v3d_stats.insert(queue, (ts, rt));
                                    }
                                }
                            }
                        }
                    }
                    if usage.is_some() {
                        let vendor = fs::read_to_string(device_path.join("vendor")).unwrap_or_default().trim().to_lowercase();
                        let name_str = match vendor.as_str() {
                            "0x1002" => "AMD GPU", "0x8086" => "Intel GPU", "0x10de" => "NVIDIA GPU", "0x14e4" => "Broadcom GPU",
                            _ => {
                                if let Ok(uevent) = fs::read_to_string(device_path.join("uevent")) {
                                    if uevent.contains("DRIVER=v3d") || uevent.contains("DRIVER=vc4") { "VideoCore GPU" } else { "GPU" }
                                } else { "GPU" }
                            }
                        }.to_string();
                        let (mut mem_used, mut mem_total, mut gpu_temp) = (None, None, None);
                        if let Ok(h_entries) = fs::read_dir(device_path.join("hwmon")) {
                            for h_entry in h_entries.flatten() {
                                if h_entry.file_name().into_string().unwrap_or_default().starts_with("hwmon") {
                                    if let Ok(t_str) = fs::read_to_string(h_entry.path().join("temp1_input")) {
                                        if let Ok(t) = t_str.trim().parse::<f64>() { gpu_temp = Some(t / 1000.0); }
                                    }
                                }
                            }
                        }
                        if gpu_temp.is_none() { if let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") { if let Ok(t) = t_str.trim().parse::<f64>() { gpu_temp = Some(t / 1000.0); } } }
                        if let Ok(u_str) = fs::read_to_string(device_path.join("mem_info_vram_used")) { mem_used = u_str.trim().parse().ok(); }
                        if let Ok(t_str) = fs::read_to_string(device_path.join("mem_info_vram_total")) { mem_total = t_str.trim().parse().ok(); }
                        if mem_used.is_none() {
                            if let Ok(u_str) = fs::read_to_string(card_path.join("tile0/vram0/used")) { mem_used = u_str.trim().parse().ok(); }
                            if let Ok(t_str) = fs::read_to_string(card_path.join("tile0/vram0/size")) { mem_total = t_str.trim().parse().ok(); }
                        }
                        if mem_used.is_none() && name_str == "VideoCore GPU" {
                            if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
                                let (mut cma_t, mut cma_f) = (0, 0);
                                for line in meminfo.lines() {
                                    if line.starts_with("CmaTotal:") { cma_t = parse_meminfo_kb(line) * 1024; } else if line.starts_with("CmaFree:") { cma_f = parse_meminfo_kb(line) * 1024; }
                                }
                                if cma_t > 0 { mem_used = Some(if cma_t > cma_f { cma_t - cma_f } else { 0 }); mem_total = Some(cma_t); }
                            }
                        }
                                                return Some(GpuSnapshot { name: name_str, usage, mem_used, mem_total, temp: gpu_temp });
                                            }
                                        }
                                    }
                                }
                        
                                // 3. Adreno / kgsl
                                let adreno_paths = [
                                    "/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage",
                                    "/sys/class/kgsl/kgsl-3d0/gpubusy",
                                ];
                                for p in adreno_paths {
                                    if let Ok(val_str) = fs::read_to_string(p) {
                                        let val_str = val_str.trim();
                                        let usage = if p.contains("gpubusy") {
                                            let parts: Vec<f64> = val_str.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                                            if parts.len() >= 2 && parts[1] > 0.0 { Some((parts[0] / parts[1]) * 100.0) } else { None }
                                        } else {
                                            val_str.parse().ok()
                                        };
                                        if let Some(u) = usage {
                                            let mut a_temp = None;
                                            if let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
                                                if let Ok(t) = t_str.trim().parse::<f64>() { a_temp = Some(t / 1000.0); }
                                            }
                                            return Some(GpuSnapshot { name: "Adreno GPU".to_string(), usage: Some(u), mem_used: None, mem_total: None, temp: a_temp });
                                        }
                                    }
                                }
                        
                                // 4. Generic devfreq
                                if let Ok(entries) = fs::read_dir("/sys/class/devfreq") {
                                    for entry in entries.flatten() {
                                        let df_name = entry.file_name().into_string().unwrap_or_default();
                                        if df_name.contains("v3d") || df_name.contains("gpu") || df_name.contains("mali") {
                                            if let Ok(load_str) = fs::read_to_string(entry.path().join("load")) {
                                                if let Some(usage_part) = load_str.trim().split('@').next() {
                                                    if let Ok(u) = usage_part.parse::<f64>() {
                                                        let mut g_temp = None;
                                                        if let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
                                                            if let Ok(t) = t_str.trim().parse::<f64>() { g_temp = Some(t / 1000.0); }
                                                        }
                                                        let name = if df_name.contains("v3d") { "VideoCore GPU" } else if df_name.contains("mali") { "Mali GPU" } else { "GPU" };
                                                        return Some(GpuSnapshot { name: name.to_string(), usage: Some(u), mem_used: None, mem_total: None, temp: g_temp });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                        
                                self.last_nvidia_gpu.clone()
                            }
                        

    fn read_processes(&mut self, cpu_delta_total: u64, sort_mode: SortMode, filter: &str) -> Vec<ProcessInfo> {
        let mut rows = Vec::with_capacity(512);
        let mut current_total_per_pid = HashMap::new();
        let filter_lower = filter.to_lowercase();
        let mut buf = [0u8; 1024];

        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name_os = entry.file_name();
                let name = name_os.to_string_lossy();
                let pid: i32 = match name.parse() { Ok(p) => p, Err(_) => continue };

                let stat_path = entry.path().join("stat");
                let bytes_read = match fs::File::open(stat_path) {
                    Ok(mut f) => f.read(&mut buf).unwrap_or(0),
                    Err(_) => 0,
                };
                if bytes_read == 0 { continue; }

                let stat_content = String::from_utf8_lossy(&buf[..bytes_read]);
                if let Some(parsed) = parse_proc_stat(&stat_content) {
                    let total_ticks = parsed.utime + parsed.stime;
                    current_total_per_pid.insert(pid, total_ticks);
                    if !filter.is_empty() && !parsed.name.to_lowercase().contains(&filter_lower) && !pid.to_string().contains(filter) { continue; }
                    let prev_ticks = *self.previous_total_per_pid.get(&pid).unwrap_or(&total_ticks);
                    let delta_ticks = if total_ticks >= prev_ticks { total_ticks - prev_ticks } else { 0 };
                    let cpu_percent = if cpu_delta_total == 0 { 0.0 } else {
                        ((delta_ticks as f64 / cpu_delta_total as f64) * 100.0).min(100.0)
                    };
                    rows.push(ProcessInfo { pid, name: parsed.name, cpu_percent, mem_bytes: (parsed.rss_pages as u64) * self.page_size, threads: parsed.num_threads });
                }
            }
        }
        self.previous_total_per_pid = current_total_per_pid;
        rows.sort_by(|a, b| match sort_mode {
            SortMode::Cpu => b.cpu_percent.partial_cmp(&a.cpu_percent).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.mem_bytes.cmp(&a.mem_bytes)),
            SortMode::Memory => b.mem_bytes.cmp(&a.mem_bytes).then_with(|| b.cpu_percent.partial_cmp(&a.cpu_percent).unwrap_or(std::cmp::Ordering::Equal))
        });
        rows
    }
}

fn parse_meminfo_kb(line: &str) -> u64 { line.split_whitespace().filter_map(|s| s.parse().ok()).next().unwrap_or(0) }

struct ParsedStat { name: String, utime: u64, stime: u64, num_threads: i32, rss_pages: i64 }

fn parse_proc_stat(raw: &str) -> Option<ParsedStat> {
    let open = raw.find('(')?; let close = raw.rfind(')')?; if open >= close { return None; }
    let name = raw[open + 1..close].to_string();
    let fields: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    if fields.len() < 22 { return None; }
    Some(ParsedStat { name, utime: fields[11].parse().unwrap_or(0), stime: fields[12].parse().unwrap_or(0), num_threads: fields[17].parse().unwrap_or(1), rss_pages: fields[21].parse().unwrap_or(0) })
}

// --- Rendering & Utilities ---

fn human_bytes(bytes: u64) -> String {
    let (kb, mb, gb) = (1024.0, 1024.0 * 1024.0, 1024.0 * 1024.0 * 1024.0);
    let v = bytes as f64;
    if v >= gb { format!("{:.2} GiB", v / gb) } else if v >= mb { format!("{:.1} MiB", v / mb) } else if v >= kb { format!("{:.1} KiB", v / kb) } else { format!("{} B", bytes) }
}

fn term_size() -> (usize, usize) {
    let mut ws: winsize = unsafe { std::mem::zeroed() };
    if unsafe { ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row as usize, ws.ws_col as usize)
    } else {
        let rows = std::env::var("LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(24);
        let cols = std::env::var("COLUMNS").ok().and_then(|s| s.parse().ok()).unwrap_or(80);
        (rows, cols)
    }
}


fn pad_right(s: &str, width: usize) -> String { if s.len() >= width { s[..width].to_string() } else { format!("{:<width$}", s, width = width) } }
fn pad_left(s: &str, width: usize) -> String { if s.len() >= width { s[s.len() - width..].to_string() } else { format!("{:>width$}", s, width = width) } }
fn clip_line(s: &str, cols: usize) -> String { if s.len() <= cols { s.to_string() } else { s[..cols].to_string() } }

fn append_line(out: &mut String, line: &str, cols: usize, selected: bool) {
    if selected { out.push_str("\x1B[0m\x1B[2K\x1B[7m"); out.push_str(&clip_line(line, cols)); out.push_str("\x1B[0m\n"); }
    else { out.push_str("\x1B[0m\x1B[2K"); out.push_str(&clip_line(line, cols)); out.push('\n'); }
}

fn render(cpu: f64, cpu_temp: Option<f64>, cpu_freq: Option<f64>, mem: &MemorySnapshot, net: &NetworkSnapshot, gpu: &Option<GpuSnapshot>, procs: &[ProcessInfo], selected: usize, cpu_count: usize, sort: SortMode, filter: &str, is_searching: bool) {
    let (rows, cols) = term_size();
    let header_height = 12;
    let visible_rows = (rows as i32 - header_height as i32 - 3).max(5) as usize;
    let safe_sel = selected.min(procs.len().saturating_sub(1));
    let scroll_top = (safe_sel as i32 - (visible_rows as i32 / 2)).max(0).min((procs.len() as i32 - visible_rows as i32).max(0)) as usize;
    let mut out = String::new();
    out.push_str("\x1B[H"); // Cursor Home
    append_line(&mut out, &format!("utop    CPUs: {}", cpu_count), cols, false);
    let freq_str = cpu_freq.map(|f| format!(" @ {:.2} GHz", f / 1000.0)).unwrap_or_default();
    append_line(&mut out, &format!("CPU: {:5.1}%{}{}", cpu, freq_str, cpu_temp.map(|t| format!(" {:4.1}°C", t)).unwrap_or_default()), cols, false);
    append_line(&mut out, &format!("MEM: {:5.1}%  {} / {}", mem.combined_percent(), human_bytes(mem.combined_used()), human_bytes(mem.combined_total())), cols, false);
    append_line(&mut out, &format!("SWP: {:5.1}%  {} / {}", mem.swap_percent(), human_bytes(mem.swap_used_bytes), human_bytes(mem.swap_total_bytes)), cols, false);
    if let Some(g) = gpu {
        let vram = if let (Some(u), Some(t)) = (g.mem_used, g.mem_total) { format!("  VRAM: {} / {}", human_bytes(u), human_bytes(t)) } else { "".to_string() };
        append_line(&mut out, &format!("{}: {}{}{}", g.name, g.usage.map(|u| format!("{:5.1}%", u)).unwrap_or_else(|| " - %".to_string()), g.temp.map(|t| format!(" {:4.1}°C", t)).unwrap_or_default(), vram), cols, false);
    } else { append_line(&mut out, "GPU: - %", cols, false); }
    append_line(&mut out, &format!("NET: {}  rx {}/s  tx {}/s", net.iface, human_bytes(net.rx_rate as u64), human_bytes(net.tx_rate as u64)), cols, false);
    append_line(&mut out, "Controls: q quit, j/k/arrows move, h/l/arrows sort, / search", cols, false);
    if is_searching { append_line(&mut out, &format!("\x1B[1;32mSearch: /{filter}\x1B[0m\x1B[5m_\x1B[0m"), cols, false); }
    else if !filter.is_empty() { append_line(&mut out, &format!("Filter: {filter} (press / to edit)"), cols, false); } else { append_line(&mut out, "", cols, false); }
    append_line(&mut out, "", cols, false);
    let (p_c, c_c, m_c, t_c) = (7usize, 8usize, 12usize, 4usize);
    let n_c = (cols as i32 - (p_c as i32 + c_c as i32 + m_c as i32 + t_c as i32 + 10)).max(12) as usize;
    let h = format!("{} {} {} {} {}", pad_right("PID", p_c), pad_right("NAME", n_c), pad_left(if sort == SortMode::Cpu { "CPU%▼" } else { "CPU%" }, c_c), pad_left(if sort == SortMode::Memory { "MEM▼" } else { "MEM" }, m_c), pad_left("THR", t_c));
    append_line(&mut out, &h, cols, false);
    append_line(&mut out, &"-".repeat(cols.min(h.len() + 4)), cols, false);
    for idx in scroll_top..(scroll_top + visible_rows).min(procs.len()) {
        let p = &procs[idx];
        let name = if p.name.len() > n_c { format!("{}..", &p.name[..n_c.saturating_sub(2)]) } else { p.name.clone() };
        append_line(&mut out, &format!("{} {} {} {} {}", pad_right(&p.pid.to_string(), p_c), pad_right(&name, n_c), pad_left(&format!("{:.1}", p.cpu_percent), c_c), pad_left(&human_bytes(p.mem_bytes), m_c), pad_left(&p.threads.to_string(), t_c)), cols, idx == safe_sel);
    }
    if procs.is_empty() { append_line(&mut out, "No processes available.", cols, false); }
    append_line(&mut out, "", cols, false);
    append_line(&mut out, &format!("Showing {}-{} of {}", scroll_top + 1, (scroll_top + visible_rows).min(procs.len()), procs.len()), cols, false);
    out.push_str("\x1B[J"); write_esc(&out);
}

// --- Main Loop & Input ---

fn read_key() -> Option<Key> {
    let mut buf = [0u8; 8];
    let n = unsafe { libc::read(STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n <= 0 { return None; }
    let bytes = &buf[..n as usize];
    if bytes.contains(&3) { return Some(Key::Quit); }
    if bytes.len() == 1 {
        match bytes[0] { 27 => Some(Key::Esc), 127 | 8 => Some(Key::Backspace), 10 | 13 => Some(Key::Enter), 32..=126 => Some(Key::Char(bytes[0] as char)), _ => None }
    } else if bytes.len() >= 3 && bytes[0] == 27 && bytes[1] == 91 {
        match bytes[2] { 65 => Some(Key::Up), 66 => Some(Key::Down), 67 => Some(Key::Right), 68 => Some(Key::Left), _ => None }
    } else { None }
}

fn main() {
    if unsafe { libc::isatty(STDIN_FILENO) } != 1 || unsafe { libc::isatty(STDOUT_FILENO) } != 1 { eprintln!("utop requires an interactive terminal."); std::process::exit(1); }
    unsafe {
        signal(SIGINT, utop_signal_handler as *const () as usize);
        signal(SIGTERM, utop_signal_handler as *const () as usize);
        signal(SIGHUP, utop_signal_handler as *const () as usize);
        signal(SIGQUIT, utop_signal_handler as *const () as usize);
    }
    let mut terminal = TerminalRawMode::new().expect("failed to init terminal");
    let mut sampler = Sampler::new();
    let (mut sel, mut sort, mut filter, mut is_search) = (0usize, SortMode::Cpu, String::new(), false);
    
    let sample_interval = Duration::from_millis(500);
    let render_interval = Duration::from_micros(1_000_000 / 30);
    
    let mut latest = sampler.sample(sort, &filter);
    let mut next_s = Instant::now() + sample_interval;
    let mut next_r = Instant::now() + render_interval;
    let mut running = true;
    let mut needs_r = true;

    while running {
        let now = Instant::now();
        let timeout = if needs_r { next_r.saturating_duration_since(now).min(next_s.saturating_duration_since(now)) } else { next_s.saturating_duration_since(now) };
        let mut fds = [pollfd { fd: STDIN_FILENO, events: POLLIN, revents: 0 }];
        let poll_ret = unsafe { poll(fds.as_mut_ptr(), 1, timeout.as_millis() as i32) };
        let mut input = false;
        let old_f = filter.clone();
        if poll_ret > 0 && (fds[0].revents & POLLIN) != 0 {
            while let Some(key) = read_key() {
                input = true;
                if is_search {
                    match key {
                        Key::Quit => running = false,
                        Key::Enter | Key::Esc => {
                            if let Key::Esc = key {
                                filter.clear();
                            }
                            is_search = false;
                        }
                        Key::Backspace => {
                            if filter.is_empty() {
                                is_search = false;
                            } else {
                                filter.pop();
                            }
                        }
                        Key::Char(c) => {
                            filter.push(c);
                        }
                        _ => {}
                    }
                } else {
                    match key {
                        Key::Quit => running = false,
                        Key::Up => sel = sel.saturating_sub(1),
                        Key::Down => sel += 1,
                        Key::Left => sort = SortMode::Cpu,
                        Key::Right => sort = SortMode::Memory,
                        Key::Char(c) => match c {
                            'q' => running = false,
                            'j' => sel += 1,
                            'k' => sel = sel.saturating_sub(1),
                            'h' => sort = SortMode::Cpu,
                            'l' => sort = SortMode::Memory,
                            '/' => is_search = true,
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
        if filter != old_f { sel = 0; }
        if input { if is_search || !filter.is_empty() || sort != SortMode::Cpu { latest = sampler.sample(sort, &filter); } sel = sel.min(latest.6.len().saturating_sub(1)); needs_r = true; }
        let now_p = Instant::now();
        if now_p >= next_s { latest = sampler.sample(sort, &filter); sel = sel.min(latest.6.len().saturating_sub(1)); needs_r = true; next_s = now_p + Duration::from_millis(500); }
        if needs_r && now_p >= next_r { render(latest.0, latest.1, latest.2, &latest.3, &latest.4, &latest.5, &latest.6, sel, latest.7, sort, &filter, is_search); needs_r = false; next_r = now_p + Duration::from_micros(1_000_000 / 30); }
    }
    terminal.restore_now();
}
