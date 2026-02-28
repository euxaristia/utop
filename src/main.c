#define _GNU_SOURCE
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

// --- Data Structures ---

typedef struct {
  unsigned long long user, nice, system, idle, iowait, irq, softirq, steal;
} CpuTimes;

typedef struct {
  unsigned long long used_bytes, total_bytes;
  unsigned long long swap_used_bytes, swap_total_bytes;
  unsigned long long cma_used_bytes, cma_total_bytes;
} MemorySnapshot;

typedef struct {
  char name[64];
  double usage;
  unsigned long long mem_used, mem_total;
  double temp;
  bool has_usage, has_mem, has_temp;
} GpuSnapshot;

typedef struct {
  int pid;
  char name[256];
  double cpu_percent;
  unsigned long long mem_bytes;
  int threads;
} ProcessInfo;

typedef enum { SORT_CPU, SORT_MEM } SortMode;

typedef struct {
  char iface[32];
  double rx_rate, tx_rate;
} NetworkSnapshot;

typedef struct {
  char iface[32];
  unsigned long long rx, tx;
} NetStats;

// --- Global State ---

struct termios g_original_termios;
int g_termios_active = 0;

static void restore_terminal() {
  if (!g_termios_active)
    return;
  tcsetattr(STDIN_FILENO, TCSAFLUSH, &g_original_termios);
  printf("\x1B[?1049l\x1B[?25h\x1B[0m");
  fflush(stdout);
  g_termios_active = 0;
}

static void signal_handler(int sig) {
  restore_terminal();
  exit(sig);
}

static void init_terminal() {
  tcgetattr(STDIN_FILENO, &g_original_termios);
  struct termios raw = g_original_termios;
  raw.c_lflag &= ~(ECHO | ICANON | ISIG);
  tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw);
  fcntl(STDIN_FILENO, F_SETFL, fcntl(STDIN_FILENO, F_GETFL) | O_NONBLOCK);
  g_termios_active = 1;
  printf("\x1B[?1049h\x1B[2J\x1B[H\x1B[?25l");
  fflush(stdout);
}

// --- Sampler State ---

typedef struct {
  unsigned long long ticks;
  int pid;
} PidTicks;

typedef struct {
  char queue[32];
  unsigned long long last_ts;
  unsigned long long last_rt;
} V3dStats;

typedef struct {
  CpuTimes prev_cpu;
  PidTicks *prev_ticks;
  size_t prev_ticks_count;
  NetStats *prev_net;
  size_t prev_net_count;
  struct timeval last_sample;
  long page_size;
  V3dStats v3d_stats[16];
  int v3d_stats_count;
} Sampler;

static Sampler *sampler_create() {
  Sampler *s = calloc(1, sizeof(Sampler));
  if (!s)
    exit(1);
  s->page_size = sysconf(_SC_PAGESIZE);
  gettimeofday(&s->last_sample, NULL);
  return s;
}

// --- Utilities ---

static char *human_bytes(unsigned long long bytes) {
  static char bufs[8][32];
  static int idx = 0;
  idx = (idx + 1) % 8;
  char *buf = bufs[idx];
  double v = (double)bytes;
  if (v >= 1024.0 * 1024 * 1024)
    sprintf(buf, "%.2f GiB", v / (1024.0 * 1024 * 1024));
  else if (v >= 1024.0 * 1024)
    sprintf(buf, "%.1f MiB", v / (1024.0 * 1024));
  else if (v >= 1024.0)
    sprintf(buf, "%.1f KiB", v / 1024.0);
  else
    sprintf(buf, "%llu B", bytes);
  return buf;
}

// --- Implementation ---

static double read_cpu_temp() {
  DIR *dir = opendir("/sys/class/thermal");
  if (dir) {
    const struct dirent *entry;
    while ((entry = readdir(dir))) {
      if (strncmp(entry->d_name, "thermal_zone", 12) == 0) {
        char path[512];
        snprintf(path, sizeof(path), "/sys/class/thermal/%s/type",
                 entry->d_name);
        FILE *f = fopen(path, "r");
        if (f) {
          char type[256];
          if (fgets(type, sizeof(type), f)) {
            for (int i = 0; type[i]; i++)
              type[i] = tolower(type[i]);
            if (strstr(type, "pkg") || strstr(type, "cpu") ||
                strstr(type, "core") || strstr(type, "soc")) {
              fclose(f);
              snprintf(path, sizeof(path), "/sys/class/thermal/%s/temp",
                       entry->d_name);
              f = fopen(path, "r");
              if (f) {
                double t;
                if (fscanf(f, "%lf", &t) == 1) {
                  fclose(f);
                  closedir(dir);
                  return t / 1000.0;
                }
                fclose(f);
              }
            } else
              fclose(f);
          } else
            fclose(f);
        }
      }
    }
    closedir(dir);
  }
  // Fallback to hwmon
  dir = opendir("/sys/class/hwmon");
  if (dir) {
    const struct dirent *hwmon_entry;
    while ((hwmon_entry = readdir(dir))) {
      char path[512];
      snprintf(path, sizeof(path), "/sys/class/hwmon/%s/name",
               hwmon_entry->d_name);
      FILE *f = fopen(path, "r");
      if (f) {
        char name[256];
        if (fgets(name, sizeof(name), f)) {
          for (int i = 0; name[i]; i++)
            name[i] = tolower(name[i]);
          if (strstr(name, "coretemp") || strstr(name, "cpu") ||
              strstr(name, "k10temp")) {
            fclose(f);
            char subpath[512];
            snprintf(subpath, sizeof(subpath), "/sys/class/hwmon/%s",
                     hwmon_entry->d_name);
            DIR *subdir = opendir(subpath);
            if (subdir) {
              const struct dirent *subentry;
              double best = -1000.0;
              while ((subentry = readdir(subdir))) {
                if (strncmp(subentry->d_name, "temp", 4) == 0 &&
                    strstr(subentry->d_name, "_input")) {
                  char fpath[512];
                  snprintf(fpath, sizeof(fpath), "%s/%s", subpath,
                           subentry->d_name);
                  FILE *sf = fopen(fpath, "r");
                  if (sf) {
                    double t;
                    if (fscanf(sf, "%lf", &t) == 1) {
                      if (t / 1000.0 > best)
                        best = t / 1000.0;
                    }
                    fclose(sf);
                  }
                }
              }
              closedir(subdir);
              if (best > -1000.0) {
                closedir(dir);
                return best;
              }
            }
          } else
            fclose(f);
        } else
          fclose(f);
      }
    }
    closedir(dir);
  }
  return -1000.0;
}

static double read_cpu_freq() {
  FILE *f = fopen("/proc/cpuinfo", "r");
  if (f) {
    char line[256];
    double total = 0;
    int count = 0;
    while (fgets(line, sizeof(line), f)) {
      if (strncmp(line, "cpu MHz", 7) == 0) {
        const char *p = strchr(line, ':');
        if (p) {
          double freq;
          if (sscanf(p + 1, "%lf", &freq) == 1) {
            total += freq;
            count++;
          }
        }
      }
    }
    fclose(f);
    if (count > 0)
      return total / count;
  }
  // Fallback sysfs
  DIR *dir = opendir("/sys/devices/system/cpu");
  if (dir) {
    const struct dirent *entry;
    double total = 0;
    int count = 0;
    while ((entry = readdir(dir))) {
      if (strncmp(entry->d_name, "cpu", 3) == 0 && isdigit(entry->d_name[3])) {
        char path[512];
        snprintf(path, sizeof(path),
                 "/sys/devices/system/cpu/%s/cpufreq/scaling_cur_freq",
                 entry->d_name);
        FILE *fc = fopen(path, "r");
        if (fc) {
          double khz;
          if (fscanf(fc, "%lf", &khz) == 1) {
            total += khz / 1000.0;
            count++;
          }
          fclose(fc);
        }
      }
    }
    closedir(dir);
    if (count > 0)
      return total / count;
  }
  return 0;
}

static CpuTimes read_cpu_times() {
  CpuTimes t = {0};
  FILE *f = fopen("/proc/stat", "r");
  if (!f)
    return t;
  char line[256];
  if (fgets(line, sizeof(line), f)) {
    sscanf(line, "cpu %llu %llu %llu %llu %llu %llu %llu %llu", &t.user,
           &t.nice, &t.system, &t.idle, &t.iowait, &t.irq, &t.softirq,
           &t.steal);
  }
  fclose(f);
  return t;
}

static MemorySnapshot read_memory() {
  MemorySnapshot m = {0};
  FILE *f = fopen("/proc/meminfo", "r");
  if (!f)
    return m;
  char line[256];
  unsigned long long total = 0, avail = 0, s_total = 0, s_free = 0,
                     cma_total = 0, cma_free = 0;
  while (fgets(line, sizeof(line), f)) {
    if (strncmp(line, "MemTotal:", 9) == 0)
      sscanf(line + 9, "%llu", &total);
    else if (strncmp(line, "MemAvailable:", 13) == 0)
      sscanf(line + 13, "%llu", &avail);
    else if (strncmp(line, "SwapTotal:", 10) == 0)
      sscanf(line + 10, "%llu", &s_total);
    else if (strncmp(line, "SwapFree:", 9) == 0)
      sscanf(line + 9, "%llu", &s_free);
    else if (strncmp(line, "CmaTotal:", 9) == 0)
      sscanf(line + 9, "%llu", &cma_total);
    else if (strncmp(line, "CmaFree:", 8) == 0)
      sscanf(line + 8, "%llu", &cma_free);
  }
  fclose(f);
  m.total_bytes = total * 1024;
  m.used_bytes = (total - avail) * 1024;
  m.swap_total_bytes = s_total * 1024;
  m.swap_used_bytes = (s_total - s_free) * 1024;
  m.cma_total_bytes = cma_total * 1024;
  m.cma_used_bytes = (cma_total - cma_free) * 1024;
  return m;
}

static int read_cpu_count() {
  FILE *f = fopen("/proc/stat", "r");
  if (!f)
    return 1;
  char line[256];
  int count = 0;
  while (fgets(line, sizeof(line), f)) {
    if (strncmp(line, "cpu", 3) == 0 && isdigit(line[3]))
      count++;
  }
  fclose(f);
  return count > 0 ? count : 1;
}

static GpuSnapshot read_gpu(Sampler *s, MemorySnapshot mem) {
  static GpuSnapshot cached_gpu = {"GPU",   0,     0,     0,
                                   -1000.0, false, false, false};
  static struct timeval last_gpu_read = {0, 0};
  struct timeval now;
  gettimeofday(&now, NULL);

  long long ms_diff = (now.tv_sec - last_gpu_read.tv_sec) * 1000 +
                      (now.tv_usec - last_gpu_read.tv_usec) / 1000;
  if (ms_diff < 800 && last_gpu_read.tv_sec != 0)
    return cached_gpu;
  last_gpu_read = now;

  GpuSnapshot g = {"GPU", 0, 0, 0, -1000.0, false, false, false};

  // 1. NVIDIA via nvidia-smi
  FILE *fp = popen("/usr/bin/nvidia-smi "
                   "--query-gpu=utilization.gpu,memory.used,memory.total,"
                   "temperature.gpu --format=csv,noheader,nounits 2>/dev/null",
                   "r");
  if (fp) {
    char buf[256];
    if (fgets(buf, sizeof(buf), fp)) {
      const char *token;
      char *saveptr;
      int field = 0;
      token = strtok_r(buf, ",", &saveptr);
      while (token) {
        while (isspace(*token))
          token++;
        if (field == 0) {
          g.usage = atof(token);
          g.has_usage = true;
        } else if (field == 1) {
          g.mem_used = (unsigned long long)atoll(token) * 1024 * 1024;
          g.has_mem = true;
        } else if (field == 2) {
          g.mem_total = (unsigned long long)atoll(token) * 1024 * 1024;
        } else if (field == 3) {
          g.temp = atof(token);
          g.has_temp = true;
        }
        token = strtok_r(NULL, ",", &saveptr);
        field++;
      }
      if (g.has_usage) {
        strcpy(g.name, "NVIDIA GPU");
        pclose(fp);
        cached_gpu = g;
        return g;
      }
    }
    pclose(fp);
  }

  // 2. DRM / sysfs
  DIR *dir = opendir("/sys/class/drm");
  if (dir) {
    const struct dirent *entry;
    while ((entry = readdir(dir))) {
      if (strncmp(entry->d_name, "card", 4) == 0 &&
          !strchr(entry->d_name, '-')) {
        char path[512];
        bool found_usage = false;

        // Try common usage files
        const char *usage_files[] = {
            "/sys/class/drm/%s/device/gpu_busy_percent",
            "/sys/class/drm/%s/gt/gt0/usage", "/sys/class/drm/%s/device/usage",
            "/sys/class/drm/%s/device/load"};

        for (int i = 0; i < 4; i++) {
          snprintf(path, sizeof(path), usage_files[i], entry->d_name);
          FILE *f = fopen(path, "r");
          if (f) {
            if (fscanf(f, "%lf", &g.usage) == 1) {
              g.has_usage = true;
              found_usage = true;
              fclose(f);
              break;
            }
            fclose(f);
          }
        }

        // Try gpu_stats (v3d)
        if (!found_usage) {
          snprintf(path, sizeof(path), "/sys/class/drm/%s/device/gpu_stats",
                   entry->d_name);
          FILE *f_stats = fopen(path, "r");
          if (!f_stats) {
            // Fallback to debugfs
            char card_num = entry->d_name[4]; // card0 -> '0'
            if (isdigit(card_num)) {
              snprintf(path, sizeof(path), "/sys/kernel/debug/dri/%c/gpu_stats",
                       card_num);
              f_stats = fopen(path, "r");
            }
          }
          if (f_stats) {
            char line[256];
            fgets(line, sizeof(line), f_stats); // skip header
            while (fgets(line, sizeof(line), f_stats)) {
              char q_name[32];
              unsigned long long ts, rt;
              if (sscanf(line, "%31s %llu %*u %llu", q_name, &ts, &rt) == 3) {
                int idx = -1;
                for (int k = 0; k < s->v3d_stats_count; k++) {
                  if (strcmp(s->v3d_stats[k].queue, q_name) == 0) {
                    idx = k;
                    break;
                  }
                }
                if (idx == -1 && s->v3d_stats_count < 16) {
                  idx = s->v3d_stats_count++;
                  strcpy(s->v3d_stats[idx].queue, q_name);
                  s->v3d_stats[idx].last_ts = ts;
                  s->v3d_stats[idx].last_rt = rt;
                } else if (idx != -1) {
                  if (ts > s->v3d_stats[idx].last_ts) {
                    double q_u = (double)(rt - s->v3d_stats[idx].last_rt) *
                                 100.0 / (ts - s->v3d_stats[idx].last_ts);
                    if (q_u > g.usage)
                      g.usage = q_u;
                    g.has_usage = true;
                  }
                  s->v3d_stats[idx].last_ts = ts;
                  s->v3d_stats[idx].last_rt = rt;
                }
              }
            }
            fclose(f_stats);
          }
        }

        // Determine GPU name
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/vendor",
                 entry->d_name);
        FILE *fv = fopen(path, "r");
        if (fv) {
          char vendor[32];
          if (fgets(vendor, sizeof(vendor), fv)) {
            if (strstr(vendor, "0x1002"))
              strcpy(g.name, "AMD GPU");
            else if (strstr(vendor, "0x8086"))
              strcpy(g.name, "Intel GPU");
            else if (strstr(vendor, "0x10de"))
              strcpy(g.name, "NVIDIA GPU");
            else if (strstr(vendor, "0x14e4"))
              strcpy(g.name, "Broadcom GPU");
          }
          fclose(fv);
        } else {
          // Check uevent for driver name
          snprintf(path, sizeof(path), "/sys/class/drm/%s/device/uevent",
                   entry->d_name);
          FILE *fe = fopen(path, "r");
          if (fe) {
            char line[256];
            while (fgets(line, sizeof(line), fe)) {
              if (strstr(line, "DRIVER=v3d") || strstr(line, "DRIVER=vc4")) {
                strcpy(g.name, "VideoCore GPU");
                break;
              }
            }
            fclose(fe);
          }
        }

        // GPU Temp
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/hwmon",
                 entry->d_name);
        DIR *hdir = opendir(path);
        if (hdir) {
          const struct dirent *hentry;
          while ((hentry = readdir(hdir))) {
            if (strncmp(hentry->d_name, "hwmon", 5) == 0) {
              char tpath[512];
              snprintf(tpath, sizeof(tpath),
                       "/sys/class/drm/%s/device/hwmon/%s/temp1_input",
                       entry->d_name, hentry->d_name);
              FILE *tf = fopen(tpath, "r");
              if (tf) {
                if (fscanf(tf, "%lf", &g.temp) == 1) {
                  g.temp /= 1000.0;
                  g.has_temp = true;
                }
                fclose(tf);
                break;
              }
            }
          }
          closedir(hdir);
        }
        if (!g.has_temp) {
          FILE *tf = fopen("/sys/class/thermal/thermal_zone0/temp", "r");
          if (tf) {
            if (fscanf(tf, "%lf", &g.temp) == 1) {
              g.temp /= 1000.0;
              g.has_temp = true;
            }
            fclose(tf);
          }
        }

        // VRAM for Intel and others
        if (!g.has_mem) {
          snprintf(path, sizeof(path), "/sys/class/drm/%s/tile0/vram0/used",
                   entry->d_name);
          FILE *fm = fopen(path, "r");
          if (fm) {
            if (fscanf(fm, "%llu", &g.mem_used) == 1)
              g.has_mem = true;
            fclose(fm);
          }
          snprintf(path, sizeof(path), "/sys/class/drm/%s/tile0/vram0/size",
                   entry->d_name);
          fm = fopen(path, "r");
          if (fm) {
            fscanf(fm, "%llu", &g.mem_total);
            fclose(fm);
          }
        }

        // VRAM for VideoCore
        if (strcmp(g.name, "Broadcom GPU") == 0 ||
            strcmp(g.name, "VideoCore GPU") == 0 ||
            strcmp(g.name, "GPU") == 0) {
          if (mem.cma_total_bytes > 0) {
            g.mem_used = mem.cma_used_bytes;
            g.mem_total = mem.cma_total_bytes;
            g.has_mem = true;
            if (strcmp(g.name, "GPU") == 0)
              strcpy(g.name, "VideoCore GPU");
          }
        }

        if (g.has_usage) {
          closedir(dir);
          cached_gpu = g;
          return g;
        }
      }
    }
    closedir(dir);
  }

  // 3. Adreno / kgsl
  const char *adreno_paths[] = {"/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage",
                                "/sys/class/kgsl/kgsl-3d0/gpubusy"};
  for (int i = 0; i < 2; i++) {
    FILE *f = fopen(adreno_paths[i], "r");
    if (f) {
      double usage = 0;
      if (i == 1) { // gpubusy
        unsigned long long busy, total;
        if (fscanf(f, "%llu %llu", &busy, &total) == 2 && total > 0)
          usage = (double)busy * 100.0 / total;
      } else {
        fscanf(f, "%lf", &usage);
      }
      fclose(f);
      if (usage > 0 || i == 0) {
        strcpy(g.name, "Adreno GPU");
        g.usage = usage;
        g.has_usage = true;
        FILE *tf = fopen("/sys/class/thermal/thermal_zone0/temp", "r");
        if (tf) {
          if (fscanf(tf, "%lf", &g.temp) == 1) {
            g.temp /= 1000.0;
            g.has_temp = true;
          }
          fclose(tf);
        }
        cached_gpu = g;
        return g;
      }
    }
  }

  // 4. Generic devfreq (and RPI specific paths)
  const char *devfreq_dirs[] = {"/sys/class/devfreq",
                                "/sys/devices/platform/soc/soc:gpu/devfreq"};
  for (int d = 0; d < 2; d++) {
    dir = opendir(devfreq_dirs[d]);
    if (dir) {
      const struct dirent *entry;
      while ((entry = readdir(dir))) {
        if (strstr(entry->d_name, "v3d") || strstr(entry->d_name, "gpu") ||
            strstr(entry->d_name, "mali") || strstr(entry->d_name, "soc:gpu")) {
          char path[512];
          snprintf(path, sizeof(path), "%s/%s/load", devfreq_dirs[d],
                   entry->d_name);
          FILE *f = fopen(path, "r");
          if (f) {
            char load_buf[64];
            if (fgets(load_buf, sizeof(load_buf), f)) {
              char *at = strchr(load_buf, '@');
              if (at)
                *at = '\0';
              g.usage = atof(load_buf);
              g.has_usage = true;
              if (strstr(entry->d_name, "v3d") ||
                  strstr(entry->d_name, "soc:gpu"))
                strcpy(g.name, "VideoCore GPU");
              else if (strstr(entry->d_name, "mali"))
                strcpy(g.name, "Mali GPU");
              else if (strcmp(g.name, "GPU") == 0)
                strcpy(g.name, "GPU");

              fclose(f);
              if (!g.has_temp) {
                FILE *tf = fopen("/sys/class/thermal/thermal_zone0/temp", "r");
                if (tf) {
                  if (fscanf(tf, "%lf", &g.temp) == 1) {
                    g.temp /= 1000.0;
                    g.has_temp = true;
                  }
                  fclose(tf);
                }
              }
              closedir(dir);
              cached_gpu = g;
              return g;
            }
            fclose(f);
          }
        }
      }
      closedir(dir);
    }
  }

  // 5. Fallback for SoC (Broadcom/VideoCore) if DRM didn't pick it up
  if (!g.has_usage && !g.has_mem && mem.cma_total_bytes > 0) {
    strcpy(g.name, "VideoCore GPU");
    g.mem_used = mem.cma_used_bytes;
    g.mem_total = mem.cma_total_bytes;
    g.has_mem = true;
    if (!g.has_temp) {
      FILE *tf = fopen("/sys/class/thermal/thermal_zone0/temp", "r");
      if (tf) {
        if (fscanf(tf, "%lf", &g.temp) == 1) {
          g.temp /= 1000.0;
          g.has_temp = true;
        }
        fclose(tf);
      }
    }
  }

  cached_gpu = g;
  return g;
}

static int compare_procs(const void *a, const void *b, void *arg) {
  const ProcessInfo *pa = a, *pb = b;
  SortMode mode = *(SortMode *)arg;
  if (mode == SORT_CPU) {
    if (pb->cpu_percent > pa->cpu_percent)
      return 1;
    if (pb->cpu_percent < pa->cpu_percent)
      return -1;
    return (pb->mem_bytes > pa->mem_bytes) ? 1 : -1;
  } else {
    if (pb->mem_bytes > pa->mem_bytes)
      return 1;
    if (pb->mem_bytes < pa->mem_bytes)
      return -1;
    return (pb->cpu_percent > pa->cpu_percent) ? 1 : -1;
  }
}

static NetworkSnapshot read_network(Sampler *s, double elapsed) {
  NetworkSnapshot best = {"-", 0, 0};
  FILE *f = fopen("/proc/net/dev", "r");
  if (!f)
    return best;
  char line[512];
  fgets(line, sizeof(line), f); // skip header
  fgets(line, sizeof(line), f); // skip header

  NetStats *cur_net = malloc(16 * sizeof(NetStats));
  size_t cur_count = 0, cur_cap = 16;
  unsigned long long best_total = 0;

  while (fgets(line, sizeof(line), f)) {
    char iface[32];
    unsigned long long rx, tx;
    char *p = strchr(line, ':');
    if (!p)
      continue;
    *p = ' ';
    if (sscanf(line, "%31s %llu %*u %*u %*u %*u %*u %*u %*u %llu", iface, &rx,
               &tx) != 3)
      continue;
    if (strcmp(iface, "lo") == 0)
      continue;

    if (cur_count >= cur_cap) {
      cur_cap *= 2;
      void *tmp = realloc(cur_net, cur_cap * sizeof(NetStats));
      if (!tmp)
        exit(1);
      cur_net = tmp;
    }
    strcpy(cur_net[cur_count].iface, iface);
    cur_net[cur_count].rx = rx;
    cur_net[cur_count].tx = tx;
    cur_count++;

    unsigned long long prev_rx = rx, prev_tx = tx;
    for (size_t i = 0; i < s->prev_net_count; i++) {
      if (strcmp(s->prev_net[i].iface, iface) == 0) {
        prev_rx = s->prev_net[i].rx;
        prev_tx = s->prev_net[i].tx;
        break;
      }
    }

    double rx_r = (rx >= prev_rx ? rx - prev_rx : 0) / elapsed;
    double tx_r = (tx >= prev_tx ? tx - prev_tx : 0) / elapsed;
    if (rx + tx > best_total) {
      best_total = rx + tx;
      strcpy(best.iface, iface);
      best.rx_rate = rx_r;
      best.tx_rate = tx_r;
    }
  }
  fclose(f);
  if (s->prev_net)
    free(s->prev_net);
  s->prev_net = cur_net;
  s->prev_net_count = cur_count;
  return best;
}

static void sample(Sampler *s, SortMode sort, const char *filter,
                   ProcessInfo **out_procs, int *out_count, double *out_cpu,
                   MemorySnapshot *out_mem, NetworkSnapshot *out_net,
                   GpuSnapshot *out_gpu, int *out_cpus) {
  struct timeval now;
  gettimeofday(&now, NULL);
  double elapsed = (now.tv_sec - s->last_sample.tv_sec) +
                   (now.tv_usec - s->last_sample.tv_usec) / 1000000.0;
  if (elapsed < 0.001)
    elapsed = 0.001;
  s->last_sample = now;

  CpuTimes cur_cpu = read_cpu_times();
  unsigned long long total_prev = s->prev_cpu.user + s->prev_cpu.nice +
                                  s->prev_cpu.system + s->prev_cpu.idle +
                                  s->prev_cpu.iowait + s->prev_cpu.irq +
                                  s->prev_cpu.softirq + s->prev_cpu.steal;
  unsigned long long total_cur = cur_cpu.user + cur_cpu.nice + cur_cpu.system +
                                 cur_cpu.idle + cur_cpu.iowait + cur_cpu.irq +
                                 cur_cpu.softirq + cur_cpu.steal;
  unsigned long long total_delta = total_cur - total_prev;
  unsigned long long idle_delta =
      cur_cpu.idle + cur_cpu.iowait - (s->prev_cpu.idle + s->prev_cpu.iowait);

  *out_cpu = total_delta > 0
                 ? (double)(total_delta - idle_delta) * 100.0 / total_delta
                 : 0;
  *out_mem = read_memory();
  *out_net = read_network(s, elapsed);
  *out_gpu = read_gpu(s, *out_mem);
  *out_cpus = read_cpu_count();

  DIR *dir = opendir("/proc");
  if (!dir)
    return;
  const struct dirent *entry;
  int count = 0;
  int capacity = 256;
  ProcessInfo *procs = malloc(capacity * sizeof(ProcessInfo));
  PidTicks *cur_ticks = malloc(capacity * sizeof(PidTicks));
  int ticks_count = 0;

  while ((entry = readdir(dir))) {
    if (!isdigit(entry->d_name[0]))
      continue;
    int pid = atoi(entry->d_name);
    char path[512];
    snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0)
      continue;
    char buf[1024];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0)
      continue;
    buf[n] = 0;

    const char *p = strchr(buf, '(');
    const char *endp = strrchr(buf, ')');
    if (!p || !endp)
      continue;
    char name[256];
    size_t name_len = endp - p - 1;
    strncpy(name, p + 1, name_len);
    name[name_len] = 0;

    if (filter[0] != '\0') {
      char pid_str[16];
      sprintf(pid_str, "%d", pid);
      if (!strcasestr(name, filter) && !strstr(pid_str, filter))
        continue;
    }

    unsigned long long utime, stime;
    long rss;
    int threads;
    if (sscanf(endp + 2,
               "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %llu %llu %*d %*d "
               "%*d %*d %d %*d %*d %*d %ld",
               &utime, &stime, &threads, &rss) != 4)
      continue;

    unsigned long long total_ticks = utime + stime;
    if (ticks_count >= capacity) {
      capacity *= 2;
      void *tmp_ticks = realloc(cur_ticks, capacity * sizeof(PidTicks));
      if (!tmp_ticks)
        exit(1);
      cur_ticks = tmp_ticks;
      void *tmp_procs = realloc(procs, capacity * sizeof(ProcessInfo));
      if (!tmp_procs)
        exit(1);
      procs = tmp_procs;
    }
    cur_ticks[ticks_count++] = (PidTicks){total_ticks, pid};

    unsigned long long prev_t = 0;
    for (size_t i = 0; i < s->prev_ticks_count; i++) {
      if (s->prev_ticks[i].pid == pid) {
        prev_t = s->prev_ticks[i].ticks;
        break;
      }
    }

    double cpu_p = total_delta > 0
                       ? (double)(total_ticks - prev_t) * 100.0 / total_delta
                       : 0;
    procs[count++] = (ProcessInfo){
        pid, "", cpu_p, (unsigned long long)rss * s->page_size, threads};
    strcpy(procs[count - 1].name, name);
  }
  closedir(dir);

  free(s->prev_ticks);
  s->prev_ticks = cur_ticks;
  s->prev_ticks_count = ticks_count;
  s->prev_cpu = cur_cpu;

  qsort_r(procs, count, sizeof(ProcessInfo), compare_procs, &sort);
  *out_procs = procs;
  *out_count = count;
}

// --- Input Handling ---

typedef enum {
  K_NONE,
  K_QUIT,
  K_UP,
  K_DOWN,
  K_LEFT,
  K_RIGHT,
  K_BACKSPACE,
  K_ENTER,
  K_ESC,
  K_CHAR
} KeyType;

typedef struct {
  KeyType type;
  char ch;
} Key;

static Key read_key() {
  unsigned char buf[16];
  ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
  if (n <= 0)
    return (Key){K_NONE, 0};

  if (buf[0] == 3)
    return (Key){K_QUIT, 0}; // Ctrl+C

  if (n == 1) {
    if (buf[0] == 27)
      return (Key){K_ESC, 0};
    if (buf[0] == 127 || buf[0] == 8)
      return (Key){K_BACKSPACE, 0};
    if (buf[0] == 10 || buf[0] == 13)
      return (Key){K_ENTER, 0};
    if (isprint(buf[0]))
      return (Key){K_CHAR, buf[0]};
  } else if (n >= 3 && buf[0] == 27 && buf[1] == '[') {
    if (buf[2] == 'A')
      return (Key){K_UP, 0};
    if (buf[2] == 'B')
      return (Key){K_DOWN, 0};
    if (buf[2] == 'C')
      return (Key){K_RIGHT, 0};
    if (buf[2] == 'D')
      return (Key){K_LEFT, 0};
  }
  return (Key){K_NONE, 0};
}

int main() {
  signal(SIGINT, signal_handler);
  signal(SIGTERM, signal_handler);
  init_terminal();
  Sampler *sampler = sampler_create();
  SortMode sort = SORT_CPU;
  char filter[64] = {0};
  bool is_search = false;
  int selection = 0;

  struct timeval last_sample, last_render;
  gettimeofday(&last_sample, NULL);
  gettimeofday(&last_render, NULL);

  ProcessInfo *procs = NULL;
  int count = 0;
  double cpu = 0;
  MemorySnapshot mem = {0};
  NetworkSnapshot net = {"-", 0, 0};
  GpuSnapshot gpu = {"GPU", 0, 0, 0, -1000.0, false, false, false};
  int cpus = 1;
  bool needs_sample = true;
  bool needs_render = true;

  while (1) {
    struct timeval now;
    gettimeofday(&now, NULL);

    static double cpu_temp = -1000.0;
    static double cpu_freq = 0;

    // 1. Sampling (every 500ms)
    long long ms_since_sample = (now.tv_sec - last_sample.tv_sec) * 1000 +
                                (now.tv_usec - last_sample.tv_usec) / 1000;
    if (ms_since_sample >= 500 || needs_sample) {
      if (procs)
        free(procs);
      sample(sampler, sort, filter, &procs, &count, &cpu, &mem, &net, &gpu,
             &cpus);
      cpu_temp = read_cpu_temp();
      cpu_freq = read_cpu_freq();
      last_sample = now;
      needs_sample = false;
      needs_render = true;
    }

    // 2. Rendering (max 60 FPS)
    long long ms_since_render = (now.tv_sec - last_render.tv_sec) * 1000 +
                                (now.tv_usec - last_render.tv_usec) / 1000;
    if (needs_render && ms_since_render >= 16) {
      struct winsize ws;
      ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws);
      printf("\x1B[H");
      printf("utop (C version)    CPUs: %d\x1B[K\n", cpus);

      char temp_str[32] = {0};
      if (cpu_temp > -1000.0)
        sprintf(temp_str, " %.1f°C", cpu_temp);
      char freq_str[32] = {0};
      if (cpu_freq > 0)
        sprintf(freq_str, " @ %.2f GHz", cpu_freq / 1000.0);

      printf("CPU: %5.1f%%%s%s\x1B[K\n", cpu, freq_str, temp_str);
      printf("MEM: %5.1f%% %s / %s\x1B[K\n",
             (double)mem.used_bytes * 100.0 / mem.total_bytes,
             human_bytes(mem.used_bytes), human_bytes(mem.total_bytes));
      if (mem.swap_total_bytes > 0) {
        printf("SWP: %5.1f%% %s / %s\x1B[K\n",
               (double)mem.swap_used_bytes * 100.0 / mem.swap_total_bytes,
               human_bytes(mem.swap_used_bytes),
               human_bytes(mem.swap_total_bytes));
      } else {
        printf("\x1B[K\n");
      }
      if (mem.cma_total_bytes > 0) {
        printf("CMA: %5.1f%% %s / %s\x1B[K\n",
               (double)mem.cma_used_bytes * 100.0 / mem.cma_total_bytes,
               human_bytes(mem.cma_used_bytes),
               human_bytes(mem.cma_total_bytes));
      }

      if (gpu.has_usage || gpu.has_mem) {
        char g_temp[32] = {0}, g_vram[64] = {0}, g_usage[32] = {0};
        if (gpu.has_temp)
          sprintf(g_temp, " %.1f°C", gpu.temp);
        if (gpu.has_mem)
          sprintf(g_vram, "  VRAM: %5.1f%% %s / %s",
                  (double)gpu.mem_used * 100.0 / gpu.mem_total,
                  human_bytes(gpu.mem_used), human_bytes(gpu.mem_total));
        if (gpu.has_usage)
          sprintf(g_usage, "%5.1f%%", gpu.usage);
        printf("%s: %s%s%s\x1B[K\n", gpu.name, g_usage, g_temp, g_vram);
      } else {
        printf("GPU:\x1B[K\n");
      }

      printf("NET: %s  rx %s/s  tx %s/s\x1B[K\n", net.iface,
             human_bytes((unsigned long long)net.rx_rate),
             human_bytes((unsigned long long)net.tx_rate));
      printf("Controls: q:quit, j/k/arrows:move, h/l/arrows:sort, /:filter "
             "[%s]\x1B[K\n",
             is_search ? "SEARCHING" : "NORMAL");
      if (is_search)
        printf("Filter: /%s_\x1B[K\n", filter);
      else if (filter[0])
        printf("Filter: %s (press / to edit)\x1B[K\n", filter);
      else
        printf("\x1B[K\n");
      printf("\x1B[K\n");

      int pid_w = 7, cpu_w = 8, mem_w = 12, thr_w = 4;
      int name_w = (int)ws.ws_col - (pid_w + cpu_w + mem_w + thr_w + 5);
      if (name_w < 12)
        name_w = 12;

      char cpu_hdr[32], mem_hdr[32];
      if (sort == SORT_CPU)
        strcpy(cpu_hdr, "CPU%▼");
      else
        strcpy(cpu_hdr, "CPU%");
      if (sort == SORT_MEM)
        strcpy(mem_hdr, "MEM▼");
      else
        strcpy(mem_hdr, "MEM");

      printf("%-*s %-*s %*s %*s %*s\x1B[K\n", pid_w, "PID", name_w, "NAME",
             cpu_w + (sort == SORT_CPU ? 2 : 0), cpu_hdr,
             mem_w + (sort == SORT_MEM ? 2 : 0), mem_hdr, thr_w, "THR");
      for (int i = 0; i < (int)ws.ws_col &&
                      i < (pid_w + name_w + cpu_w + mem_w + thr_w + 4);
           i++)
        putchar('-');
      printf("\x1B[K\n");

      int visible = ws.ws_row - 12; // Adjusted for new header lines
      if (selection >= count)
        selection = (count > 0) ? count - 1 : 0;
      if (selection < 0)
        selection = 0;

      int scroll_top = selection - (visible / 2);
      if (scroll_top > count - visible)
        scroll_top = count - visible;
      if (scroll_top < 0)
        scroll_top = 0;

      for (int i = scroll_top; i < count && i < scroll_top + visible; i++) {
        if (i == selection)
          printf("\x1B[7m");
        printf("%-*d %-*.*s %*.1f %*s %*d\x1B[0m\x1B[K\n", pid_w, procs[i].pid,
               name_w, name_w, procs[i].name, cpu_w, procs[i].cpu_percent,
               mem_w, human_bytes(procs[i].mem_bytes), thr_w, procs[i].threads);
      }
      printf("\x1B[J");
      if (count > 0)
        printf("\x1B[%d;1HShowing %d-%d of %d\x1B[K", ws.ws_row, scroll_top + 1,
               (scroll_top + visible > count ? count : scroll_top + visible),
               count);
      fflush(stdout);
      last_render = now;
      needs_render = false;
    }

    struct pollfd pfd = {STDIN_FILENO, POLLIN, 0};
    if (poll(&pfd, 1, 10) > 0) {
      Key k;
      while ((k = read_key()).type != K_NONE) {
        if (k.type == K_QUIT) {
          restore_terminal();
          return 0;
        }
        if (is_search) {
          if (k.type == K_ESC || k.type == K_ENTER) {
            is_search = false;
            needs_render = true;
          } else if (k.type == K_BACKSPACE) {
            if (strlen(filter) > 0) {
              filter[strlen(filter) - 1] = 0;
              selection = 0;
              needs_sample = true;
            } else {
              is_search = false;
              needs_render = true;
            }
          } else if (k.type == K_CHAR) {
            if (strlen(filter) < 63) {
              size_t l = strlen(filter);
              filter[l] = k.ch;
              filter[l + 1] = 0;
              selection = 0;
              needs_sample = true;
            }
          }
        } else {
          if (k.type == K_UP) {
            if (selection > 0)
              selection--;
            needs_render = true;
          } else if (k.type == K_DOWN) {
            selection++;
            needs_render = true;
          } else if (k.type == K_LEFT) {
            sort = SORT_CPU;
            needs_sample = true;
          } else if (k.type == K_RIGHT) {
            sort = SORT_MEM;
            needs_sample = true;
          } else if (k.type == K_ESC) {
            if (filter[0]) {
              filter[0] = 0;
              selection = 0;
              needs_sample = true;
            }
          } else if (k.type == K_CHAR) {
            if (k.ch == 'q') {
              restore_terminal();
              return 0;
            }
            if (k.ch == 'j') {
              selection++;
              needs_render = true;
            }
            if (k.ch == 'k') {
              if (selection > 0)
                selection--;
              needs_render = true;
            }
            if (k.ch == 'h') {
              sort = SORT_CPU;
              needs_sample = true;
            }
            if (k.ch == 'l') {
              sort = SORT_MEM;
              needs_sample = true;
            }
            if (k.ch == '/') {
              is_search = true;
              filter[0] = 0;
              needs_render = true;
            }
          }
        }
      }
    }
  }
}
