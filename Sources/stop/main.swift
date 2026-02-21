import Foundation
import Glibc

private var gTermiosOriginal = termios()
private var gTermiosFlags: Int32 = -1
private var gTermiosActive: sig_atomic_t = 0
private var gAltScreenActive: sig_atomic_t = 0

@inline(__always)
func writeEsc(_ sequence: String) {
    _ = sequence.withCString { ptr in
        write(STDOUT_FILENO, ptr, strlen(ptr))
    }
}

@_cdecl("rtop_restore_terminal")
func rtop_restore_terminal() {
    if gTermiosActive == 0 {
        return
    }
    var restore = gTermiosOriginal
    _ = tcsetattr(STDIN_FILENO, TCSAFLUSH, &restore)
    if gTermiosFlags != -1 {
        _ = fcntl(STDIN_FILENO, F_SETFL, gTermiosFlags)
    }
    if gAltScreenActive == 1 {
        writeEsc("\u{001B}[?1049l")
        gAltScreenActive = 0
    }
    writeEsc("\u{001B}[?25h\u{001B}[0m")
    fflush(stdout)
    gTermiosActive = 0
}

@_cdecl("rtop_signal_handler")
func rtop_signal_handler(_ sig: Int32) {
    rtop_restore_terminal()
    _ = signal(sig, SIG_DFL)
    _ = kill(getpid(), sig)
}

struct CpuTimes {
    let user: UInt64
    let nice: UInt64
    let system: UInt64
    let idle: UInt64
    let iowait: UInt64
    let irq: UInt64
    let softirq: UInt64
    let steal: UInt64

    var total: UInt64 { user + nice + system + idle + iowait + irq + softirq + steal }
    var idleTotal: UInt64 { idle + iowait }
}

struct MemorySnapshot {
    let usedBytes: UInt64
    let totalBytes: UInt64
    let ramSpeed: String?

    var usedPercent: Double {
        guard totalBytes > 0 else { return 0.0 }
        return min(100.0, (Double(usedBytes) / Double(totalBytes)) * 100.0)
    }
}

struct NetworkSnapshot {
    let iface: String
    let rxRate: Double
    let txRate: Double
}

struct GpuSnapshot {
    let name: String
    let usage: Double?      // 0-100%
    let memUsed: UInt64?
    let memTotal: UInt64?
    let temp: Double?       // Celsius
    
    var memPercent: Double? {
        guard let used = memUsed, let total = memTotal, total > 0 else { return nil }
        return (Double(used) / Double(total)) * 100.0
    }
}

enum SortMode {
    case cpu
    case memory
}

struct ProcessInfo {
    let pid: Int
    let name: String
    let cpuPercent: Double
    let memBytes: UInt64
    let threads: Int
}

enum Key {
    case quit
    case up
    case down
    case left
    case right
    case search
    case backspace
    case enter
    case esc
    case char(Character)
}

final class TerminalRawMode {
    private var original = termios()
    private var originalFlags: Int32 = 0
    private var active = false

    init?() {
        if tcgetattr(STDIN_FILENO, &original) != 0 { return nil }
        originalFlags = fcntl(STDIN_FILENO, F_GETFL, 0)
        if originalFlags == -1 { return nil }

        var raw = original
        // Keep control in-process so normal teardown always restores cursor/tty.
        raw.c_lflag &= ~tcflag_t(ECHO | ICANON | ISIG)
        if tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) != 0 { return nil }
        if fcntl(STDIN_FILENO, F_SETFL, originalFlags | O_NONBLOCK) == -1 {
            var restore = original
            _ = tcsetattr(STDIN_FILENO, TCSAFLUSH, &restore)
            return nil
        }

        gTermiosOriginal = original
        gTermiosFlags = originalFlags
        gTermiosActive = 1
        gAltScreenActive = 1

        active = true
        // Use alternate screen to prevent shell/build output from overlapping UI.
        writeEsc("\u{001B}[?1049h\u{001B}[2J\u{001B}[H\u{001B}[?25l")
        fflush(stdout)
    }

    func restoreNow() {
        guard active else { return }
        active = false
        var restore = original
        _ = tcsetattr(STDIN_FILENO, TCSAFLUSH, &restore)
        _ = fcntl(STDIN_FILENO, F_SETFL, originalFlags)
        gTermiosActive = 0
        if gAltScreenActive == 1 {
            writeEsc("\u{001B}[?1049l")
            gAltScreenActive = 0
        }
        writeEsc("\u{001B}[?25h\u{001B}[0m") // show cursor + reset
        fflush(stdout)
    }

    deinit { restoreNow() }
}

final class Sampler {
    private var previousCpu: CpuTimes?
    private var previousTotalPerPid: [Int: UInt64] = [:]
    private var previousRxByIface: [String: UInt64] = [:]
    private var previousTxByIface: [String: UInt64] = [:]
    private var lastSampleAt: Date = Date()
    private let pageSize: UInt64 = UInt64(max(1, sysconf(Int32(_SC_PAGESIZE))))
    private var cpuCount: Int = max(1, Foundation.ProcessInfo.processInfo.activeProcessorCount)

    private var lastNvidiaSampleAt: Date = Date.distantPast
    private var lastNvidiaGpu: GpuSnapshot?
    private var lastV3dStats: [String: (UInt64, UInt64)] = [:]
    private var cachedRamSpeed: String?
    private var checkedRamSpeed = false

    func sample(sortMode: SortMode, filter: String) -> (Double, Double?, MemorySnapshot, NetworkSnapshot, GpuSnapshot?, [ProcessInfo], Int) {
        let now = Date()
        let elapsed = max(0.001, now.timeIntervalSince(lastSampleAt))
        lastSampleAt = now

        let currentCpu = readCpuTimes()
        let (cpuPercent, delta) = computeCpuDelta(currentCpu)
        let cpuTemp = readCpuTemp()
        let memory = readMemory()
        let network = readNetwork(elapsed: elapsed)
        let gpu = readGpu()
        let processes = readProcesses(cpuDeltaTotal: delta, sortMode: sortMode, filter: filter)

        return (cpuPercent, cpuTemp, memory, network, gpu, processes, cpuCount)
    }

    private func readCpuTemp() -> Double? {
        // Try thermal_zone (usually more standard)
        if let dir = opendir("/sys/class/thermal") {
            defer { closedir(dir) }
            while let entry = readdir(dir) {
                let name = withUnsafePointer(to: entry.pointee.d_name) { ptr in
                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                }
                if name.hasPrefix("thermal_zone") {
                    let typePath = "/sys/class/thermal/\(name)/type"
                    if let type = (try? String(contentsOfFile: typePath, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                       type.lowercased().contains("pkg") || type.lowercased().contains("cpu") || type.lowercased().contains("core") {
                        let tempPath = "/sys/class/thermal/\(name)/temp"
                        if let tempStr = (try? String(contentsOfFile: tempPath, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                           let t = Double(tempStr) {
                            return t / 1000.0
                        }
                    }
                }
            }
        }
        
        // Fallback to hwmon
        if let dir = opendir("/sys/class/hwmon") {
            defer { closedir(dir) }
            while let entry = readdir(dir) {
                let name = withUnsafePointer(to: entry.pointee.d_name) { ptr in
                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                }
                if name.hasPrefix("hwmon") {
                    let path = "/sys/class/hwmon/\(name)"
                    let namePath = "\(path)/name"
                    if let hwName = (try? String(contentsOfFile: namePath, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                       hwName.lowercased().contains("coretemp") || hwName.lowercased().contains("cpu") || hwName.lowercased().contains("k10temp") {
                        // Look for temp1_input or temp*_input
                        var bestTemp: Double?
                        if let hDir = opendir(path) {
                            defer { closedir(hDir) }
                            while let hEntry = readdir(hDir) {
                                let hName = withUnsafePointer(to: hEntry.pointee.d_name) { ptr in
                                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                                }
                                if hName.hasPrefix("temp") && hName.hasSuffix("_input") {
                                    if let tStr = (try? String(contentsOfFile: "\(path)/\(hName)", encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                                       let t = Double(tStr) {
                                        bestTemp = max(bestTemp ?? 0, t / 1000.0)
                                    }
                                }
                            }
                        }
                        if let bt = bestTemp { return bt }
                    }
                }
            }
        }
        return nil
    }

    private func readGpu() -> GpuSnapshot? {
        // 1. NVIDIA via nvidia-smi (High priority for NVIDIA)
        // Check if we have an NVIDIA card in DRM first or just try nvidia-smi
        let now = Date()
        if now.timeIntervalSince(lastNvidiaSampleAt) >= 0.8 { // slightly faster than 1s to ensure it updates
            lastNvidiaSampleAt = now
            let pipe = Pipe()
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/nvidia-smi")
            process.arguments = ["--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"]
            process.standardOutput = pipe
            
            do {
                try process.run()
                process.waitUntilExit()
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                if let output = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) {
                    let parts = output.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                    if parts.count >= 4 {
                        let usage = Double(parts[0])
                        let used = UInt64(parts[1]).map { $0 * 1024 * 1024 }
                        let total = UInt64(parts[2]).map { $0 * 1024 * 1024 }
                        let temp = Double(parts[3])
                        lastNvidiaGpu = GpuSnapshot(name: "NVIDIA GPU", usage: usage, memUsed: used, memTotal: total, temp: temp)
                        return lastNvidiaGpu
                    }
                }
            } catch {
                // nvidia-smi likely not found or failed, fall through to sysfs
            }
        } else if lastNvidiaGpu != nil {
            return lastNvidiaGpu
        }

        // 2. Check for AMD / DRM / generic sysfs percentage
        if let dir = opendir("/sys/class/drm") {
            defer { closedir(dir) }
            while let entry = readdir(dir) {
                let name = withUnsafePointer(to: entry.pointee.d_name) { ptr in
                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                }
                if name.hasPrefix("card") && !name.hasSuffix("-") {
                    let cardPath = "/sys/class/drm/\(name)"
                    let devicePath = "\(cardPath)/device"
                    
                    // Try various usage paths
                    var usage: Double?
                    let usagePaths = [
                        "\(devicePath)/gpu_busy_percent", // AMD, RPi
                        "\(cardPath)/gt/gt0/usage",      // Intel (newer)
                        "\(devicePath)/usage",           // Generic
                        "\(devicePath)/load"             // Broadcom/v3d
                    ]
                    
                    for p in usagePaths {
                        if let valStr = try? String(contentsOfFile: p, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines) {
                            // Handle "usage@freq" format found in devfreq and some ARM drivers
                            let firstPart = valStr.split(separator: "@").first.map(String.init) ?? valStr
                            if let u = Double(firstPart) {
                                usage = u
                                break
                            }
                        }
                    }
                    
                    if usage == nil {
                        // Broadcom v3d gpu_stats
                        if let stats = try? String(contentsOfFile: "\(devicePath)/gpu_stats", encoding: .utf8) {
                            let lines = stats.split(separator: "\n").dropFirst()
                            for line in lines {
                                let parts = line.split(whereSeparator: { $0.isWhitespace })
                                if parts.count >= 4, let ts = UInt64(parts[1]), let rt = UInt64(parts[3]) {
                                    let queue = String(parts[0])
                                    if let last = lastV3dStats[queue], ts > last.0 {
                                        let u = (Double(rt - last.1) / Double(ts - last.0)) * 100.0
                                        usage = max(usage ?? 0, u)
                                    }
                                    lastV3dStats[queue] = (ts, rt)
                                }
                            }
                        }
                    }
                    
                    if usage != nil {
                        let vendorPath = "\(devicePath)/vendor"
                        let vendor = (try? String(contentsOfFile: vendorPath, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                        
                        var nameStr = "GPU"
                        if vendor == "0x1002" { nameStr = "AMD GPU" }
                        else if vendor == "0x8086" { nameStr = "Intel GPU" }
                        else if vendor == "0x10de" { nameStr = "NVIDIA GPU" }
                        else if vendor == "0x14e4" { nameStr = "Broadcom GPU" }
                        else {
                            // Check for Broadcom/VideoCore via uevent/driver
                            if let uevent = try? String(contentsOfFile: "\(devicePath)/uevent", encoding: .utf8) {
                                if uevent.contains("DRIVER=v3d") || uevent.contains("DRIVER=vc4") {
                                    nameStr = "VideoCore GPU"
                                }
                            }
                        }
                        
                        // Try to get VRAM
                        var usedVram: UInt64?
                        var totalVram: UInt64?
                        var gpuTemp: Double?
                        
                        // Try to find temp in hwmon inside device or sibling
                        let hwmonDir = "\(devicePath)/hwmon"
                        if let hDir = opendir(hwmonDir) {
                            defer { closedir(hDir) }
                            while let hEntry = readdir(hDir) {
                                let hName = withUnsafePointer(to: hEntry.pointee.d_name) { ptr in
                                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                                }
                                if hName.hasPrefix("hwmon") {
                                    if let tStr = (try? String(contentsOfFile: "\(hwmonDir)/\(hName)/temp1_input", encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                                       let t = Double(tStr) {
                                        gpuTemp = t / 1000.0
                                    }
                                }
                            }
                        }
                        
                        if gpuTemp == nil {
                            // Generic fallback for SoC (RPi, etc.)
                            if let tStr = (try? String(contentsOfFile: "/sys/class/thermal/thermal_zone0/temp", encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                               let t = Double(tStr) {
                                gpuTemp = t / 1000.0
                            }
                        }
                        
                        // AMD specific
                        if let uStr = try? String(contentsOfFile: "\(devicePath)/mem_info_vram_used", encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),
                           let u = UInt64(uStr) {
                            usedVram = u
                        }
                        if let tStr = try? String(contentsOfFile: "\(devicePath)/mem_info_vram_total", encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),
                           let t = UInt64(tStr) {
                            totalVram = t
                        }
                        
                        // Intel specific (xe driver / newer)
                        if usedVram == nil {
                            if let uStr = try? String(contentsOfFile: "\(cardPath)/tile0/vram0/used", encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),
                               let u = UInt64(uStr) {
                                usedVram = u
                            }
                            if let tStr = try? String(contentsOfFile: "\(cardPath)/tile0/vram0/size", encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),
                               let t = UInt64(tStr) {
                                totalVram = t
                            }
                        }

                        // VideoCore / Broadcom CMA fallback
                        if usedVram == nil && nameStr == "VideoCore GPU" {
                            if let meminfo = try? String(contentsOfFile: "/proc/meminfo", encoding: .utf8) {
                                var cmaTotal: UInt64 = 0
                                var cmaFree: UInt64 = 0
                                for line in meminfo.split(separator: "\n") {
                                    if line.hasPrefix("CmaTotal:") {
                                        cmaTotal = parseMeminfoKB(line) * 1024
                                    } else if line.hasPrefix("CmaFree:") {
                                        cmaFree = parseMeminfoKB(line) * 1024
                                    }
                                }
                                if cmaTotal > 0 {
                                    usedVram = cmaTotal > cmaFree ? cmaTotal - cmaFree : 0
                                    totalVram = cmaTotal
                                }
                            }
                        }
                        
                        return GpuSnapshot(name: nameStr, usage: usage, memUsed: usedVram, memTotal: totalVram, temp: gpuTemp)
                    }
                }
            }
        }
        
        // 3. Adreno / kgsl (Android/Linux handhelds)
        let adrenoPaths = [
            "/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage",
            "/sys/class/kgsl/kgsl-3d0/gpubusy"
        ]
        for p in adrenoPaths {
            if let valStr = try? String(contentsOfFile: p, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines) {
                let usage: Double?
                if p.contains("gpubusy") {
                    // gpubusy often returns "busy_ticks total_ticks"
                    let parts = valStr.split(separator: " ").compactMap { Double($0) }
                    if parts.count >= 2 && parts[1] > 0 {
                        usage = (parts[0] / parts[1]) * 100.0
                    } else { usage = nil }
                } else {
                    usage = Double(valStr)
                }
                
                if let u = usage {
                    // Try to get temp for Adreno
                    var aTemp: Double?
                    if let tStr = (try? String(contentsOfFile: "/sys/class/thermal/thermal_zone0/temp", encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                       let t = Double(tStr) {
                        aTemp = t / 1000.0
                    }
                    return GpuSnapshot(name: "Adreno GPU", usage: u, memUsed: nil, memTotal: nil, temp: aTemp)
                }
            }
        }
        
        // 4. Generic devfreq fallback (RPi, Mali, etc.)
        if let dir = opendir("/sys/class/devfreq") {
            defer { closedir(dir) }
            while let entry = readdir(dir) {
                let dfName = withUnsafePointer(to: entry.pointee.d_name) { ptr in
                    String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
                }
                if dfName.contains("v3d") || dfName.contains("gpu") || dfName.contains("mali") {
                    let loadPath = "/sys/class/devfreq/\(dfName)/load"
                    if let loadStr = try? String(contentsOfFile: loadPath, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines) {
                        let usagePart = loadStr.split(separator: "@").first.map(String.init) ?? loadStr
                        if let u = Double(usagePart) {
                            var gpuTemp: Double?
                            if let tStr = (try? String(contentsOfFile: "/sys/class/thermal/thermal_zone0/temp", encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines),
                               let t = Double(tStr) {
                                gpuTemp = t / 1000.0
                            }
                            
                            let name: String
                            if dfName.contains("v3d") { name = "VideoCore GPU" }
                            else if dfName.contains("mali") { name = "Mali GPU" }
                            else { name = "GPU" }
                            
                            return GpuSnapshot(name: name, usage: u, memUsed: nil, memTotal: nil, temp: gpuTemp)
                        }
                    }
                }
            }
        }
        
        return lastNvidiaGpu
    }

    private func computeCpuDelta(_ current: CpuTimes?) -> (Double, UInt64) {
        guard let current else { return (0.0, 0) }
        defer { previousCpu = current }
        guard let prev = previousCpu else { return (0.0, 0) }

        let totalDelta = current.total > prev.total ? current.total - prev.total : 0
        let idleDelta = current.idleTotal > prev.idleTotal ? current.idleTotal - prev.idleTotal : 0
        guard totalDelta > 0 else { return (0.0, 0) }
        let used = totalDelta > idleDelta ? totalDelta - idleDelta : 0
        let percent = min(100.0, (Double(used) / Double(totalDelta)) * 100.0)
        return (percent, totalDelta)
    }

    private func readCpuTimes() -> CpuTimes? {
        guard let stat = try? String(contentsOfFile: "/proc/stat", encoding: .utf8) else { return nil }
        guard let line = stat.split(separator: "\n").first(where: { $0.hasPrefix("cpu ") }) else { return nil }
        let fields = line.split(whereSeparator: { $0 == " " || $0 == "\t" })
        guard fields.count >= 9 else { return nil }

        let nums = fields.dropFirst().prefix(8).compactMap { UInt64($0) }
        guard nums.count == 8 else { return nil }

        cpuCount = stat.split(separator: "\n").filter { $0.hasPrefix("cpu") && !$0.hasPrefix("cpu ") }.count
        cpuCount = max(1, cpuCount)

        return CpuTimes(
            user: nums[0],
            nice: nums[1],
            system: nums[2],
            idle: nums[3],
            iowait: nums[4],
            irq: nums[5],
            softirq: nums[6],
            steal: nums[7]
        )
    }

    private func readMemory() -> MemorySnapshot {
        if !checkedRamSpeed {
            checkedRamSpeed = true
            // 1. Try udevadm info (fast, no root, modern kernels/udev)
            let pipe = Pipe()
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/udevadm")
            process.arguments = ["info", "--query=property", "--path=/sys/devices/virtual/dmi/id/"]
            process.standardOutput = pipe
            do {
                try process.run()
                process.waitUntilExit()
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                if let output = String(data: data, encoding: .utf8) {
                    let lines = output.split(separator: "\n")
                    var maxSpeed = 0
                    for line in lines {
                        if line.contains("SPEED_MTS=") {
                            let parts = line.split(separator: "=")
                            if parts.count >= 2, let s = Int(parts[1]) {
                                maxSpeed = max(maxSpeed, s)
                            }
                        }
                    }
                    if maxSpeed > 0 {
                        cachedRamSpeed = "\(maxSpeed) MT/s"
                    }
                }
            } catch {}

            // 2. Fallback to dmidecode if udevadm failed
            if cachedRamSpeed == nil {
                let pipe = Pipe()
                let process = Process()
                process.executableURL = URL(fileURLWithPath: "/usr/bin/dmidecode")
                process.arguments = ["-t", "memory"]
                process.standardOutput = pipe
                do {
                    try process.run()
                    process.waitUntilExit()
                    let data = pipe.fileHandleForReading.readDataToEndOfFile()
                    if let output = String(data: data, encoding: .utf8) {
                        let lines = output.split(separator: "\n")
                        for line in lines {
                            if line.contains("Speed:") && !line.contains("Unknown") && !line.contains("Configured") {
                                let parts = line.split(separator: ":")
                                if parts.count >= 2 {
                                    cachedRamSpeed = parts[1].trimmingCharacters(in: .whitespacesAndNewlines)
                                    break
                                }
                            }
                        }
                    }
                } catch {}
            }
        }

        guard let text = try? String(contentsOfFile: "/proc/meminfo", encoding: .utf8) else {
            return MemorySnapshot(usedBytes: 0, totalBytes: 0, ramSpeed: cachedRamSpeed)
        }
        
        var totalKB: UInt64 = 0
        var availableKB: UInt64 = 0

        for line in text.split(separator: "\n") {
            if line.hasPrefix("MemTotal:") {
                totalKB = parseMeminfoKB(line)
            } else if line.hasPrefix("MemAvailable:") {
                availableKB = parseMeminfoKB(line)
            }
        }

        let total = totalKB * 1024
        let used = total > (availableKB * 1024) ? total - (availableKB * 1024) : 0
        return MemorySnapshot(usedBytes: used, totalBytes: total, ramSpeed: cachedRamSpeed)
    }

    private func parseMeminfoKB(_ line: Substring) -> UInt64 {
        let parts = line.split(whereSeparator: { $0 == " " || $0 == "\t" || $0 == ":" })
        for token in parts {
            if let v = UInt64(token) { return v }
        }
        return 0
    }

    private func readNetwork(elapsed: Double) -> NetworkSnapshot {
        guard let text = try? String(contentsOfFile: "/proc/net/dev", encoding: .utf8) else {
            return NetworkSnapshot(iface: "-", rxRate: 0, txRate: 0)
        }

        var best = NetworkSnapshot(iface: "-", rxRate: 0, txRate: 0)
        var bestTotal: UInt64 = 0

        for rawLine in text.split(separator: "\n").dropFirst(2) {
            let line = rawLine.replacingOccurrences(of: ":", with: " ")
            let fields = line.split(whereSeparator: { $0 == " " || $0 == "\t" })
            guard fields.count >= 17 else { continue }

            let iface = String(fields[0])
            let rxTotal = UInt64(fields[1]) ?? 0
            let txTotal = UInt64(fields[9]) ?? 0
            if iface == "lo" { continue }

            let prevRx = previousRxByIface[iface] ?? rxTotal
            let prevTx = previousTxByIface[iface] ?? txTotal
            previousRxByIface[iface] = rxTotal
            previousTxByIface[iface] = txTotal

            let rxRate = Double(rxTotal >= prevRx ? rxTotal - prevRx : 0) / elapsed
            let txRate = Double(txTotal >= prevTx ? txTotal - prevTx : 0) / elapsed

            let sum = rxTotal + txTotal
            if sum > bestTotal {
                bestTotal = sum
                best = NetworkSnapshot(iface: iface, rxRate: rxRate, txRate: txRate)
            }
        }

        return best
    }

    private func readProcesses(cpuDeltaTotal: UInt64, sortMode: SortMode, filter: String) -> [ProcessInfo] {
        guard let dir = opendir("/proc") else { return [] }
        defer { closedir(dir) }

        var currentTotalPerPid: [Int: UInt64] = [:]
        var rows: [ProcessInfo] = []
        rows.reserveCapacity(512)

        let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: 1024)
        defer { buffer.deallocate() }

        while let entry = readdir(dir) {
            let name = withUnsafePointer(to: entry.pointee.d_name) { ptr in
                String(cString: UnsafeRawPointer(ptr).assumingMemoryBound(to: CChar.self))
            }
            guard let pid = Int(name) else { continue }

            let path = "/proc/\(name)/stat"
            let fd = open(path, O_RDONLY)
            if fd < 0 { continue }
            let bytesRead = read(fd, buffer, 1024)
            close(fd)
            if bytesRead <= 0 { continue }

            let statStr = String(decoding: UnsafeBufferPointer(start: buffer, count: bytesRead), as: UTF8.self)
            guard let parsed = parseProcStat(statStr) else { continue }

            let totalTicks = parsed.utime + parsed.stime
            currentTotalPerPid[pid] = totalTicks

            if !filter.isEmpty {
                if !parsed.name.lowercased().contains(filter.lowercased()) && !String(pid).contains(filter) {
                    continue
                }
            }

            let prevTicks = previousTotalPerPid[pid] ?? totalTicks
            let deltaTicks = totalTicks >= prevTicks ? totalTicks - prevTicks : UInt64(0)

            let cpuPercent: Double
            if cpuDeltaTotal == 0 {
                cpuPercent = 0.0
            } else {
                let ratio = Double(deltaTicks) / Double(cpuDeltaTotal)
                cpuPercent = min(100.0, ratio * 100.0)
            }

            let rssBytes = UInt64(parsed.rssPages) * pageSize
            rows.append(
                ProcessInfo(
                    pid: pid,
                    name: parsed.name,
                    cpuPercent: cpuPercent,
                    memBytes: rssBytes,
                    threads: parsed.numThreads
                )
            )
        }

        previousTotalPerPid = currentTotalPerPid

        rows.sort {
            switch sortMode {
            case .cpu:
                if $0.cpuPercent == $1.cpuPercent { return $0.memBytes > $1.memBytes }
                return $0.cpuPercent > $1.cpuPercent
            case .memory:
                if $0.memBytes == $1.memBytes { return $0.cpuPercent > $1.cpuPercent }
                return $0.memBytes > $1.memBytes
            }
        }

        return rows
    }

    private func parseProcStat(_ raw: String) -> (name: String, utime: UInt64, stime: UInt64, numThreads: Int, rssPages: Int64)? {
        guard let open = raw.firstIndex(of: "("), let close = raw.lastIndex(of: ")"), open < close else { return nil }
        let name = String(raw[raw.index(after: open)..<close])
        let after = raw[raw.index(after: close)...]
        let fields = after.split(whereSeparator: { $0 == " " || $0 == "\t" })
        guard fields.count > 21 else { return nil }

        let utime = UInt64(fields[11]) ?? 0
        let stime = UInt64(fields[12]) ?? 0
        let threads = Int(fields[17]) ?? 0
        let rss = Int64(fields[21]) ?? 0

        return (name: name, utime: utime, stime: stime, numThreads: max(1, threads), rssPages: max(0, rss))
    }
}

func humanBytes(_ bytes: UInt64) -> String {
    let kb = 1024.0
    let mb = kb * 1024.0
    let gb = mb * 1024.0
    let value = Double(bytes)
    if value >= gb { return String(format: "%.2f GiB", value / gb) }
    if value >= mb { return String(format: "%.1f MiB", value / mb) }
    if value >= kb { return String(format: "%.1f KiB", value / kb) }
    return "\(bytes) B"
}

func readKey() -> Key? {
    var buf = [UInt8](repeating: 0, count: 8)
    let n = read(STDIN_FILENO, &buf, buf.count)
    if n <= 0 { return nil }
    let bytes = Array(buf.prefix(Int(n)))
    if bytes.contains(3) { return .quit } // Ctrl-C
    
    if bytes.count == 1 {
        let b = bytes[0]
        if b == 27 { return .esc }
        if b == UInt8(ascii: "q") { return .quit }
        if b == UInt8(ascii: "j") { return .down }
        if b == UInt8(ascii: "k") { return .up }
        if b == UInt8(ascii: "h") { return .left }
        if b == UInt8(ascii: "l") { return .right }
        if b == UInt8(ascii: "/") { return .search }
        if b == 127 || b == 8 { return .backspace }
        if b == 10 || b == 13 { return .enter }
        if b >= 32 && b <= 126 { return .char(Character(UnicodeScalar(b))) }
    }
    
    if bytes.count >= 3, bytes[0] == 27, bytes[1] == 91 {
        if bytes[2] == 65 { return .up }
        if bytes[2] == 66 { return .down }
        if bytes[2] == 67 { return .right }
        if bytes[2] == 68 { return .left }
    }
    return nil
}

func termSize() -> (rows: Int, cols: Int) {
    var ws = winsize()
    if ioctl(STDOUT_FILENO, UInt(TIOCGWINSZ), &ws) == 0 {
        return (rows: max(10, Int(ws.ws_row)), cols: max(40, Int(ws.ws_col)))
    }
    return (rows: 30, cols: 100)
}

func clamp(_ value: Int, _ minValue: Int, _ maxValue: Int) -> Int {
    if value < minValue { return minValue }
    if value > maxValue { return maxValue }
    return value
}

func padRight(_ s: String, _ width: Int) -> String {
    if width <= 0 { return "" }
    if s.count >= width { return String(s.prefix(width)) }
    return s + String(repeating: " ", count: width - s.count)
}

func padLeft(_ s: String, _ width: Int) -> String {
    if width <= 0 { return "" }
    if s.count >= width { return String(s.suffix(width)) }
    return String(repeating: " ", count: width - s.count) + s
}

func render(
    cpu: Double,
    cpuTemp: Double?,
    memory: MemorySnapshot,
    network: NetworkSnapshot,
    gpu: GpuSnapshot?,
    processes: [ProcessInfo],
    selected: Int,
    cpuCount: Int,
    sortMode: SortMode,
    filter: String,
    isSearching: Bool
) {
    let size = termSize()
    let headerLines = 9
    let tableStart = headerLines + 2
    let visibleRows = max(5, size.rows - tableStart - 2)

    let safeSelected = clamp(selected, 0, max(0, processes.count - 1))
    let scrollTop = max(0, min(safeSelected - (visibleRows / 2), max(0, processes.count - visibleRows)))
    let end = min(processes.count, scrollTop + visibleRows)

    @inline(__always)
    func clipLine(_ s: String) -> String {
        if s.count <= size.cols { return s }
        return String(s.prefix(size.cols))
    }

    @inline(__always)
    func appendLine(_ out: inout String, _ line: String) {
        out += "\u{001B}[0m\u{001B}[2K"
        out += clipLine(line)
        out += "\n"
    }

    @inline(__always)
    func appendSelectedLine(_ out: inout String, _ line: String) {
        out += "\u{001B}[0m\u{001B}[2K\u{001B}[7m"
        out += clipLine(line)
        out += "\u{001B}[0m\n"
    }

    // Repaint in-place and clear each row to avoid stale text artifacts.
    var out = "\u{001B}[H"
    appendLine(&out, "stop (pure Swift, no libs)    CPUs: \(cpuCount)")
    
    let cpuTempStr = cpuTemp.map { String(format: " %4.1f°C", $0) } ?? ""
    appendLine(&out, "CPU: \(String(format: "%5.1f", cpu))%\(cpuTempStr)")
    
    let speedStr = memory.ramSpeed.map { " @ \($0)" } ?? ""
    appendLine(&out, "MEM: \(String(format: "%5.1f", memory.usedPercent))%  \(humanBytes(memory.usedBytes)) / \(humanBytes(memory.totalBytes))\(speedStr)")
    
    if let g = gpu {
        let usageStr = g.usage.map { String(format: "%5.1f%%", $0) } ?? " - %"
        let gTempStr = g.temp.map { String(format: " %4.1f°C", $0) } ?? ""
        var vramStr = ""
        if let used = g.memUsed, let total = g.memTotal {
            vramStr = "  VRAM: \(humanBytes(used)) / \(humanBytes(total))"
        }
        appendLine(&out, "\(g.name): \(usageStr)\(gTempStr)\(vramStr)")
    } else {
        appendLine(&out, "GPU: - %")
    }

    appendLine(&out, "NET: \(network.iface)  rx \(humanBytes(UInt64(network.rxRate)))/s  tx \(humanBytes(UInt64(network.txRate)))/s")
    appendLine(&out, "Controls: q quit, j/k/arrows move, h/l/arrows sort, / search")
    if isSearching {
        appendLine(&out, "\u{001B}[1;32mSearch: /\(filter)\u{001B}[0m\u{001B}[5m_\u{001B}[0m")
    } else if !filter.isEmpty {
        appendLine(&out, "Filter: \(filter) (press / to edit)")
    } else {
        appendLine(&out, "")
    }
    appendLine(&out, "")

    let pidCol = 7
    let cpuCol = 8
    let memCol = 12
    let thrCol = 4
    let fixed = pidCol + cpuCol + memCol + thrCol + 10
    let nameCol = max(12, size.cols - fixed)
    
    let h1 = padRight("PID", pidCol)
    let h2 = padRight("NAME", nameCol)
    let h3 = padLeft(sortMode == .cpu ? "CPU%▼" : "CPU%", cpuCol)
    let h4 = padLeft(sortMode == .memory ? "MEM▼" : "MEM", memCol)
    let h5 = padLeft("THR", thrCol)
    
    appendLine(&out, "\(h1) \(h2) \(h3) \(h4) \(h5)")
    appendLine(&out, String(repeating: "-", count: min(size.cols, max(40, h1.count + h2.count + h3.count + h4.count + h5.count + 4))))

    for idx in scrollTop..<end {
        let p = processes[idx]
        let name = p.name.count > nameCol ? String(p.name.prefix(max(0, nameCol - 2))) + ".." : p.name
        let row = [
            padRight(String(p.pid), pidCol),
            padRight(name, nameCol),
            padLeft(String(format: "%.1f", p.cpuPercent), cpuCol),
            padLeft(humanBytes(p.memBytes), memCol),
            padLeft(String(p.threads), thrCol),
        ].joined(separator: " ")
        if idx == safeSelected {
            appendSelectedLine(&out, row)
        } else {
            appendLine(&out, row)
        }
    }

    if processes.isEmpty {
        appendLine(&out, "No processes available.")
    }

    let shownEnd = processes.isEmpty ? 0 : end
    appendLine(&out, "")
    appendLine(&out, "Showing \(scrollTop + 1)-\(shownEnd) of \(processes.count)")
    out += "\u{001B}[J" // clear any leftover lines below current frame
    writeEsc(out)
    fflush(stdout)
}

if isatty(STDIN_FILENO) != 1 || isatty(STDOUT_FILENO) != 1 {
    fputs("stop requires an interactive terminal (TTY). Run it directly in a terminal session.\n", stderr)
    exit(1)
}

_ = atexit(rtop_restore_terminal)
_ = signal(SIGINT, rtop_signal_handler)
_ = signal(SIGTERM, rtop_signal_handler)
_ = signal(SIGHUP, rtop_signal_handler)
_ = signal(SIGQUIT, rtop_signal_handler)

guard let terminal = TerminalRawMode() else {
    fputs("failed to initialize terminal raw mode (tty setup failed)\n", stderr)
    exit(1)
}

var sampler = Sampler()
var selected = 0
var sortMode: SortMode = .cpu
var filter = ""
var isSearching = false
var latest = sampler.sample(sortMode: sortMode, filter: filter)
var running = true
var nextSample = Date()
var nextRender = Date()
let sampleInterval: TimeInterval = 0.5
let renderInterval: TimeInterval = 1.0 / 30.0
var needsRender = true

while running {
    let now = Date()
    let timeToSample = max(0, nextSample.timeIntervalSince(now))
    let timeToRender = needsRender ? max(0, nextRender.timeIntervalSince(now)) : timeToSample
    let waitTime = min(timeToSample, timeToRender)
    let timeoutMs = Int32(waitTime * 1000)

    var fds = [pollfd(fd: STDIN_FILENO, events: Int16(POLLIN), revents: 0)]
    let pollRet = poll(&fds, 1, timeoutMs)

    var hadInput = false
    let oldFilter = filter
    if pollRet > 0 && (fds[0].revents & Int16(POLLIN)) != 0 {
        while let key = readKey() {
            hadInput = true
            if isSearching {
                switch key {
                case .quit:
                    running = false
                case .enter:
                    isSearching = false
                case .esc:
                    isSearching = false
                    filter = ""
                case .backspace:
                    if !filter.isEmpty { filter.removeLast() }
                case .char(let c):
                    filter.append(c)
                default:
                    break
                }
            } else {
                switch key {
                case .quit:
                    running = false
                case .up:
                    selected -= 1
                case .down:
                    selected += 1
                case .left:
                    sortMode = .cpu
                case .right:
                    sortMode = .memory
                case .search:
                    isSearching = true
                default:
                    break
                }
            }
        }
    }

    if filter != oldFilter {
        selected = 0
    }

    if hadInput {
        if isSearching || filter != "" || sortMode != .cpu { // Rough check if we need to re-sample immediately
             latest = sampler.sample(sortMode: sortMode, filter: filter)
        }
        selected = clamp(selected, 0, max(0, latest.5.count - 1))
        needsRender = true
    }

    let nowPostPoll = Date()
    if nowPostPoll >= nextSample {
        latest = sampler.sample(sortMode: sortMode, filter: filter)
        selected = clamp(selected, 0, max(0, latest.5.count - 1))
        needsRender = true
        nextSample = nowPostPoll.addingTimeInterval(sampleInterval)
    }

    if needsRender && nowPostPoll >= nextRender {
        render(
            cpu: latest.0,
            cpuTemp: latest.1,
            memory: latest.2,
            network: latest.3,
            gpu: latest.4,
            processes: latest.5,
            selected: selected,
            cpuCount: latest.6,
            sortMode: sortMode,
            filter: filter,
            isSearching: isSearching
        )
        needsRender = false
        nextRender = nowPostPoll.addingTimeInterval(renderInterval)
    }
}

terminal.restoreNow()
