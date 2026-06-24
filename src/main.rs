use std::cmp::Ordering;
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::fmt;
#[cfg(target_os = "macos")]
use std::ffi::{CStr, CString};
#[cfg(target_os = "linux")]
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
#[cfg(target_os = "linux")]
use std::io::Read;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, BOOL, TRUE, FILETIME, CloseHandle};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Console::{
    GetConsoleMode, SetConsoleMode, GetStdHandle, GetConsoleScreenBufferInfo,
    CONSOLE_SCREEN_BUFFER_INFO,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_WINDOW_INPUT,
    SetConsoleCtrlHandler,
    ReadConsoleInputW, GetNumberOfConsoleInputEvents, INPUT_RECORD, KEY_EVENT_RECORD,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{
    GetSystemTimes, OpenProcess, GetProcessTimes,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, GetPerformanceInfo, PERFORMANCE_INFORMATION,
    K32EnumPageFilesW, ENUM_PAGE_FILE_INFORMATION,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
    PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::SystemInformation::{
    GetSystemInfo, SYSTEM_INFO,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetIfTable2, FreeMibTable, MIB_IF_TABLE2,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    GetLogicalDriveStringsW, GetDiskFreeSpaceExW,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Registry::{
    RegOpenKeyExW, RegQueryValueExW, RegCloseKey, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_SZ,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::WaitForSingleObject;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Power::{
    CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION,
};

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Terminal {
    original_termios: libc::termios,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

            print!("\x1B[?1049h\x1B[?7l\x1B[2J\x1B[H\x1B[?25l");
            io::stdout().flush()?;

            Ok(Self { original_termios })
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original_termios);
        }
        print!("\x1B[?7h\x1B[?1049l\x1B[?25h\x1B[0m");
        let _ = io::stdout().flush();
    }
}

#[cfg(target_os = "windows")]
struct Terminal {
    stdin_handle: HANDLE,
    stdout_handle: HANDLE,
    original_stdin_mode: u32,
    original_stdout_mode: u32,
}

#[cfg(target_os = "windows")]
impl Terminal {
    fn init() -> io::Result<Self> {
        unsafe {
            let stdin_handle = GetStdHandle(STD_INPUT_HANDLE);
            let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if stdin_handle == INVALID_HANDLE_VALUE || stdout_handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }

            let mut original_stdin_mode: u32 = 0;
            let mut original_stdout_mode: u32 = 0;
            GetConsoleMode(stdin_handle, &mut original_stdin_mode);
            GetConsoleMode(stdout_handle, &mut original_stdout_mode);

            // Enable VT processing on stdout
            let new_stdout_mode = original_stdout_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT;
            SetConsoleMode(stdout_handle, new_stdout_mode);

            // Disable line input, echo, processed input on stdin; enable window input + VT input for resize/seq events
            let new_stdin_mode = (original_stdin_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_WINDOW_INPUT | ENABLE_VIRTUAL_TERMINAL_INPUT;
            SetConsoleMode(stdin_handle, new_stdin_mode);

            print!("\x1B[?1049h\x1B[?7l\x1B[2J\x1B[H\x1B[?25l");
            io::stdout().flush()?;

            Ok(Self { stdin_handle, stdout_handle, original_stdin_mode, original_stdout_mode })
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.stdin_handle, self.original_stdin_mode);
            SetConsoleMode(self.stdout_handle, self.original_stdout_mode);
        }
        print!("\x1B[?7h\x1B[?1049l\x1B[?25h\x1B[0m");
        let _ = io::stdout().flush();
    }
}

// Global flag for signals
static mut QUIT: bool = false;

#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn signal_handler(_sig: libc::c_int) {
    unsafe { QUIT = true; }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    unsafe { QUIT = true; }
    TRUE
}

#[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    page_size: i64,
    #[cfg(target_os = "linux")]
    v3d_stats: Vec<V3dStats>,
    cpu_count: String,
    cpu_name: String,
    gpu_cores: String,
    procs: Vec<ProcessInfo>,
    cpu_temp_path: Option<String>,
    cpu_freq_paths: Vec<String>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    logical_cpus: u64,
    #[cfg(target_os = "macos")]
    proc_names: HashMap<i32, String>,
}

impl Sampler {
    fn new() -> Self {
        Self {
            prev_cpu: CpuTimes::default(),
            prev_ticks: HashMap::new(),
            prev_net: HashMap::new(),
            last_sample: Instant::now(),
            #[cfg(target_os = "linux")]
            page_size: unsafe { libc::sysconf(libc::_SC_PAGESIZE) },
            #[cfg(target_os = "linux")]
            v3d_stats: Vec::new(),
            cpu_count: read_cpu_count(),
            cpu_name: read_cpu_name(),
            gpu_cores: read_gpu_cores(),
            procs: Vec::new(),
            cpu_temp_path: None,
            cpu_freq_paths: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            logical_cpus: {
                #[cfg(target_os = "macos")]
                { read_logical_cpu_count() }
                #[cfg(target_os = "windows")]
                {
                    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
                    unsafe { GetSystemInfo(&mut info); }
                    info.dwNumberOfProcessors as u64
                }
            },
            #[cfg(target_os = "macos")]
            proc_names: HashMap::new(),
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

fn clip_to_width(line: &str, width: usize) -> String {
    line.chars().take(width).collect()
}

fn draw_line<W: Write>(
    out: &mut W,
    row: u16,
    width: usize,
    reverse: bool,
    args: fmt::Arguments<'_>,
) -> io::Result<()> {
    let line = clip_to_width(&args.to_string(), width);
    if reverse {
        write!(out, "\x1B[{};1H\x1B[0m\x1B[7m{}\x1B[0m\x1B[K", row, line)
    } else {
        write!(out, "\x1B[{};1H\x1B[0m{}\x1B[K", row, line)
    }
}

fn draw_next_line<W: Write>(
    out: &mut W,
    row: &mut u16,
    height: u16,
    width: usize,
    reverse: bool,
    args: fmt::Arguments<'_>,
) {
    if *row <= height {
        let _ = draw_line(out, *row, width, reverse, args);
    }
    *row = (*row).saturating_add(1);
}

#[cfg(target_os = "macos")]
fn sysctl_into<T>(name: &str, value: &mut T) -> bool {
    let Ok(name) = CString::new(name) else { return false; };
    let mut len = std::mem::size_of::<T>() as libc::size_t;
    unsafe {
        libc::sysctlbyname(name.as_ptr(), value as *mut T as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0) == 0
    }
}

#[cfg(target_os = "macos")]
fn mach_host_self() -> libc::host_t {
    #[allow(deprecated)]
    unsafe { libc::mach_host_self() }
}

#[cfg(target_os = "macos")]
fn sysctl_i32(name: &str) -> Option<i32> {
    let mut value = 0_i32;
    sysctl_into(name, &mut value).then_some(value)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let mut value = 0_u64;
    sysctl_into(name, &mut value).then_some(value)
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    unsafe {
        let mut len = 0_usize;
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0_u8; len];
        if libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        while buf.last().copied() == Some(0) {
            buf.pop();
        }
        String::from_utf8(buf).ok().filter(|s| !s.is_empty())
    }
}

#[cfg(target_os = "macos")]
fn c_char_array_to_string(buf: &[libc::c_char]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let bytes: Vec<u8> = buf[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_cpu_temp(_cached_path: &mut Option<String>) -> f64 {
    -1000.0
}

#[cfg(target_os = "windows")]
fn win_reg_read_string(subkey: &[u16], value_name: &[u16]) -> Option<String> {
    unsafe {
        let mut hkey: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }
        let mut buf = [0u16; 512];
        let mut buf_len = (buf.len() * 2) as u32;
        let mut reg_type: u32 = 0;
        let result = RegQueryValueExW(
            hkey, value_name.as_ptr(), std::ptr::null(),
            &mut reg_type, buf.as_mut_ptr() as *mut u8, &mut buf_len,
        );
        RegCloseKey(hkey);
        if result != 0 || reg_type != REG_SZ { return None; }
        let chars = buf_len as usize / 2;
        let s = String::from_utf16_lossy(&buf[..chars]);
        Some(s.trim_end_matches('\0').to_string())
    }
}

#[cfg(target_os = "windows")]
fn win_wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn read_cpu_temp(_cached_path: &mut Option<String>) -> f64 {
    // Try wmic for CPU temperature
    if let Ok(output) = std::process::Command::new("wmic")
        .args(["path", "MSAcpi_ThermalZoneTemperature", "get", "CurrentTemperature", "/value"])
        .output()
        && output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            for line in out_str.lines() {
                if let Some(val) = line.strip_prefix("CurrentTemperature=") {
                    if let Ok(t) = val.trim().parse::<f64>() {
                        return (t - 2732.0) / 10.0;
                    }
                }
            }
        }
    -1000.0
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_cpu_freq(_cached_paths: &mut Vec<String>) -> f64 {
    sysctl_u64("hw.cpufrequency")
        .or_else(|| sysctl_u64("hw.cpufrequency_max"))
        .map(|hz| hz as f64 / 1_000_000.0)
        .unwrap_or(0.0)
}

#[cfg(target_os = "windows")]
fn read_cpu_freq(_cached_paths: &mut Vec<String>) -> f64 {
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        let count = si.dwNumberOfProcessors as usize;
        if count == 0 {
            return 0.0;
        }
        let size = count * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>();
        let mut info: Vec<PROCESSOR_POWER_INFORMATION> = Vec::with_capacity(count);
        let ret = CallNtPowerInformation(
            ProcessorInformation,
            std::ptr::null(),
            0,
            info.as_mut_ptr() as *mut core::ffi::c_void,
            size as u32,
        );
        if ret == 0 {
            info.set_len(count);
            info[0].CurrentMhz as f64
        } else {
            0.0
        }
    }
}

#[cfg(target_os = "linux")]
fn read_gpu_cores() -> String {
    let mut gpu_count = 0;
    let mut unique_devices: HashSet<String> = HashSet::new();
    let mut has_videocore = false;
    
    // Count physical GPUs via DRM, deduplicating by device path
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("card") && !name.contains('-') {
                let dev_path = format!("/sys/class/drm/{}/device", name);
                let dev_id = fs::canonicalize(&dev_path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| name.clone());
                
                // VideoCore/Broadcom GPUs (Pi4, etc.) expose multiple DRM
                // drivers (vc4 display + v3d render) for one physical GPU
                let is_videocore =
                    fs::read_to_string(format!("{}/uevent", dev_path))
                        .is_ok_and(|u| u.contains("DRIVER=v3d") || u.contains("DRIVER=vc4"))
                    || fs::read_to_string(format!("{}/vendor", dev_path))
                        .is_ok_and(|v| v.contains("0x14e4"));
                
                if is_videocore {
                    has_videocore = true;
                } else if unique_devices.insert(dev_id) {
                    gpu_count += 1;
                }
            }
        }
    }
    if has_videocore {
        gpu_count += 1;
    }
    
    // Fallback to nvidia-smi if DRM didn't find any
    if gpu_count == 0 {
        if let Ok(output) = std::process::Command::new("/usr/bin/nvidia-smi").arg("-L").output() {
            if output.status.success() {
                let out_str = String::from_utf8_lossy(&output.stdout);
                gpu_count = out_str.lines().filter(|l| l.starts_with("GPU")).count();
            }
        }
    }

    let gpu_str = if gpu_count == 1 { "GPU:" } else { "GPUs:" };
    let count_prefix = if gpu_count > 0 { format!("{} {}, ", gpu_str, gpu_count) } else { String::new() };

    // Try NVIDIA
    if let Ok(output) = std::process::Command::new("/usr/bin/nvidia-settings")
        .args(["-q", "CUDACores", "-t"])
        .output()
    {
        if output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(cores) = out_str.trim().parse::<u32>() {
                return format!("{}{} CUDA Cores", count_prefix, cores);
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
            return format!("{}{} Stream Processors", count_prefix, sps);
        }
    }

    if gpu_count > 0 {
        format!("{} {}", gpu_str, gpu_count)
    } else {
        String::new()
    }
}

#[cfg(target_os = "macos")]
fn read_gpu_cores() -> String {
    String::new()
}

#[cfg(target_os = "windows")]
fn read_gpu_cores() -> String {
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .arg("-L")
        .output()
        && output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            let gpu_count = out_str.lines().filter(|l| l.starts_with("GPU")).count();
            if gpu_count > 0 {
                let gpu_str = if gpu_count == 1 { "GPU:" } else { "GPUs:" };
                return format!("{} {}", gpu_str, gpu_count);
            }
        }
    String::new()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_cpu_name() -> String {
    sysctl_string("machdep.cpu.brand_string")
        .or_else(|| sysctl_string("hw.model"))
        .unwrap_or_else(|| "CPU".to_string())
}

#[cfg(target_os = "windows")]
fn read_cpu_name() -> String {
    let subkey = win_wstr("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
    let value_name = win_wstr("ProcessorNameString");
    if let Some(name) = win_reg_read_string(&subkey, &value_name) {
        let mut n = name.replace("(R)", "").replace("(TM)", "").replace("(tm)", "").replace("(r)", "");
        n = n.replace(" CPU", "").replace(" Processor", "");
        let parts: Vec<&str> = n.split_whitespace().collect();
        n = parts.join(" ");
        n = n.replace("Intel Core ", "");
        n = n.replace("AMD Ryzen ", "Ryzen ");
        return n;
    }
    "CPU".to_string()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_cpu_times() -> CpuTimes {
    let mut t = CpuTimes::default();
    unsafe {
        let mut info = std::mem::zeroed::<libc::host_cpu_load_info>();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let kr = libc::host_statistics64(
            mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as libc::host_info64_t,
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            t.user = info.cpu_ticks[libc::CPU_STATE_USER as usize] as u64;
            t.sys = info.cpu_ticks[libc::CPU_STATE_SYSTEM as usize] as u64;
            t.idle = info.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64;
            t.nice = info.cpu_ticks[libc::CPU_STATE_NICE as usize] as u64;
        }
    }
    t
}

#[cfg(target_os = "windows")]
fn read_cpu_times() -> CpuTimes {
    let mut t = CpuTimes::default();
    unsafe {
        let mut idle_time: FILETIME = std::mem::zeroed();
        let mut kernel_time: FILETIME = std::mem::zeroed();
        let mut user_time: FILETIME = std::mem::zeroed();
        if GetSystemTimes(&mut idle_time, &mut kernel_time, &mut user_time) != 0 {
            let idle = ((idle_time.dwHighDateTime as u64) << 32) | idle_time.dwLowDateTime as u64;
            let kernel = ((kernel_time.dwHighDateTime as u64) << 32) | kernel_time.dwLowDateTime as u64;
            let user = ((user_time.dwHighDateTime as u64) << 32) | user_time.dwLowDateTime as u64;
            // kernel includes idle time on Windows
            t.sys = kernel.saturating_sub(idle);
            t.user = user;
            t.idle = idle;
        }
    }
    t
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_memory() -> MemorySnapshot {
    let mut m = MemorySnapshot::default();
    m.total_bytes = sysctl_u64("hw.memsize").unwrap_or(0);

    unsafe {
        let mut stats = std::mem::zeroed::<libc::vm_statistics64>();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let kr = libc::host_statistics64(
            mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as libc::host_info64_t,
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            let page_size = libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64;
            let active = stats.active_count as u64;
            let wire = stats.wire_count as u64;
            let compressed = stats.compressor_page_count as u64;
            let used = (active + wire + compressed).saturating_mul(page_size);
            m.used_bytes = used.min(m.total_bytes);
        }

        let mut swap = std::mem::zeroed::<libc::xsw_usage>();
        if sysctl_into("vm.swapusage", &mut swap) {
            m.swap_total_bytes = swap.xsu_total;
            m.swap_used_bytes = swap.xsu_used;
        }
    }

    m
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_pf_callback(
    pcontext: *mut core::ffi::c_void,
    ppagefileinfo: *mut ENUM_PAGE_FILE_INFORMATION,
    _lpfilename: windows_sys::core::PCWSTR,
) -> BOOL {
    unsafe {
        let info = &*ppagefileinfo;
        let out = &mut *(pcontext as *mut (u64, u64));
        *out = (info.TotalInUse as u64, info.TotalSize as u64);
    }
    1
}

#[cfg(target_os = "windows")]
fn read_memory() -> MemorySnapshot {
    let mut m = MemorySnapshot::default();
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        let page_size = si.dwPageSize as u64;

        let mut pi: PERFORMANCE_INFORMATION = std::mem::zeroed();
        pi.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
        if GetPerformanceInfo(&mut pi, pi.cb) != 0 {
            m.total_bytes = (pi.PhysicalTotal as u64) * page_size;
            m.used_bytes = (pi.PhysicalTotal.saturating_sub(pi.PhysicalAvailable) as u64) * page_size;
        }

        let mut pf_info: (u64, u64) = (0, 0);
        K32EnumPageFilesW(
            Some(enum_pf_callback),
            &mut pf_info as *mut _ as *mut core::ffi::c_void,
        );
        if pf_info.1 > 0 {
            m.swap_total_bytes = pf_info.1 * page_size;
            m.swap_used_bytes = std::cmp::min(pf_info.0 * page_size, pf_info.1 * page_size);
        }
    }
    m
}

#[cfg(target_os = "linux")]
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

    let cpu_str = if sockets == 1 { "CPU:" } else { "CPUs:" };
    let core_str = if cores == 1 { "core" } else { "cores" };

    format!("{} {}, {} {}", cpu_str, sockets, cores, core_str)
}

#[cfg(target_os = "macos")]
fn read_cpu_count() -> String {
    let cores = read_logical_cpu_count();
    let sockets = sysctl_i32("hw.packages").filter(|v| *v > 0).unwrap_or(1) as u64;
    let cpu_str = if sockets == 1 { "CPU:" } else { "CPUs:" };
    let core_str = if cores == 1 { "core" } else { "cores" };
    format!("{} {}, {} {}", cpu_str, sockets, cores, core_str)
}

#[cfg(target_os = "macos")]
fn read_logical_cpu_count() -> u64 {
    sysctl_i32("hw.logicalcpu")
        .or_else(|| sysctl_i32("hw.ncpu"))
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .unwrap_or(1)
}

#[cfg(target_os = "windows")]
fn read_cpu_count() -> String {
    unsafe {
        let mut info: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut info);
        let cores = info.dwNumberOfProcessors;
        let cpu_str = if cores == 1 { "CPU:" } else { "CPUs:" };
        let core_str = if cores == 1 { "core" } else { "cores" };
        format!("{} {}, {} {}", cpu_str, 1, cores, core_str)
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_gpu(
    _s: &mut Sampler,
    _mem: &MemorySnapshot,
    cached_gpu: &mut GpuSnapshot,
    last_gpu_read: &mut Instant,
) -> GpuSnapshot {
    let now = Instant::now();
    if now.duration_since(*last_gpu_read) < Duration::from_millis(800) {
        return cached_gpu.clone();
    }
    *last_gpu_read = now;

    let g = GpuSnapshot::default();
    *cached_gpu = g.clone();
    g
}

#[cfg(target_os = "windows")]
fn read_gpu(
    _s: &mut Sampler,
    _mem: &MemorySnapshot,
    cached_gpu: &mut GpuSnapshot,
    last_gpu_read: &mut Instant,
) -> GpuSnapshot {
    let now = Instant::now();
    if now.duration_since(*last_gpu_read) < Duration::from_millis(800) {
        return cached_gpu.clone();
    }
    *last_gpu_read = now;

    // Try nvidia-smi
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
        && output.status.success()
    {
        let out_str = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = out_str.lines().next() {
            let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
            if parts.len() >= 5 {
                let mut g = GpuSnapshot::default();
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

    *cached_gpu = GpuSnapshot::default();
    cached_gpu.clone()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_network(prev_net: &mut HashMap<String, (u64, u64)>, elapsed: f64) -> NetworkSnapshot {
    let mut best = NetworkSnapshot { iface: "-".to_string(), rx_rate: 0.0, tx_rate: 0.0 };
    let mut cur_net: HashMap<String, (u64, u64)> = HashMap::new();

    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return best;
        }

        let mut p = addrs;
        let mut best_total = 0_u64;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_LINK
                && !ifa.ifa_data.is_null()
                && (ifa.ifa_flags & libc::IFF_LOOPBACK as u32) == 0
            {
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                let data = &*(ifa.ifa_data as *const libc::if_data);
                let rx = data.ifi_ibytes as u64;
                let tx = data.ifi_obytes as u64;
                let (prev_rx, prev_tx) = prev_net.get(&name).copied().unwrap_or((rx, tx));
                let rx_r = if rx >= prev_rx { (rx - prev_rx) as f64 / elapsed } else { 0.0 };
                let tx_r = if tx >= prev_tx { (tx - prev_tx) as f64 / elapsed } else { 0.0 };
                if rx + tx > best_total {
                    best_total = rx + tx;
                    best.iface = name.clone();
                    best.rx_rate = rx_r;
                    best.tx_rate = tx_r;
                }
                cur_net.insert(name, (rx, tx));
            }
            p = ifa.ifa_next;
        }

        libc::freeifaddrs(addrs);
    }

    *prev_net = cur_net;
    best
}

#[cfg(target_os = "windows")]
fn read_network(prev_net: &mut HashMap<String, (u64, u64)>, elapsed: f64) -> NetworkSnapshot {
    let mut best = NetworkSnapshot { iface: "-".to_string(), rx_rate: 0.0, tx_rate: 0.0 };
    let mut cur_net: HashMap<String, (u64, u64)> = HashMap::new();
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table) != 0 {
            return best;
        }
        let entries = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), entries);
        let mut best_total = 0_u64;
        for row in rows {
            let name = String::from_utf16_lossy(&row.Description).trim_end_matches('\0').to_string();
            let rx = row.InOctets;
            let tx = row.OutOctets;
            let (prev_rx, prev_tx) = prev_net.get(&name).copied().unwrap_or((rx, tx));
            let rx_r = if rx >= prev_rx { (rx - prev_rx) as f64 / elapsed } else { 0.0 };
            let tx_r = if tx >= prev_tx { (tx - prev_tx) as f64 / elapsed } else { 0.0 };
            if rx + tx > best_total {
                best_total = rx + tx;
                best.iface = name.clone();
                best.rx_rate = rx_r;
                best.tx_rate = tx_r;
            }
            cur_net.insert(name, (rx, tx));
        }
        FreeMibTable(table as *mut _);
    }
    *prev_net = cur_net;
    best
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn read_storage() -> Vec<StorageSnapshot> {
    let mut snapshots = Vec::new();
    unsafe {
        let count = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
        if count <= 0 {
            return snapshots;
        }

        let mut mounts = vec![std::mem::zeroed::<libc::statfs>(); count as usize];
        let bytes = (mounts.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
        let actual = libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
        if actual <= 0 {
            return snapshots;
        }

        for mount in mounts.iter().take(actual as usize) {
            let device = c_char_array_to_string(&mount.f_mntfromname);
            if !device.starts_with("/dev/") { continue; }
            let mount_point = c_char_array_to_string(&mount.f_mntonname);
            if snapshots.iter().any(|s: &StorageSnapshot| s.mount_point == mount_point) { continue; }

            let total = mount.f_blocks.saturating_mul(mount.f_bsize as u64);
            let free = mount.f_bfree.saturating_mul(mount.f_bsize as u64);
            let used = total.saturating_sub(free);
            if total > 0 {
                snapshots.push(StorageSnapshot {
                    mount_point,
                    device,
                    used_bytes: used,
                    total_bytes: total,
                });
            }
        }
    }

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

#[cfg(target_os = "windows")]
fn read_storage() -> Vec<StorageSnapshot> {
    let mut snapshots = Vec::new();
    unsafe {
        let mut buf = [0u16; 512];
        if GetLogicalDriveStringsW((buf.len() / 2) as u32, buf.as_mut_ptr()) == 0 {
            return snapshots;
        }
        let mut i = 0;
        while i < buf.len() && buf[i] != 0 {
            let end = i + buf[i..].iter().position(|&c| c == 0).unwrap_or(buf.len() - i);
            let root = String::from_utf16_lossy(&buf[i..end]);
            i = end + 1;
            let root_w = root.encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
            let mut free_bytes: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut total_free: u64 = 0;
            if GetDiskFreeSpaceExW(root_w.as_ptr(), &mut free_bytes, &mut total_bytes, &mut total_free) != 0 && total_bytes > 0 {
                let used = total_bytes.saturating_sub(free_bytes);
                let device = root.trim_end_matches('\\').to_string();
                snapshots.push(StorageSnapshot {
                    mount_point: root,
                    device,
                    used_bytes: used,
                    total_bytes,
                });
            }
        }
    }
    snapshots.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
    snapshots
}

#[cfg_attr(target_os = "windows", allow(unused_variables))]
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
    #[allow(unused_mut)]
    let mut new_ticks: HashMap<i32, u64> = HashMap::with_capacity(s.prev_ticks.len());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let filter_lower = filter.to_lowercase();

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "macos")]
    {
        let initial_bytes = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if initial_bytes > 0 {
            let initial_count = initial_bytes as usize / std::mem::size_of::<i32>();
            let mut pids = vec![0_i32; initial_count + 64];
            let bytes = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
            let actual_bytes = unsafe {
                libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes)
            };

            if actual_bytes > 0 {
                let actual_count = actual_bytes as usize / std::mem::size_of::<i32>();
                let proc_cpu_denominator = elapsed * s.logical_cpus.max(1) as f64 * 1_000_000_000.0;
                for pid in pids.into_iter().take(actual_count).filter(|pid| *pid > 0) {
                    let mut proc_name = String::new();
                    let mut got_name = false;
                    if let Some(name) = s.proc_names.get(&pid) {
                        proc_name = name.clone();
                        got_name = true;
                    }

                    let (total_ticks, resident_size, threadnum) = if got_name {
                        let mut info = unsafe { std::mem::zeroed::<libc::proc_taskinfo>() };
                        let info_size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
                        let read = unsafe {
                            libc::proc_pidinfo(
                                pid,
                                libc::PROC_PIDTASKINFO,
                                0,
                                &mut info as *mut _ as *mut libc::c_void,
                                info_size,
                            )
                        };
                        if read < info_size { continue; }
                        (info.pti_total_user.saturating_add(info.pti_total_system), info.pti_resident_size, info.pti_threadnum)
                    } else {
                        let mut info = unsafe { std::mem::zeroed::<libc::proc_taskallinfo>() };
                        let info_size = std::mem::size_of::<libc::proc_taskallinfo>() as libc::c_int;
                        let read = unsafe {
                            libc::proc_pidinfo(
                                pid,
                                libc::PROC_PIDTASKALLINFO,
                                0,
                                &mut info as *mut _ as *mut libc::c_void,
                                info_size,
                            )
                        };
                        if read < info_size { continue; }
                        let mut name = c_char_array_to_string(&info.pbsd.pbi_name);
                        if name.is_empty() {
                            name = c_char_array_to_string(&info.pbsd.pbi_comm);
                        }
                        if name.is_empty() {
                            name = format!("[{}]", pid);
                        }
                        proc_name = name.clone();
                        s.proc_names.insert(pid, name);
                        (info.ptinfo.pti_total_user.saturating_add(info.ptinfo.pti_total_system), info.ptinfo.pti_resident_size, info.ptinfo.pti_threadnum)
                    };

                    if !filter.is_empty() {
                        let pid_str = pid.to_string();
                        if !proc_name.to_lowercase().contains(&filter_lower) && !pid_str.contains(&filter_lower) {
                            new_ticks.insert(pid, total_ticks);
                            continue;
                        }
                    }

                    let prev_t = s.prev_ticks.get(&pid).copied().unwrap_or(total_ticks);
                    new_ticks.insert(pid, total_ticks);

                    let cpu_p = if proc_cpu_denominator > 0.0 {
                        total_ticks.saturating_sub(prev_t) as f64 * 100.0 / proc_cpu_denominator
                    } else { 0.0 };

                    s.procs.push(ProcessInfo {
                        pid,
                        name: proc_name,
                        cpu_percent: cpu_p,
                        mem_bytes: resident_size,
                        threads: threadnum,
                    });
                }
                s.proc_names.retain(|k, _| new_ticks.contains_key(k));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let filter_lower = filter.to_lowercase();
        let logical_cpus = s.logical_cpus.max(1);
        let proc_cpu_denominator = elapsed * logical_cpus as f64 * 10_000_000.0;
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot != INVALID_HANDLE_VALUE {
                let mut entry: PROCESSENTRY32W = std::mem::zeroed();
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
                while ok {
                    let pid = entry.th32ProcessID as i32;
                    let name = String::from_utf16_lossy(&entry.szExeFile).trim_end_matches('\0').to_string();
                    let threads = entry.cntThreads as i32;

                    if !filter.is_empty() {
                        let pid_str = pid.to_string();
                        if !name.to_lowercase().contains(&filter_lower) && !pid_str.contains(&filter_lower) {
                            ok = Process32NextW(snapshot, &mut entry) != 0;
                            continue;
                        }
                    }

                    let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid as u32);
                    if !handle.is_null() {
                        let mut create_time: FILETIME = std::mem::zeroed();
                        let mut exit_time: FILETIME = std::mem::zeroed();
                        let mut kernel_time: FILETIME = std::mem::zeroed();
                        let mut user_time: FILETIME = std::mem::zeroed();
                        let mut mem_bytes = 0u64;
                        if GetProcessTimes(handle, &mut create_time, &mut exit_time, &mut kernel_time, &mut user_time) != 0 {
                            let kt = ((kernel_time.dwHighDateTime as u64) << 32) | kernel_time.dwLowDateTime as u64;
                            let ut = ((user_time.dwHighDateTime as u64) << 32) | user_time.dwLowDateTime as u64;
                            let total_ticks = kt + ut;
                            let prev_t = s.prev_ticks.get(&pid).copied().unwrap_or(total_ticks);
                            new_ticks.insert(pid, total_ticks);
                            let cpu_p = if proc_cpu_denominator > 0.0 {
                                total_ticks.saturating_sub(prev_t) as f64 * 100.0 / proc_cpu_denominator
                            } else { 0.0 };

                            let mut pmc: windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
                            pmc.cb = std::mem::size_of::<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS>() as u32;
                            if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                                mem_bytes = pmc.WorkingSetSize as u64;
                            }

                            s.procs.push(ProcessInfo {
                                pid,
                                name,
                                cpu_percent: cpu_p,
                                mem_bytes,
                                threads,
                            });
                        }
                        CloseHandle(handle);
                    }
                    ok = Process32NextW(snapshot, &mut entry) != 0;
                }
                CloseHandle(snapshot);
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(target_os = "windows")]
fn read_key() -> KeyType {
    let mut num_events: u32 = 0;
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        // Non-blocking: return None immediately if no events pending
        if GetNumberOfConsoleInputEvents(h, &mut num_events) == 0 || num_events == 0 {
            return KeyType::None;
        }
        let mut record: INPUT_RECORD = std::mem::zeroed();
        let mut events_read: u32 = 0;
        if ReadConsoleInputW(h, &mut record, 1, &mut events_read) == 0 || events_read == 0 {
            return KeyType::None;
        }
        if record.EventType != 1 /* KEY_EVENT */ { return KeyType::None; }
        let ke: KEY_EVENT_RECORD = record.Event.KeyEvent;
        if ke.bKeyDown == 0 { return KeyType::None; }
        let ch = ke.uChar.UnicodeChar;
        if ch != 0 {
            let byte = ch as u8;
            if byte == 3 { return KeyType::Quit; }
            if byte == 27 { return KeyType::Esc; }
            if byte == 127 || byte == 8 { return KeyType::Backspace; }
            if byte == 10 || byte == 13 { return KeyType::Enter; }
            if byte.is_ascii_graphic() || byte == b' ' { return KeyType::Char(byte as char); }
            return KeyType::None;
        }
        match ke.wVirtualKeyCode {
            0x26 /* VK_UP */ => KeyType::Up,
            0x28 /* VK_DOWN */ => KeyType::Down,
            0x25 /* VK_LEFT */ => KeyType::Left,
            0x27 /* VK_RIGHT */ => KeyType::Right,
            _ => KeyType::None,
        }
    }
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, signal_handler as *const () as libc::sighandler_t);
    }
    #[cfg(target_os = "windows")]
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), TRUE);
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let (term_height, term_width) = {
                let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
                unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws); }
                let h = if ws.ws_row == 0 { 24 } else { ws.ws_row };
                let w = if ws.ws_col == 0 { 80 } else { ws.ws_col as usize };
                (h, w)
            };
            #[cfg(target_os = "windows")]
            let (term_height, term_width) = {
                let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
                let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
                if unsafe { GetConsoleScreenBufferInfo(h, &mut csbi) } != 0 {
                    let w = (csbi.srWindow.Right - csbi.srWindow.Left + 1) as usize;
                    let h = (csbi.srWindow.Bottom - csbi.srWindow.Top + 1) as u16;
                    (h, w)
                } else {
                    (24u16, 80usize)
                }
            };
            let mut row = 1_u16;

            let _ = write!(out, "\x1B[H");
            let gpu_cores_str = if !sampler.gpu_cores.is_empty() { format!("    {}", sampler.gpu_cores) } else { String::new() };
            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("utop (Rust version)    {}{}", cpus, gpu_cores_str));

            let temp_str = if cpu_temp > -1000.0 { format!(" {:.1}°C", cpu_temp) } else { String::new() };
            let freq_str = if cpu_freq > 0.0 { format!(" @ {:.2} GHz", cpu_freq / 1000.0) } else { String::new() };

            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("{}: {:5.1}%{}{}", sampler.cpu_name, cpu, freq_str, temp_str));
            let mem_pct = if mem.total_bytes > 0 { mem.used_bytes as f64 * 100.0 / mem.total_bytes as f64 } else { 0.0 };
            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("MEM: {:5.1}% {} / {}", mem_pct, human_bytes(mem.used_bytes), human_bytes(mem.total_bytes)));

            if mem.swap_total_bytes > 0 {
                let swp_pct = mem.swap_used_bytes as f64 * 100.0 / mem.swap_total_bytes as f64;
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("SWP: {:5.1}% {} / {}", swp_pct, human_bytes(mem.swap_used_bytes), human_bytes(mem.swap_total_bytes)));
            } else {
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!(""));
            }

            if gpu.has_usage || gpu.has_mem {
                let g_temp = if gpu.has_temp { format!(" {:.1}°C", gpu.temp) } else { String::new() };
                let g_vram = if gpu.has_mem {
                    let pct = if gpu.mem_total > 0 { gpu.mem_used as f64 * 100.0 / gpu.mem_total as f64 } else { 0.0 };
                    format!("  VRAM: {:5.1}% {} / {}", pct, human_bytes(gpu.mem_used), human_bytes(gpu.mem_total))
                } else { String::new() };
                let g_usage = if gpu.has_usage { format!("{:5.1}%", gpu.usage) } else { String::new() };
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("{}: {}{}{}", gpu.name, g_usage, g_temp, g_vram));
            } else {
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("GPU:"));
            }

            if mem.cma_total_bytes > 0 && (!gpu.has_mem || mem.cma_total_bytes != gpu.mem_total) {
                let cma_pct = mem.cma_used_bytes as f64 * 100.0 / mem.cma_total_bytes as f64;
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("CMA: {:5.1}% {} / {}", cma_pct, human_bytes(mem.cma_used_bytes), human_bytes(mem.cma_total_bytes)));
            }

            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("NET: {}  rx {}/s  tx {}/s", net.iface, human_bytes(net.rx_rate as u64), human_bytes(net.tx_rate as u64)));
            
            for s in storage.iter().take(3) {
                let pct = if s.total_bytes > 0 { s.used_bytes as f64 * 100.0 / s.total_bytes as f64 } else { 0.0 };
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("DSK: {:<10} {:5.1}% {} / {} [{}]", s.mount_point, pct, human_bytes(s.used_bytes), human_bytes(s.total_bytes), s.device));
            }

            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("Controls: q:quit, j/k/arrows:move, h/l/arrows:sort, /:filter [{}]", if is_search { "SEARCHING" } else { "NORMAL" }));

            if is_search {
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("Filter: /{}_", filter));
            } else if !filter.is_empty() {
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("Filter: {} (press / to edit)", filter));
            } else {
                draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!(""));
            }
            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!(""));

            let pid_w = 7;
            let cpu_w = 8;
            let mem_w = 12;
            let thr_w = 4;

            let cpu_hdr = if sort == SortMode::Cpu { "CPU%▼" } else { "CPU%" };
            let mem_hdr = if sort == SortMode::Mem { "MEM▼" } else { "MEM" };

            let sort_extra = (if sort == SortMode::Cpu { 2isize } else { 0 }) + (if sort == SortMode::Mem { 2 } else { 0 });
            let name_w = (term_width as isize - (pid_w as isize + cpu_w as isize + mem_w as isize + thr_w as isize + 9 + sort_extra)).max(12) as usize;

            let w1 = cpu_w + if sort == SortMode::Cpu { 2 } else { 0 };
            let w2 = mem_w + if sort == SortMode::Mem { 2 } else { 0 };

            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("{:<pid_w$} {:<name_w$} {:>w1$} {:>w2$} {:>thr_w$}", "PID", "NAME", cpu_hdr, mem_hdr, "THR",
                pid_w=pid_w, name_w=name_w, w1=w1, w2=w2, thr_w=thr_w));

            let max_dashes = term_width;
            let req_dashes = pid_w + name_w + cpu_w + mem_w + thr_w + 4;
            let num_dashes = max_dashes.min(req_dashes);
            draw_next_line(&mut out, &mut row, term_height, term_width, false, format_args!("{}", "-".repeat(num_dashes)));

            let visible = term_height.saturating_sub(row) as usize;
            let count = sampler.procs.len();
            if selection >= count && count > 0 { selection = count - 1; }
            if count == 0 { selection = 0; }

            let mut scroll_top = selection.saturating_sub(visible / 2);
            if scroll_top > count.saturating_sub(visible) { scroll_top = count.saturating_sub(visible); }

            for i in scroll_top..count.min(scroll_top + visible) {
                let p = &sampler.procs[i];
                let mut p_name = p.name.clone();
                if p_name.chars().count() > name_w {
                    p_name = p_name.chars().take(name_w).collect();
                }

                draw_next_line(&mut out, &mut row, term_height, term_width, i == selection, format_args!("{:<pid_w$} {:<name_w$} {:>w1$.1} {:>mem_w$} {:>thr_w$}",
                    p.pid, p_name, p.cpu_percent, human_bytes(p.mem_bytes), p.threads,
                    pid_w=pid_w, name_w=name_w, w1=w1, mem_w=w2, thr_w=thr_w));
            }
            let clear_row = row.min(term_height);
            let _ = write!(out, "\x1B[{};1H\x1B[J", clear_row);
            if count > 0 {
                let end_idx = count.min(scroll_top + visible);
                let _ = draw_line(&mut out, term_height, term_width, false, format_args!("Showing {}-{} of {}", scroll_top + 1, end_idx, count));
            }
            let _ = out.flush();
            last_render = now;
            needs_render = false;
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let has_input = {
            let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            (unsafe { libc::poll(&mut pfd, 1, 10) }) > 0
        };
        #[cfg(target_os = "windows")]
        let has_input = unsafe { WaitForSingleObject(GetStdHandle(STD_INPUT_HANDLE), 10) == 0 };

        if has_input {
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_sample_populates_core_data() {
        let mut sampler = Sampler::new();
        let mut cpu = 0.0;
        let mut mem = MemorySnapshot::default();
        let mut net = NetworkSnapshot::default();
        let mut gpu = GpuSnapshot::default();
        let mut storage = Vec::new();
        let mut cpus = String::new();
        let mut cached_gpu = GpuSnapshot::default();
        let mut last_gpu_read = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();

        sample(
            &mut sampler,
            SortMode::Cpu,
            "",
            &mut cpu,
            &mut mem,
            &mut net,
            &mut gpu,
            &mut storage,
            &mut cpus,
            &mut cached_gpu,
            &mut last_gpu_read,
        );

        assert!(!cpus.is_empty());
        assert!(cpu >= 0.0);
        assert!(mem.total_bytes > 0);
        assert!(!sampler.procs.is_empty());

        #[cfg(target_os = "macos")]
        assert!(!storage.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_read_memory_windows() {
        let mem = read_memory();
        assert!(mem.total_bytes > 0, "total memory should be > 0");
        assert!(mem.total_bytes >= mem.used_bytes, "total >= used");
        assert!(mem.total_bytes >= 128 * 1024 * 1024, "total memory >= 128 MiB");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_read_swap_windows() {
        let mem = read_memory();
        assert!(mem.swap_total_bytes > 0, "swap total should be > 0, got {}", mem.swap_total_bytes);
        assert!(mem.swap_used_bytes <= mem.swap_total_bytes, "swap used ({}) <= total ({})", mem.swap_used_bytes, mem.swap_total_bytes);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_read_cpu_count_windows() {
        let cpus = read_cpu_count();
        assert!(!cpus.is_empty(), "CPU count string should not be empty");
        let cores: u32 = cpus.split_whitespace().nth(2)
            .and_then(|w| w.trim_end_matches(',').parse().ok())
            .unwrap_or(0);
        assert!(cores > 0, "CPU core count should be > 0, got '{cpus}'");
        assert!(cores <= 256, "CPU core count should be reasonable, got {cores}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_read_network_windows() {
        let mut prev = HashMap::new();
        let elapsed = 1.0;
        let net = read_network(&mut prev, elapsed);
        assert!(!net.iface.is_empty(), "network interface should not be empty");
        assert!(net.rx_rate >= 0.0, "rx rate >= 0");
        assert!(net.tx_rate >= 0.0, "tx rate >= 0");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_read_storage_windows() {
        let storage = read_storage();
        assert!(!storage.is_empty(), "should have at least one storage device");
        for s in &storage {
            assert!(!s.device.is_empty(), "device should not be empty");
            assert!(s.total_bytes > 0, "total bytes > 0 for {}", s.mount_point);
            assert!(s.used_bytes <= s.total_bytes, "used <= total for {}", s.mount_point);
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_sample_windows() {
        let mut sampler = Sampler::new();
        let mut cpu = 0.0;
        let mut mem = MemorySnapshot::default();
        let mut net = NetworkSnapshot::default();
        let mut gpu = GpuSnapshot::default();
        let mut storage = Vec::new();
        let mut cpus = String::new();
        let mut cached_gpu = GpuSnapshot::default();
        let mut last_gpu_read = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();

        sample(
            &mut sampler,
            SortMode::Cpu,
            "",
            &mut cpu,
            &mut mem,
            &mut net,
            &mut gpu,
            &mut storage,
            &mut cpus,
            &mut cached_gpu,
            &mut last_gpu_read,
        );

        assert!(cpu >= 0.0);
        assert!(mem.total_bytes > 0);
        assert!(!sampler.procs.is_empty(), "should have at least one process");
        assert!(!cpus.is_empty(), "CPU count should be populated");
        assert!(!storage.is_empty(), "should have at least one storage device");
    }
}
