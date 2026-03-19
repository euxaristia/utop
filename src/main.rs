use std::collections::HashMap;
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::time::{Duration, Instant};

#[derive(Default, Clone)]
struct CpuTimes {
    user: u64, nice: u64, sys: u64, idle: u64,
    iowait: u64, irq: u64, softirq: u64, steal: u64,
}

#[derive(Default, Clone)]
struct MemorySnapshot {
    used_bytes: u64, total_bytes: u64,
    swap_used_bytes: u64, swap_total_bytes: u64,
    cma_used_bytes: u64, cma_total_bytes: u64,
}

#[derive(Clone)]
struct GpuSnapshot {
    name: String,
    usage: f64,
    mem_used: u64, mem_total: u64,
    temp: f64,
    has_usage: bool, has_mem: bool, has_temp: bool,
}

impl Default for GpuSnapshot {
    fn default() -> Self {
        Self {
            name: "GPU".to_string(),
            usage: 0.0, mem_used: 0, mem_total: 0, temp: -1000.0,
            has_usage: false, has_mem: false, has_temp: false,
        }
    }
}

#[derive(Clone)]
struct ProcessInfo {
    pid: i32,
    name: String,
    cpu_percent: f64,
    mem_bytes: u64,
    threads: i32,
}

#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Cpu,
    Mem,
}

#[derive(Default, Clone)]
struct NetworkSnapshot {
    iface: String,
    rx_rate: f64,
    tx_rate: f64,
}

#[derive(Default, Clone)]
struct StorageSnapshot {
    mount_point: String,
    device: String,
    used_bytes: u64,
    total_bytes: u64,
}

struct Terminal {
    original_termios: libc::termios,
}

impl Terminal {
    fn init() -> io::Result<Self> {
        unsafe {
            let mut original_termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original_termios) != 0 {
                return Err(io::Error::last_os_error());
            }

            let mut raw = original_termios;
            raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw);

            let flags = libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL);
            libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);

            print!("\x1B[?1049h\x1B[2J\x1B[H\x1B[?25l");
            io::stdout().flush()?;

            Ok(Self { original_termios })
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original_termios);
        }
        print!("\x1B[?1049l\x1B[?25h\x1B[0m");
        let _ = io::stdout().flush();
    }
}

// Global flag for signals
static mut QUIT: bool = false;

extern "C" fn signal_handler(_sig: libc::c_int) {
    unsafe { QUIT = true; }
}

#[derive(Default, Clone)]
struct V3dStats {
    queue: String,
    last_ts: u64,
    last_rt: u64,
}

struct Sampler {
    prev_cpu: CpuTimes,
    prev_ticks: HashMap<i32, u64>,
    prev_net: HashMap<String, (u64, u64)>,
    last_sample: Instant,
    page_size: i64,
    v3d_stats: Vec<V3dStats>,
    cpu_count: String,
    cpu_name: String,
    gpu_cores: String,
    procs: Vec<ProcessInfo>,
    cpu_temp_path: Option<String>,
    cpu_freq_paths: Vec<String>,
}

impl Sampler {
    fn new() -> Self {
        Self {
            prev_cpu: CpuTimes::default(),
            prev_ticks: HashMap::new(),
            prev_net: HashMap::new(),
            last_sample: Instant::now(),
            page_size: unsafe { libc::sysconf(libc::_SC_PAGESIZE) },
            v3d_stats: Vec::new(),
            cpu_count: read_cpu_count(),
            cpu_name: read_cpu_name(),
            gpu_cores: read_gpu_cores(),
            procs: Vec::new(),
            cpu_temp_path: None,
            cpu_freq_paths: Vec::new(),
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    let v = bytes as f64;
    if v >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", v / (1024.0 * 1024.0 * 1024.0))
    } else if v >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", v / (1024.0 * 1024.0))
    } else if v >= 1024.0 {
        format!("{:.1} KiB", v / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn read_cpu_temp(cached_path: &mut Option<String>) -> f64 {
    if let Some(ref path) = *cached_path {
        if let Ok(temp_str) = fs::read_to_string(path)
            && let Ok(t) = temp_str.trim().parse::<f64>() {
                return t / 1000.0;
            }
    }
    // Discovery: scan once and cache the winning path
    if let Ok(entries) = fs::read_dir("/sys/class/thermal") {
        for entry in entries.flatten() {
            let name_str = entry.file_name().to_string_lossy().to_string();
            if name_str.starts_with("thermal_zone")
                && let Ok(type_str) = fs::read_to_string(entry.path().join("type")) {
                    let type_lower = type_str.to_lowercase();
                    if type_lower.contains("pkg") || type_lower.contains("cpu") ||
                       type_lower.contains("core") || type_lower.contains("soc") {
                        let temp_path = entry.path().join("temp");
                        if let Ok(temp_str) = fs::read_to_string(&temp_path)
                            && let Ok(t) = temp_str.trim().parse::<f64>() {
                                *cached_path = Some(temp_path.to_string_lossy().into_owned());
                                return t / 1000.0;
                            }
                    }
                }
        }
    }
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            if let Ok(name_str) = fs::read_to_string(entry.path().join("name")) {
                let name_lower = name_str.to_lowercase();
                if name_lower.contains("coretemp") || name_lower.contains("cpu") || name_lower.contains("k10temp") {
                    let mut best = -1000.0;
                    let mut best_path = String::new();
                    if let Ok(subentries) = fs::read_dir(entry.path()) {
                        for subentry in subentries.flatten() {
                            let sname_str = subentry.file_name().to_string_lossy().to_string();
                            if sname_str.starts_with("temp") && sname_str.contains("_input")
                                && let Ok(t_str) = fs::read_to_string(subentry.path())
                                    && let Ok(t) = t_str.trim().parse::<f64>() {
                                        let t_c = t / 1000.0;
                                        if t_c > best {
                                            best = t_c;
                                            best_path = subentry.path().to_string_lossy().into_owned();
                                        }
                                    }
                        }
                    }
                    if best > -1000.0 {
                        *cached_path = Some(best_path);
                        return best;
                    }
                }
            }
        }
    }
    -1000.0
}

fn read_cpu_freq(cached_paths: &mut Vec<String>) -> f64 {
    // Discover sysfs freq paths once and cache them
    if cached_paths.is_empty() {
        if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
            for entry in entries.flatten() {
                let name_str = entry.file_name().to_string_lossy().to_string();
                if name_str.starts_with("cpu") && name_str.len() > 3
                    && name_str.chars().nth(3).unwrap().is_ascii_digit() {
                    let freq_path = entry.path().join("cpufreq/scaling_cur_freq");
                    if fs::metadata(&freq_path).is_ok() {
                        cached_paths.push(freq_path.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    if !cached_paths.is_empty() {
        let mut total = 0.0;
        let mut count = 0;
        for path in cached_paths.iter() {
            if let Ok(khz_str) = fs::read_to_string(path)
                && let Ok(khz) = khz_str.trim().parse::<f64>() {
                    total += khz / 1000.0;
                    count += 1;
                }
        }
        if count > 0 { return total / count as f64; }
    }
    // Fall back to /proc/cpuinfo
    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        let mut total = 0.0;
        let mut count = 0;
        for line in content.lines() {
            if line.starts_with("cpu MHz")
                && let Some(p) = line.split(':').nth(1)
                    && let Ok(freq) = p.trim().parse::<f64>() {
                        total += freq;
                        count += 1;
                    }
        }
        if count > 0 {
            return total / count as f64;
        }
    }
    0.0
}

fn read_gpu_cores() -> String {
    // Try NVIDIA
    if let Ok(output) = std::process::Command::new("/usr/bin/nvidia-settings")
        .args(["-q", "CUDACores", "-t"])
        .output()
    {
        if output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(cores) = out_str.trim().parse::<u32>() {
                return format!("{} CUDA Cores", cores);
            }
        }
    }

    // Try AMD
    if let Ok(entries) = fs::read_dir("/sys/class/kfd/kfd/topology/nodes/") {
        let mut max_simd = 0;
        for entry in entries.flatten() {
            if let Ok(content) = fs::read_to_string(entry.path().join("properties")) {
                for line in content.lines() {
                    if line.starts_with("simd_count") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            if let Ok(simd) = parts[1].parse::<u32>() {
                                if simd > max_simd {
                                    max_simd = simd;
                                }
                            }
                        }
                    }
                }
            }
        }
        if max_simd > 0 {
            let sps = max_simd * 64;
            return format!("{} Stream Processors", sps);
        }
    }

    String::new()
}

fn read_cpu_name() -> String {
    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        for line in content.lines() {
            if line.starts_with("model name") {
                if let Some(name) = line.split(':').nth(1) {
                    let mut n = name.trim().replace("(R)", "").replace("(TM)", "").replace("(tm)", "").replace("(r)", "");
                    n = n.replace(" CPU", "").replace(" Processor", "");
                    let parts: Vec<&str> = n.split_whitespace().collect();
                    n = parts.join(" ");
                    n = n.replace("Intel Core ", "");
                    n = n.replace("AMD Ryzen ", "Ryzen ");
                    return n;
                }
            }
        }
    }
    "CPU".to_string()
}

fn read_cpu_times() -> CpuTimes {
    let mut t = CpuTimes::default();
    if let Ok(content) = fs::read_to_string("/proc/stat")
        && let Some(line) = content.lines().next() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 9 && parts[0] == "cpu" {
                t.user = parts[1].parse().unwrap_or(0);
                t.nice = parts[2].parse().unwrap_or(0);
                t.sys = parts[3].parse().unwrap_or(0);
                t.idle = parts[4].parse().unwrap_or(0);
                t.iowait = parts[5].parse().unwrap_or(0);
                t.irq = parts[6].parse().unwrap_or(0);
                t.softirq = parts[7].parse().unwrap_or(0);
                t.steal = parts[8].parse().unwrap_or(0);
            }
        }
    t
}

fn read_memory() -> MemorySnapshot {
    let mut m = MemorySnapshot::default();
    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        let mut total = 0;
        let mut avail = 0;
        let mut s_total = 0;
        let mut s_free = 0;
        let mut cma_total = 0;
        let mut cma_free = 0;
        
        for line in content.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("");
            let val = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
            
            match key {
                "MemTotal:" => total = val,
                "MemAvailable:" => avail = val,
                "SwapTotal:" => s_total = val,
                "SwapFree:" => s_free = val,
                "CmaTotal:" => cma_total = val,
                "CmaFree:" => cma_free = val,
                _ => {}
            }
        }
        m.total_bytes = total * 1024;
        m.used_bytes = total.saturating_sub(avail) * 1024;
        m.swap_total_bytes = s_total * 1024;
        m.swap_used_bytes = s_total.saturating_sub(s_free) * 1024;
        m.cma_total_bytes = cma_total * 1024;
        m.cma_used_bytes = cma_total.saturating_sub(cma_free) * 1024;
    }
    m
}

fn read_cpu_count() -> String {
    let mut cores = 0;
    let mut physical_ids = std::collections::HashSet::new();

    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        for line in content.lines() {
            if line.starts_with("processor") {
                cores += 1;
            } else if line.starts_with("physical id") {
                if let Some(id_str) = line.split(':').nth(1) {
                    if let Ok(id) = id_str.trim().parse::<u32>() {
                        physical_ids.insert(id);
                    }
                }
            }
        }
    }

    if cores == 0 {
        if let Ok(content) = fs::read_to_string("/proc/stat") {
            for line in content.lines() {
                if line.starts_with("cpu") && line.len() > 3 && line.chars().nth(3).unwrap().is_ascii_digit() {
                    cores += 1;
                }
            }
        }
    }
    if cores == 0 { cores = 1; }

    let sockets = if physical_ids.is_empty() { 1 } else { physical_ids.len() };

    let cpu_str = if sockets == 1 { "CPU" } else { "CPUs" };
    let core_str = if cores == 1 { "core" } else { "cores" };

    format!("{} {} {} {}", sockets, cpu_str, cores, core_str)
}

fn read_gpu(s: &mut Sampler, mem: &MemorySnapshot, cached_gpu: &mut GpuSnapshot, last_gpu_read: &mut Instant) -> GpuSnapshot {
    let now = Instant::now();
    if now.duration_since(*last_gpu_read) < Duration::from_millis(800) && last_gpu_read.elapsed() < Duration::from_secs(10000) {
        return cached_gpu.clone();
    }
    *last_gpu_read = now;

    let mut g = GpuSnapshot::default();

    // 1. NVIDIA via nvidia-smi
    if let Ok(output) = std::process::Command::new("/usr/bin/nvidia-smi")
        .args(["--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
        && output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = out_str.lines().next() {
                let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
                if parts.len() >= 5 {
                    g.name = parts[0].to_string().replace("NVIDIA GeForce ", "").replace("NVIDIA ", "");
                    g.usage = parts[1].parse().unwrap_or(0.0);
                    g.has_usage = true;
                    g.mem_used = parts[2].parse::<u64>().unwrap_or(0) * 1024 * 1024;
                    g.has_mem = true;
                    g.mem_total = parts[3].parse::<u64>().unwrap_or(0) * 1024 * 1024;
                    g.temp = parts[4].parse().unwrap_or(-1000.0);
                    g.has_temp = true;
                    *cached_gpu = g.clone();
                    return g;
                }
            }
        }

    // 2. DRM / sysfs
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name_str = entry.file_name().to_string_lossy().to_string();
            if name_str.starts_with("card") && !name_str.contains('-') {
                let mut found_usage = false;
                
                let usage_files = [
                    format!("/sys/class/drm/{}/device/gpu_busy_percent", name_str),
                    format!("/sys/class/drm/{}/gt/gt0/usage", name_str),
                    format!("/sys/class/drm/{}/device/usage", name_str),
                    format!("/sys/class/drm/{}/device/load", name_str),
                ];
                
                for path in &usage_files {
                    if let Ok(val) = fs::read_to_string(path)
                        && let Ok(usage) = val.trim().parse::<f64>() {
                            g.usage = usage;
                            g.has_usage = true;
                            found_usage = true;
                            break;
                        }
                }
                
                if !found_usage {
                    let mut stats_path = format!("/sys/class/drm/{}/device/gpu_stats", name_str);
                    if fs::metadata(&stats_path).is_err()
                        && let Some(card_num) = name_str.chars().nth(4)
                            && card_num.is_ascii_digit() {
                                stats_path = format!("/sys/kernel/debug/dri/{}/gpu_stats", card_num);
                            }
                    if let Ok(content) = fs::read_to_string(&stats_path) {
                        for line in content.lines().skip(1) {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 4 {
                                let q_name = parts[0];
                                if let (Ok(ts), Ok(rt)) = (parts[1].parse::<u64>(), parts[3].parse::<u64>()) {
                                    let mut idx = None;
                                    for (i, st) in s.v3d_stats.iter().enumerate() {
                                        if st.queue == q_name {
                                            idx = Some(i);
                                            break;
                                        }
                                    }
                                    if let Some(i) = idx {
                                        if ts > s.v3d_stats[i].last_ts {
                                            let q_u = (rt - s.v3d_stats[i].last_rt) as f64 * 100.0 / (ts - s.v3d_stats[i].last_ts) as f64;
                                            if q_u > g.usage { g.usage = q_u; }
                                            g.has_usage = true;
                                        }
                                        s.v3d_stats[i].last_ts = ts;
                                        s.v3d_stats[i].last_rt = rt;
                                    } else if s.v3d_stats.len() < 16 {
                                        s.v3d_stats.push(V3dStats {
                                            queue: q_name.to_string(),
                                            last_ts: ts,
                                            last_rt: rt,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                
                if let Ok(vendor) = fs::read_to_string(format!("/sys/class/drm/{}/device/vendor", name_str)) {
                    if vendor.contains("0x1002") { g.name = "AMD GPU".to_string(); }
                    else if vendor.contains("0x8086") { g.name = "Intel GPU".to_string(); }
                    else if vendor.contains("0x10de") { g.name = "NVIDIA GPU".to_string(); }
                    else if vendor.contains("0x14e4") { g.name = "Broadcom GPU".to_string(); }
                } else if let Ok(uevent) = fs::read_to_string(format!("/sys/class/drm/{}/device/uevent", name_str))
                    && (uevent.contains("DRIVER=v3d") || uevent.contains("DRIVER=vc4")) {
                        g.name = "VideoCore GPU".to_string();
                    }
                
                if let Ok(hdirs) = fs::read_dir(format!("/sys/class/drm/{}/device/hwmon", name_str)) {
                    for hdir in hdirs.flatten() {
                        let hname_str = hdir.file_name().to_string_lossy().to_string();
                        if hname_str.starts_with("hwmon")
                            && let Ok(t_str) = fs::read_to_string(hdir.path().join("temp1_input"))
                                && let Ok(t) = t_str.trim().parse::<f64>() {
                                    g.temp = t / 1000.0;
                                    g.has_temp = true;
                                    break;
                                }
                    }
                }
                if !g.has_temp
                    && let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
                        && let Ok(t) = t_str.trim().parse::<f64>() {
                            g.temp = t / 1000.0;
                            g.has_temp = true;
                        }
                
                if !g.has_mem {
                    if let Ok(m_str) = fs::read_to_string(format!("/sys/class/drm/{}/tile0/vram0/used", name_str))
                        && let Ok(m) = m_str.trim().parse::<u64>() {
                            g.mem_used = m;
                            g.has_mem = true;
                        }
                    if let Ok(m_str) = fs::read_to_string(format!("/sys/class/drm/{}/tile0/vram0/size", name_str))
                        && let Ok(m) = m_str.trim().parse::<u64>() {
                            g.mem_total = m;
                        }
                }
                
                if (g.name == "Broadcom GPU" || g.name == "VideoCore GPU" || g.name == "GPU")
                    && mem.cma_total_bytes > 0 {
                        g.mem_used = mem.cma_used_bytes;
                        g.mem_total = mem.cma_total_bytes;
                        g.has_mem = true;
                        if g.name == "GPU" { g.name = "VideoCore GPU".to_string(); }
                    }
                
                if g.has_usage {
                    *cached_gpu = g.clone();
                    return g;
                }
            }
        }
    }

    // 3. Adreno / kgsl
    let adreno_paths = ["/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage", "/sys/class/kgsl/kgsl-3d0/gpubusy"];
    for (i, path) in adreno_paths.iter().enumerate() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut usage = 0.0;
            if i == 1 {
                let parts: Vec<&str> = content.split_whitespace().collect();
                if parts.len() >= 2
                    && let (Ok(busy), Ok(total)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>())
                        && total > 0 { usage = busy as f64 * 100.0 / total as f64; }
            } else if let Ok(val) = content.trim().parse::<f64>() {
                usage = val;
            }
            if usage > 0.0 || i == 0 {
                g.name = "Adreno GPU".to_string();
                g.usage = usage;
                g.has_usage = true;
                if let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
                    && let Ok(t) = t_str.trim().parse::<f64>() {
                        g.temp = t / 1000.0;
                        g.has_temp = true;
                    }
                *cached_gpu = g.clone();
                return g;
            }
        }
    }

    // 4. Generic devfreq
    let devfreq_dirs = ["/sys/class/devfreq", "/sys/devices/platform/soc/soc:gpu/devfreq"];
    for dir in devfreq_dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name_str = entry.file_name().to_string_lossy().to_string();
                if (name_str.contains("v3d") || name_str.contains("gpu") || name_str.contains("mali") || name_str.contains("soc:gpu"))
                    && let Ok(load_str) = fs::read_to_string(entry.path().join("load")) {
                        let load = load_str.split('@').next().unwrap_or("").trim();
                        if let Ok(usage) = load.parse::<f64>() {
                            g.usage = usage;
                            g.has_usage = true;
                            if name_str.contains("v3d") || name_str.contains("soc:gpu") { g.name = "VideoCore GPU".to_string(); }
                            else if name_str.contains("mali") { g.name = "Mali GPU".to_string(); }
                            
                            if !g.has_temp
                                && let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
                                    && let Ok(t) = t_str.trim().parse::<f64>() {
                                        g.temp = t / 1000.0;
                                        g.has_temp = true;
                                    }
                            *cached_gpu = g.clone();
                            return g;
                        }
                    }
            }
        }
    }

    // 5. Fallback for SoC (Broadcom/VideoCore)
    if !g.has_usage && !g.has_mem && mem.cma_total_bytes > 0 {
        g.name = "VideoCore GPU".to_string();
        g.mem_used = mem.cma_used_bytes;
        g.mem_total = mem.cma_total_bytes;
        g.has_mem = true;
        if !g.has_temp
            && let Ok(t_str) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
                && let Ok(t) = t_str.trim().parse::<f64>() {
                    g.temp = t / 1000.0;
                    g.has_temp = true;
                }
    }

    *cached_gpu = g.clone();
    g
}

fn read_network(prev_net: &mut HashMap<String, (u64, u64)>, elapsed: f64) -> NetworkSnapshot {
    let mut best = NetworkSnapshot { iface: "-".to_string(), rx_rate: 0.0, tx_rate: 0.0 };
    if let Ok(content) = fs::read_to_string("/proc/net/dev") {
        let mut cur_net: HashMap<String, (u64, u64)> = HashMap::new();
        let mut best_total = 0;

        for line in content.lines().skip(2) {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() == 2 {
                let iface = parts[0].trim().to_string();
                if iface == "lo" { continue; }
                let stats: Vec<&str> = parts[1].split_whitespace().collect();
                if stats.len() >= 9
                    && let (Ok(rx), Ok(tx)) = (stats[0].parse::<u64>(), stats[8].parse::<u64>()) {
                        let (prev_rx, prev_tx) = prev_net.get(&iface).copied().unwrap_or((rx, tx));
                        let rx_r = if rx >= prev_rx { (rx - prev_rx) as f64 / elapsed } else { 0.0 };
                        let tx_r = if tx >= prev_tx { (tx - prev_tx) as f64 / elapsed } else { 0.0 };
                        if rx + tx > best_total {
                            best_total = rx + tx;
                            best.iface = iface.clone();
                            best.rx_rate = rx_r;
                            best.tx_rate = tx_r;
                        }
                        cur_net.insert(iface, (rx, tx));
                    }
            }
        }
        *prev_net = cur_net;
    }
    best
}

fn read_storage() -> Vec<StorageSnapshot> {
    let mut snapshots = Vec::new();
    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let device = parts[0];
                let mount_point = parts[1];
                let _fs_type = parts[2];

                // Skip pseudo-filesystems, only want physical disks
                if !device.starts_with("/dev/") { continue; }
                
                // Avoid duplicates (e.g. same partition mounted in multiple places)
                if snapshots.iter().any(|s: &StorageSnapshot| s.mount_point == mount_point) { continue; }

                let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                if let Ok(c_mount_point) = std::ffi::CString::new(mount_point) {
                    if unsafe { libc::statvfs(c_mount_point.as_ptr(), &mut stat) } == 0 {
                        let frsize = if stat.f_frsize > 0 { stat.f_frsize as u64 } else { stat.f_bsize as u64 };
                        let total = stat.f_blocks as u64 * frsize;
                        let free = stat.f_bfree as u64 * frsize;
                        let used = total.saturating_sub(free);
                        
                        if total > 0 {
                            snapshots.push(StorageSnapshot {
                                mount_point: mount_point.to_string(),
                                device: device.to_string(),
                                used_bytes: used,
                                total_bytes: total,
                            });
                        }
                    }
                }
            }
        }
    }
    // Sort by total size (descending), ensuring / is always first
    snapshots.sort_by(|a, b| {
        if a.mount_point == "/" { std::cmp::Ordering::Less }
        else if b.mount_point == "/" { std::cmp::Ordering::Greater }
        else {
            b.total_bytes.cmp(&a.total_bytes)
                .then_with(|| a.mount_point.cmp(&b.mount_point))
        }
    });
    snapshots
}

fn sample(
    s: &mut Sampler, sort: SortMode, filter: &str,
    out_cpu: &mut f64, out_mem: &mut MemorySnapshot, out_net: &mut NetworkSnapshot,
    out_gpu: &mut GpuSnapshot, out_storage: &mut Vec<StorageSnapshot>, out_cpus: &mut String, cached_gpu: &mut GpuSnapshot, last_gpu_read: &mut Instant
) {
    let now = Instant::now();
    let mut elapsed = now.duration_since(s.last_sample).as_secs_f64();
    if elapsed < 0.001 { elapsed = 0.001; }
    s.last_sample = now;

    let cur_cpu = read_cpu_times();
    let total_prev = s.prev_cpu.user + s.prev_cpu.nice + s.prev_cpu.sys + s.prev_cpu.idle + s.prev_cpu.iowait + s.prev_cpu.irq + s.prev_cpu.softirq + s.prev_cpu.steal;
    let total_cur = cur_cpu.user + cur_cpu.nice + cur_cpu.sys + cur_cpu.idle + cur_cpu.iowait + cur_cpu.irq + cur_cpu.softirq + cur_cpu.steal;
    let total_delta = total_cur.saturating_sub(total_prev);
    let idle_delta = (cur_cpu.idle + cur_cpu.iowait).saturating_sub(s.prev_cpu.idle + s.prev_cpu.iowait);

    *out_cpu = if total_delta > 0 { (total_delta - idle_delta) as f64 * 100.0 / total_delta as f64 } else { 0.0 };
    *out_mem = read_memory();
    *out_net = read_network(&mut s.prev_net, elapsed);
    *out_gpu = read_gpu(s, out_mem, cached_gpu, last_gpu_read);
    *out_storage = read_storage();
    *out_cpus = s.cpu_count.clone();

    s.procs.clear();
    let mut new_ticks: HashMap<i32, u64> = HashMap::with_capacity(s.prev_ticks.len());
    let filter_lower = filter.to_lowercase();

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let name_str = fname.to_string_lossy();
            if name_str.chars().next().unwrap_or('a').is_ascii_digit()
                && let Ok(pid) = name_str.parse::<i32>() {
                    let stat_path = entry.path().join("stat");
                    if let Ok(mut fd) = File::open(&stat_path) {
                        let mut buf = [0u8; 1024];
                        if let Ok(n) = fd.read(&mut buf) {
                            let s_str = String::from_utf8_lossy(&buf[..n]);
                            if let Some(p) = s_str.find('(')
                                && let Some(endp) = s_str.rfind(')') {
                                    let proc_name = s_str[p + 1..endp].to_string();

                                    if !filter.is_empty() {
                                        let pid_str = pid.to_string();
                                        if !proc_name.to_lowercase().contains(&filter_lower) && !pid_str.contains(&filter_lower) {
                                            continue;
                                        }
                                    }

                                    let after_paren = &s_str[endp + 2..];
                                    let parts: Vec<&str> = after_paren.split_whitespace().collect();
                                    if parts.len() >= 22
                                        && let (Ok(utime), Ok(stime), Ok(threads), Ok(rss)) = (
                                            parts[11].parse::<u64>(), parts[12].parse::<u64>(),
                                            parts[17].parse::<i32>(), parts[21].parse::<i64>()
                                        ) {
                                            let total_ticks = utime + stime;
                                            let prev_t = s.prev_ticks.get(&pid).copied().unwrap_or(0);
                                            new_ticks.insert(pid, total_ticks);

                                            let cpu_p = if total_delta > 0 {
                                                total_ticks.saturating_sub(prev_t) as f64 * 100.0 / total_delta as f64
                                            } else { 0.0 };

                                            s.procs.push(ProcessInfo {
                                                pid,
                                                name: proc_name,
                                                cpu_percent: cpu_p,
                                                mem_bytes: (rss.max(0) as u64) * s.page_size as u64,
                                                threads,
                                            });
                                        }
                                }
                        }
                    }
                }
        }
    }

    s.prev_ticks = new_ticks;
    s.prev_cpu = cur_cpu;

    s.procs.sort_by(|a, b| {
        if sort == SortMode::Cpu {
            b.cpu_percent.partial_cmp(&a.cpu_percent).unwrap_or(Ordering::Equal)
                .then_with(|| b.mem_bytes.cmp(&a.mem_bytes))
        } else {
            b.mem_bytes.cmp(&a.mem_bytes)
                .then_with(|| b.cpu_percent.partial_cmp(&a.cpu_percent).unwrap_or(Ordering::Equal))
        }
    });
}

enum KeyType {
    None, Quit, Up, Down, Left, Right, Backspace, Enter, Esc, Char(char),
}

fn read_key() -> KeyType {
    let mut buf = [0u8; 16];
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n <= 0 { return KeyType::None; }
    let n = n as usize;

    if buf[0] == 3 { return KeyType::Quit; }

    if n == 1 {
        if buf[0] == 27 { return KeyType::Esc; }
        if buf[0] == 127 || buf[0] == 8 { return KeyType::Backspace; }
        if buf[0] == 10 || buf[0] == 13 { return KeyType::Enter; }
        if buf[0].is_ascii_graphic() || buf[0] == b' ' { return KeyType::Char(buf[0] as char); }
    } else if n >= 3 && buf[0] == 27 && buf[1] == b'[' {
        if buf[2] == b'A' { return KeyType::Up; }
        if buf[2] == b'B' { return KeyType::Down; }
        if buf[2] == b'C' { return KeyType::Right; }
        if buf[2] == b'D' { return KeyType::Left; }
    }
    KeyType::None
}

fn main() {
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, signal_handler as *const () as libc::sighandler_t);
    }

    let _terminal = Terminal::init().ok();
    
    let mut sampler = Sampler::new();
    let mut sort = SortMode::Cpu;
    let mut filter = String::new();
    let mut is_search = false;
    let mut selection = 0_usize;

    let mut last_sample = Instant::now();
    let mut last_render = Instant::now();

    let mut out = BufWriter::with_capacity(16384, io::stdout());
    let mut cpu = 0.0;
    let mut mem = MemorySnapshot::default();
    let mut net = NetworkSnapshot::default();
    let mut gpu = GpuSnapshot::default();
    let mut storage: Vec<StorageSnapshot> = Vec::new();
    let mut cached_gpu = GpuSnapshot::default();
    let mut last_gpu_read = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
    let mut cpus = String::new();
    let mut needs_sample = true;
    let mut needs_render = true;

    let mut cpu_temp = -1000.0;
    let mut cpu_freq = 0.0;

    loop {
        unsafe { if QUIT { break; } }
        let now = Instant::now();

        if now.duration_since(last_sample) >= Duration::from_millis(500) || needs_sample {
            sample(&mut sampler, sort, &filter, &mut cpu, &mut mem, &mut net, &mut gpu, &mut storage, &mut cpus, &mut cached_gpu, &mut last_gpu_read);
            cpu_temp = read_cpu_temp(&mut sampler.cpu_temp_path);
            cpu_freq = read_cpu_freq(&mut sampler.cpu_freq_paths);
            last_sample = now;
            needs_sample = false;
            needs_render = true;
        }

        if needs_render && now.duration_since(last_render) >= Duration::from_millis(16) {
            let mut ws = unsafe { std::mem::zeroed::<libc::winsize>() };
            unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws); }
            
            let _ = write!(out, "\x1B[H");
            let _ = writeln!(out, "utop (Rust version)    CPUs: {}\x1B[K", cpus);

            let temp_str = if cpu_temp > -1000.0 { format!(" {:.1}°C", cpu_temp) } else { String::new() };
            let freq_str = if cpu_freq > 0.0 { format!(" @ {:.2} GHz", cpu_freq / 1000.0) } else { String::new() };

            let _ = writeln!(out, "{}: {:5.1}%{}{}\x1B[K", sampler.cpu_name, cpu, freq_str, temp_str);
            let mem_pct = if mem.total_bytes > 0 { mem.used_bytes as f64 * 100.0 / mem.total_bytes as f64 } else { 0.0 };
            let _ = writeln!(out, "MEM: {:5.1}% {} / {}\x1B[K", mem_pct, human_bytes(mem.used_bytes), human_bytes(mem.total_bytes));

            if mem.swap_total_bytes > 0 {
                let swp_pct = mem.swap_used_bytes as f64 * 100.0 / mem.swap_total_bytes as f64;
                let _ = writeln!(out, "SWP: {:5.1}% {} / {}\x1B[K", swp_pct, human_bytes(mem.swap_used_bytes), human_bytes(mem.swap_total_bytes));
            } else {
                let _ = writeln!(out, "\x1B[K");
            }

            if gpu.has_usage || gpu.has_mem {
                let g_temp = if gpu.has_temp { format!(" {:.1}°C", gpu.temp) } else { String::new() };
                let g_vram = if gpu.has_mem {
                    let pct = if gpu.mem_total > 0 { gpu.mem_used as f64 * 100.0 / gpu.mem_total as f64 } else { 0.0 };
                    format!("  VRAM: {:5.1}% {} / {}", pct, human_bytes(gpu.mem_used), human_bytes(gpu.mem_total))
                } else { String::new() };
                let g_usage = if gpu.has_usage { format!("{:5.1}%", gpu.usage) } else { String::new() };
                let _ = writeln!(out, "{}: {}{}{}\x1B[K", gpu.name, g_usage, g_temp, g_vram);
            } else {
                let _ = writeln!(out, "GPU:\x1B[K");
            }

            if mem.cma_total_bytes > 0 && (!gpu.has_mem || mem.cma_total_bytes != gpu.mem_total) {
                let cma_pct = mem.cma_used_bytes as f64 * 100.0 / mem.cma_total_bytes as f64;
                let _ = writeln!(out, "CMA: {:5.1}% {} / {}\x1B[K", cma_pct, human_bytes(mem.cma_used_bytes), human_bytes(mem.cma_total_bytes));
            }

            let _ = writeln!(out, "NET: {}  rx {}/s  tx {}/s\x1B[K", net.iface, human_bytes(net.rx_rate as u64), human_bytes(net.tx_rate as u64));
            
            for s in storage.iter().take(3) {
                let pct = if s.total_bytes > 0 { s.used_bytes as f64 * 100.0 / s.total_bytes as f64 } else { 0.0 };
                let _ = writeln!(out, "DSK: {:<10} {:5.1}% {} / {} [{}]\x1B[K", s.mount_point, pct, human_bytes(s.used_bytes), human_bytes(s.total_bytes), s.device);
            }

            let _ = writeln!(out, "Controls: q:quit, j/k/arrows:move, h/l/arrows:sort, /:filter [{}]\x1B[K", if is_search { "SEARCHING" } else { "NORMAL" });

            if is_search {
                let _ = writeln!(out, "Filter: /{}_\x1B[K", filter);
            } else if !filter.is_empty() {
                let _ = writeln!(out, "Filter: {} (press / to edit)\x1B[K", filter);
            } else {
                let _ = writeln!(out, "\x1B[K");
            }
            let _ = writeln!(out, "\x1B[K");

            let pid_w = 7;
            let cpu_w = 8;
            let mem_w = 12;
            let thr_w = 4;

            let cpu_hdr = if sort == SortMode::Cpu { "CPU%▼" } else { "CPU%" };
            let mem_hdr = if sort == SortMode::Mem { "MEM▼" } else { "MEM" };

            let sort_extra = (if sort == SortMode::Cpu { 2isize } else { 0 }) + (if sort == SortMode::Mem { 2 } else { 0 });
            let name_w = (ws.ws_col as isize - (pid_w as isize + cpu_w as isize + mem_w as isize + thr_w as isize + 5 + sort_extra)).max(12) as usize;

            let w1 = cpu_w + if sort == SortMode::Cpu { 2 } else { 0 };
            let w2 = mem_w + if sort == SortMode::Mem { 2 } else { 0 };

            let _ = writeln!(out, "{:<pid_w$} {:<name_w$} {:>w1$} {:>w2$} {:>thr_w$}\x1B[K", "PID", "NAME", cpu_hdr, mem_hdr, "THR",
                pid_w=pid_w, name_w=name_w, w1=w1, w2=w2, thr_w=thr_w);

            let max_dashes = ws.ws_col as usize;
            let req_dashes = pid_w + name_w + cpu_w + mem_w + thr_w + 4;
            let num_dashes = max_dashes.min(req_dashes);
            for _ in 0..num_dashes { let _ = write!(out, "-"); }
            let _ = writeln!(out, "\x1B[K");

            let mut header_lines = 11;
            if mem.cma_total_bytes > 0 && (!gpu.has_mem || mem.cma_total_bytes != gpu.mem_total) { header_lines += 1; }
            header_lines += storage.len().min(3);

            let visible = ws.ws_row.saturating_sub(header_lines as u16 + 1) as usize;
            let count = sampler.procs.len();
            if selection >= count && count > 0 { selection = count - 1; }
            if count == 0 { selection = 0; }

            let mut scroll_top = selection.saturating_sub(visible / 2);
            if scroll_top > count.saturating_sub(visible) { scroll_top = count.saturating_sub(visible); }

            for i in scroll_top..count.min(scroll_top + visible) {
                if i == selection { let _ = write!(out, "\x1B[7m"); }

                let p = &sampler.procs[i];
                let mut p_name = p.name.clone();
                if p_name.chars().count() > name_w {
                    p_name = p_name.chars().take(name_w).collect();
                }

                let _ = writeln!(out, "{:<pid_w$} {:<name_w$} {:>w1$.1} {:>mem_w$} {:>thr_w$}\x1B[0m\x1B[K",
                    p.pid, p_name, p.cpu_percent, human_bytes(p.mem_bytes), p.threads,
                    pid_w=pid_w, name_w=name_w, w1=cpu_w, mem_w=mem_w, thr_w=thr_w);
            }
            let _ = write!(out, "\x1B[J");
            if count > 0 {
                let end_idx = count.min(scroll_top + visible);
                let _ = write!(out, "\x1B[{};1HShowing {}-{} of {}\x1B[K", ws.ws_row, scroll_top + 1, end_idx, count);
            }
            let _ = out.flush();
            last_render = now;
            needs_render = false;
        }

        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 10) } > 0 {
            loop {
                let k = read_key();
                match k {
                    KeyType::None => break,
                    KeyType::Quit => { return; }
                    _ => {
                        if is_search {
                            match k {
                                KeyType::Esc => {
                                    is_search = false;
                                    filter.clear();
                                    needs_sample = true;
                                    needs_render = true;
                                }
                                KeyType::Enter => {
                                    is_search = false;
                                    needs_render = true;
                                }
                                KeyType::Backspace => {
                                    if !filter.is_empty() {
                                        filter.pop();
                                        selection = 0;
                                        needs_sample = true;
                                    } else {
                                        is_search = false;
                                        needs_render = true;
                                    }
                                }
                                KeyType::Char(c)
                                    if filter.len() < 63 => {
                                        filter.push(c);
                                        selection = 0;
                                        needs_sample = true;
                                    }
                                _ => {}
                            }
                        } else {
                            match k {
                                KeyType::Up => {
                                    selection = selection.saturating_sub(1);
                                    needs_render = true;
                                }
                                KeyType::Down => {
                                    selection += 1;
                                    needs_render = true;
                                }
                                KeyType::Left => {
                                    sort = SortMode::Cpu;
                                    needs_sample = true;
                                }
                                KeyType::Right => {
                                    sort = SortMode::Mem;
                                    needs_sample = true;
                                }
                                KeyType::Esc
                                    if !filter.is_empty() => {
                                        filter.clear();
                                        selection = 0;
                                        needs_sample = true;
                                    }
                                KeyType::Char(c) => {
                                    if c == 'q' { return; }
                                    if c == 'j' { selection += 1; needs_render = true; }
                                    if c == 'k' { selection = selection.saturating_sub(1); needs_render = true; }
                                    if c == 'h' { sort = SortMode::Cpu; needs_sample = true; }
                                    if c == 'l' { sort = SortMode::Mem; needs_sample = true; }
                                    if c == '/' { is_search = true; filter.clear(); needs_render = true; }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(100), "100 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.00 GiB");
    }
}
