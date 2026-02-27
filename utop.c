#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <dirent.h>
#include <fcntl.h>
#include <termios.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <ctype.h>
#include <stdbool.h>
#include <time.h>
#include <poll.h>

// --- Data Structures ---

typedef struct {
    unsigned long long user, nice, system, idle, iowait, irq, softirq, steal;
} CpuTimes;

typedef struct {
    unsigned long long used_bytes, total_bytes;
    unsigned long long swap_used_bytes, swap_total_bytes;
} MemorySnapshot;

typedef struct {
    int pid;
    char name[256];
    double cpu_percent;
    unsigned long long mem_bytes;
    int threads;
} ProcessInfo;

typedef enum { SORT_CPU, SORT_MEM } SortMode;

// --- Global State ---

struct termios g_original_termios;
int g_termios_active = 0;

void restore_terminal() {
    if (!g_termios_active) return;
    tcsetattr(STDIN_FILENO, TCSAFLUSH, &g_original_termios);
    printf("\x1B[?1049l\x1B[?25h\x1B[0m");
    fflush(stdout);
    g_termios_active = 0;
}

void signal_handler(int sig) {
    restore_terminal();
    exit(sig);
}

void init_terminal() {
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
    CpuTimes prev_cpu;
    PidTicks *prev_ticks;
    size_t prev_ticks_count;
    struct timeval last_sample;
    long page_size;
} Sampler;

Sampler* sampler_create() {
    Sampler *s = calloc(1, sizeof(Sampler));
    s->page_size = sysconf(_SC_PAGESIZE);
    gettimeofday(&s->last_sample, NULL);
    return s;
}

// --- Utilities ---

char* human_bytes(unsigned long long bytes) {
    static char buf[32];
    double v = (double)bytes;
    if (v >= 1024*1024*1024) sprintf(buf, "%.2f GiB", v / (1024*1024*1024));
    else if (v >= 1024*1024) sprintf(buf, "%.1f MiB", v / (1024*1024));
    else if (v >= 1024) sprintf(buf, "%.1f KiB", v / 1024);
    else sprintf(buf, "%llu B", bytes);
    return buf;
}

// --- Implementation ---

CpuTimes read_cpu_times() {
    CpuTimes t = {0};
    FILE *f = fopen("/proc/stat", "r");
    if (!f) return t;
    char line[256];
    if (fgets(line, sizeof(line), f)) {
        sscanf(line, "cpu %llu %llu %llu %llu %llu %llu %llu %llu",
               &t.user, &t.nice, &t.system, &t.idle, &t.iowait, &t.irq, &t.softirq, &t.steal);
    }
    fclose(f);
    return t;
}

MemorySnapshot read_memory() {
    MemorySnapshot m = {0};
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f) return m;
    char line[256];
    unsigned long long total = 0, avail = 0, s_total = 0, s_free = 0;
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "MemTotal:", 9) == 0) sscanf(line + 9, "%llu", &total);
        else if (strncmp(line, "MemAvailable:", 13) == 0) sscanf(line + 13, "%llu", &avail);
        else if (strncmp(line, "SwapTotal:", 10) == 0) sscanf(line + 10, "%llu", &s_total);
        else if (strncmp(line, "SwapFree:", 9) == 0) sscanf(line + 9, "%llu", &s_free);
    }
    fclose(f);
    m.total_bytes = total * 1024;
    m.used_bytes = (total - avail) * 1024;
    m.swap_total_bytes = s_total * 1024;
    m.swap_used_bytes = (s_total - s_free) * 1024;
    return m;
}

int compare_procs(const void *a, const void *b, void *arg) {
    const ProcessInfo *pa = a, *pb = b;
    SortMode mode = *(SortMode*)arg;
    if (mode == SORT_CPU) {
        if (pb->cpu_percent > pa->cpu_percent) return 1;
        if (pb->cpu_percent < pa->cpu_percent) return -1;
        return (pb->mem_bytes > pa->mem_bytes) ? 1 : -1;
    } else {
        if (pb->mem_bytes > pa->mem_bytes) return 1;
        if (pb->mem_bytes < pa->mem_bytes) return -1;
        return (pb->cpu_percent > pa->cpu_percent) ? 1 : -1;
    }
}

void sample(Sampler *s, SortMode sort, const char *filter, ProcessInfo **out_procs, int *out_count, double *out_cpu, MemorySnapshot *out_mem) {
    CpuTimes cur_cpu = read_cpu_times();
    unsigned long long total_prev = s->prev_cpu.user + s->prev_cpu.nice + s->prev_cpu.system + s->prev_cpu.idle + s->prev_cpu.iowait + s->prev_cpu.irq + s->prev_cpu.softirq + s->prev_cpu.steal;
    unsigned long long total_cur = cur_cpu.user + cur_cpu.nice + cur_cpu.system + cur_cpu.idle + cur_cpu.iowait + cur_cpu.irq + cur_cpu.softirq + cur_cpu.steal;
    unsigned long long total_delta = total_cur - total_prev;
    unsigned long long idle_delta = cur_cpu.idle + cur_cpu.iowait - (s->prev_cpu.idle + s->prev_cpu.iowait);
    
    *out_cpu = total_delta > 0 ? (double)(total_delta - idle_delta) * 100.0 / total_delta : 0;
    *out_mem = read_memory();

    DIR *dir = opendir("/proc");
    if (!dir) return;
    struct dirent *entry;
    int count = 0;
    int capacity = 256;
    ProcessInfo *procs = malloc(capacity * sizeof(ProcessInfo));
    PidTicks *cur_ticks = malloc(capacity * sizeof(PidTicks));
    int ticks_count = 0;

    while ((entry = readdir(dir))) {
        if (!isdigit(entry->d_name[0])) continue;
        int pid = atoi(entry->d_name);
        char path[512];
        snprintf(path, sizeof(path), "/proc/%d/stat", pid);
        int fd = open(path, O_RDONLY);
        if (fd < 0) continue;
        char buf[1024];
        ssize_t n = read(fd, buf, sizeof(buf)-1);
        close(fd);
        if (n <= 0) continue;
        buf[n] = 0;

        char *p = strchr(buf, '(');
        char *endp = strrchr(buf, ')');
        if (!p || !endp) continue;
        char name[256];
        size_t name_len = endp - p - 1;
        strncpy(name, p + 1, name_len);
        name[name_len] = 0;

        if (filter[0] != '\0') {
            char pid_str[16]; sprintf(pid_str, "%d", pid);
            if (!strcasestr(name, filter) && !strstr(pid_str, filter)) continue;
        }

        unsigned long long utime, stime;
        long rss;
        int threads;
        if (sscanf(endp + 2, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %llu %llu %*d %*d %*d %*d %d %*d %*d %*d %ld", 
                   &utime, &stime, &threads, &rss) != 4) continue;
        
        unsigned long long total_ticks = utime + stime;
        if (ticks_count >= capacity) {
            capacity *= 2;
            cur_ticks = realloc(cur_ticks, capacity * sizeof(PidTicks));
            procs = realloc(procs, capacity * sizeof(ProcessInfo));
        }
        cur_ticks[ticks_count++] = (PidTicks){total_ticks, pid};

        unsigned long long prev_t = 0;
        for (size_t i = 0; i < s->prev_ticks_count; i++) {
            if (s->prev_ticks[i].pid == pid) { prev_t = s->prev_ticks[i].ticks; break; }
        }

        double cpu_p = total_delta > 0 ? (double)(total_ticks - prev_t) * 100.0 / total_delta : 0;
        procs[count++] = (ProcessInfo){pid, "", cpu_p, (unsigned long long)rss * s->page_size, threads};
        strcpy(procs[count-1].name, name);
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
    K_NONE, K_QUIT, K_UP, K_DOWN, K_LEFT, K_RIGHT,
    K_BACKSPACE, K_ENTER, K_ESC, K_CHAR
} KeyType;

typedef struct {
    KeyType type;
    char ch;
} Key;

Key read_key() {
    unsigned char buf[16];
    ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
    if (n <= 0) return (Key){K_NONE, 0};
    
    if (buf[0] == 3) return (Key){K_QUIT, 0}; // Ctrl+C
    
    if (n == 1) {
        if (buf[0] == 27) return (Key){K_ESC, 0};
        if (buf[0] == 127 || buf[0] == 8) return (Key){K_BACKSPACE, 0};
        if (buf[0] == 10 || buf[0] == 13) return (Key){K_ENTER, 0};
        if (isprint(buf[0])) return (Key){K_CHAR, buf[0]};
    } else if (n >= 3 && buf[0] == 27 && buf[1] == '[') {
        if (buf[2] == 'A') return (Key){K_UP, 0};
        if (buf[2] == 'B') return (Key){K_DOWN, 0};
        if (buf[2] == 'C') return (Key){K_RIGHT, 0};
        if (buf[2] == 'D') return (Key){K_LEFT, 0};
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
    bool needs_sample = true;
    bool needs_render = true;

    while (1) {
        struct timeval now;
        gettimeofday(&now, NULL);

        // 1. Sampling (every 500ms)
        long long ms_since_sample = (now.tv_sec - last_sample.tv_sec) * 1000 + (now.tv_usec - last_sample.tv_usec) / 1000;
        if (ms_since_sample >= 500 || needs_sample) {
            if (procs) free(procs);
            sample(sampler, sort, filter, &procs, &count, &cpu, &mem);
            last_sample = now;
            needs_sample = false;
            needs_render = true;
        }

        // 2. Rendering (max 60 FPS)
        long long ms_since_render = (now.tv_sec - last_render.tv_sec) * 1000 + (now.tv_usec - last_render.tv_usec) / 1000;
        if (needs_render && ms_since_render >= 16) {
            struct winsize ws;
            ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws);
            printf("\x1B[H");
            printf("utop (C version)\x1B[K\n");
            printf("CPU: %5.1f%%    MEM: %5.1f%% %s / %s\x1B[K\n", cpu, (double)mem.used_bytes * 100.0 / mem.total_bytes, human_bytes(mem.used_bytes), human_bytes(mem.total_bytes));
            printf("Controls: q:quit, j/k/arrows:move, h/l/arrows:sort, /:filter [%s]\x1B[K\n", is_search ? "SEARCHING" : "NORMAL");
            if (is_search) printf("Filter: /%s_\x1B[K\n", filter); 
            else if (filter[0]) printf("Filter: %s (press / to edit)\x1B[K\n", filter); 
            else printf("\x1B[K\n");
            printf("\x1B[K\n");

            int pid_w = 7, cpu_w = 8, mem_w = 12, thr_w = 4;
            int name_w = (int)ws.ws_col - (pid_w + cpu_w + mem_w + thr_w + 5);
            if (name_w < 12) name_w = 12;

            char cpu_hdr[32], mem_hdr[32];
            if (sort == SORT_CPU) strcpy(cpu_hdr, "CPU%▼"); else strcpy(cpu_hdr, "CPU%");
            if (sort == SORT_MEM) strcpy(mem_hdr, "MEM▼"); else strcpy(mem_hdr, "MEM");

            printf("%-*s %-*s %*s %*s %*s\x1B[K\n", pid_w, "PID", name_w, "NAME", cpu_w + (sort == SORT_CPU ? 2 : 0), cpu_hdr, mem_w + (sort == SORT_MEM ? 2 : 0), mem_hdr, thr_w, "THR");
            for (int i = 0; i < (int)ws.ws_col && i < (pid_w + name_w + cpu_w + mem_w + thr_w + 4); i++) putchar('-');
            printf("\x1B[K\n");

            int visible = ws.ws_row - 9;
            if (selection >= count) selection = (count > 0) ? count - 1 : 0;
            if (selection < 0) selection = 0;

            int scroll_top = selection - (visible / 2);
            if (scroll_top > count - visible) scroll_top = count - visible;
            if (scroll_top < 0) scroll_top = 0;

            for (int i = scroll_top; i < count && i < scroll_top + visible; i++) {
                if (i == selection) printf("\x1B[7m");
                printf("%-*d %-*.*s %*.1f %*s %*d\x1B[0m\x1B[K\n", pid_w, procs[i].pid, name_w, name_w, procs[i].name, cpu_w, procs[i].cpu_percent, mem_w, human_bytes(procs[i].mem_bytes), thr_w, procs[i].threads);
            }
            printf("\x1B[J");
            if (count > 0) printf("\x1B[%d;1HShowing %d-%d of %d\x1B[K", ws.ws_row, scroll_top + 1, (scroll_top + visible > count ? count : scroll_top + visible), count);
            fflush(stdout);
            last_render = now;
            needs_render = false;
        }

        struct pollfd pfd = { STDIN_FILENO, POLLIN, 0 };
        if (poll(&pfd, 1, 10) > 0) {
            Key k;
            while ((k = read_key()).type != K_NONE) {
                if (k.type == K_QUIT) { restore_terminal(); return 0; }
                if (is_search) {
                    if (k.type == K_ESC || k.type == K_ENTER) is_search = false;
                    else if (k.type == K_BACKSPACE) { 
                        if (strlen(filter) > 0) { filter[strlen(filter)-1] = 0; selection = 0; needs_sample = true; } 
                        else { is_search = false; }
                    }
                    else if (k.type == K_CHAR) { if (strlen(filter) < 63) { size_t l = strlen(filter); filter[l] = k.ch; filter[l+1] = 0; selection = 0; needs_sample = true; } }
                } else {
                    if (k.type == K_UP) { if (selection > 0) selection--; needs_render = true; }
                    else if (k.type == K_DOWN) { selection++; needs_render = true; }
                    else if (k.type == K_LEFT) { sort = SORT_CPU; needs_sample = true; }
                    else if (k.type == K_RIGHT) { sort = SORT_MEM; needs_sample = true; }
                    else if (k.type == K_ESC) { if (filter[0]) { filter[0] = 0; selection = 0; needs_sample = true; } }
                    else if (k.type == K_CHAR) {
                        if (k.ch == 'q') { restore_terminal(); return 0; }
                        if (k.ch == 'j') { selection++; needs_render = true; }
                        if (k.ch == 'k') { if (selection > 0) selection--; needs_render = true; }
                        if (k.ch == 'h') { sort = SORT_CPU; needs_sample = true; }
                        if (k.ch == 'l') { sort = SORT_MEM; needs_sample = true; }
                        if (k.ch == '/') { is_search = true; filter[0] = 0; }
                    }
                }
            }
        }
    }
}
