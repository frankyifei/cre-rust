/*
 * fakekindle.so -- LD_PRELOAD guest shim that lets the genuine Kindle
 * KOReader build run under qemu-user on a normal Linux host.
 *
 * Behind the scenes:
 *   /proc/usid      -> $FAKEKINDLE_DIR/usid (PW2 serial => model detection)
 *   /dev/fb0        -> regular file + FBIOGET_* ioctls (758x1024, 8bpp gray)
 *   /dev/input/*    -> pipe + evdev ioctls; taps are injected either from the
 *                      environment (one-shot) or from a FIFO command channel,
 *                      which the X11 viewer uses for mouse input.
 *   pipe()/pipe2()  -> write ends are duplicated and kept open, because
 *                      KOReader's Kindle fake-event child cannot exec
 *                      lipc-wait-event under qemu-user (no binfmt_misc), and
 *                      a dead writer makes the input loop spin on EPIPE.
 *
 * Build: arm-kindlepw2-linux-gnueabi-gcc -shared -fPIC -O2 \
 *        -o fakekindle.so fakekindle.c -ldl -lpthread
 *
 * Env:
 *   FAKEKINDLE_DIR        state dir (default /tmp/fakekindle)
 *   FAKEKINDLE_TAP_FIFO   tap command FIFO (default $FAKEKINDLE_DIR/taps);
 *                         lines of "x y" (fb coordinates) inject one tap
 *   FAKEKINDLE_TAP_DELAY  if set, inject one tap this many seconds after the
 *   FAKEKINDLE_TAP_X/_Y   touchscreen opens (default x=225 y=610 = "Ignore")
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/fb.h>
#include <linux/input.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define DIR_DEFAULT "/tmp/fakekindle"
#define FB_WIDTH 758
#define FB_HEIGHT 1024
#define FB_BPP 8
#define FB_LINE_LENGTH (FB_WIDTH * FB_BPP / 8)
#define FB_SIZE ((size_t)FB_LINE_LENGTH * FB_HEIGHT)

static int fake_fb_fd = -1;
static int fake_input_fd = -1;
static int fake_input_write_fd = -1;

static const char *state_dir(void)
{
    const char *dir = getenv("FAKEKINDLE_DIR");
    return dir && *dir ? dir : DIR_DEFAULT;
}

static void build_path(const char *name, char *out, size_t out_size)
{
    snprintf(out, out_size, "%s/%s", state_dir(), name);
}

/* ---- fd bookkeeping ---------------------------------------------------- */

static void note_fd(const char *path, int fd)
{
    char fb[256], ev0[256], ev1[256];
    if (fd < 0 || path == NULL) {
        return;
    }
    build_path("fb0", fb, sizeof(fb));
    build_path("event0", ev0, sizeof(ev0));
    build_path("event1", ev1, sizeof(ev1));
    if (strcmp(path, "/dev/fb0") == 0 || strcmp(path, fb) == 0) {
        fake_fb_fd = fd;
    } else if (strcmp(path, "/dev/input/event0") == 0 || strcmp(path, "/dev/input/event1") == 0 ||
               strcmp(path, ev0) == 0 || strcmp(path, ev1) == 0) {
        fake_input_fd = fd;
    }
}

static int is_input_path(const char *path)
{
    return path != NULL &&
        (strcmp(path, "/dev/input/event0") == 0 || strcmp(path, "/dev/input/event1") == 0);
}

static int fake_input_pipe(void)
{
    int fds[2];
    if (pipe(fds) != 0) {
        return -1;
    }
    if (fake_input_write_fd >= 0) {
        close(fake_input_write_fd);
    }
    fake_input_write_fd = fds[1];
    fake_input_fd = fds[0];
    return fds[0];
}

static const char *redirect_path(const char *path, char *buf, size_t buf_size)
{
    static __thread char redirected[256];
    if (path == NULL) {
        return NULL;
    }
    if (strcmp(path, "/proc/usid") == 0) {
        build_path("usid", redirected, sizeof(redirected));
        return redirected;
    }
    if (strcmp(path, "/dev/fb0") == 0) {
        build_path("fb0", redirected, sizeof(redirected));
        return redirected;
    }
    (void)buf;
    (void)buf_size;
    return path;
}

/* ---- open family ------------------------------------------------------- */

#define OPEN_BODY(real_call, path_arg, flags_arg, mode_arg, note_path)            \
    do {                                                                          \
        if (is_input_path(path_arg)) {                                            \
            return fake_input_pipe();                                             \
        }                                                                         \
        int fd = real_call;                                                       \
        note_fd(note_path, fd);                                                   \
        return fd;                                                                \
    } while (0)

int open(const char *path, int flags, ...)
{
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open");
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    }
    OPEN_BODY(real(redirect_path(path, NULL, 0), flags, mode), path, flags, mode, path);
}

int open64(const char *path, int flags, ...)
{
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open64");
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    }
    OPEN_BODY(real(redirect_path(path, NULL, 0), flags, mode), path, flags, mode, path);
}

int openat(int dirfd, const char *path, int flags, ...)
{
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat");
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    }
    OPEN_BODY(real(dirfd, redirect_path(path, NULL, 0), flags, mode), path, flags, mode, path);
}

int openat64(int dirfd, const char *path, int flags, ...)
{
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat64");
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    }
    OPEN_BODY(real(dirfd, redirect_path(path, NULL, 0), flags, mode), path, flags, mode, path);
}

FILE *fopen(const char *path, const char *mode)
{
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen");
    return real(redirect_path(path, NULL, 0), mode);
}

FILE *fopen64(const char *path, const char *mode)
{
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen64");
    return real(redirect_path(path, NULL, 0), mode);
}

/* ---- stat family ------------------------------------------------------- */

#define STAT_WRAP(fn, argt)                                     \
    int fn(const char *path, argt *buf)                         \
    {                                                           \
        static int (*real)(const char *, argt *);               \
        if (!real) real = dlsym(RTLD_NEXT, #fn);                \
        return real(redirect_path(path, NULL, 0), buf);         \
    }

STAT_WRAP(stat, struct stat)
STAT_WRAP(lstat, struct stat)
STAT_WRAP(stat64, struct stat64)
STAT_WRAP(lstat64, struct stat64)

#define VER_STAT_WRAP(fn, argt)                                             \
    int fn(int ver, const char *path, argt *buf)                            \
    {                                                                       \
        static int (*real)(int, const char *, argt *);                      \
        if (!real) real = dlsym(RTLD_NEXT, #fn);                            \
        return real(ver, redirect_path(path, NULL, 0), buf);                \
    }

VER_STAT_WRAP(__xstat, struct stat)
VER_STAT_WRAP(__lxstat, struct stat)
VER_STAT_WRAP(__xstat64, struct stat64)
VER_STAT_WRAP(__lxstat64, struct stat64)

#define STATAT_WRAP(fn, argt)                                               \
    int fn(int dirfd, const char *path, argt *buf, int flags)               \
    {                                                                       \
        static int (*real)(int, const char *, argt *, int);                 \
        if (!real) real = dlsym(RTLD_NEXT, #fn);                            \
        return real(dirfd, redirect_path(path, NULL, 0), buf, flags);       \
    }

STATAT_WRAP(fstatat, struct stat)
STATAT_WRAP(fstatat64, struct stat64)

/* ---- ioctl ------------------------------------------------------------- */

static void fill_fb_fix(struct fb_fix_screeninfo *fi)
{
    memset(fi, 0, sizeof(*fi));
    snprintf((char *)fi->id, sizeof(fi->id), "fakeken");
    fi->smem_len = (unsigned)FB_SIZE;
    fi->type = FB_TYPE_PACKED_PIXELS;
    fi->visual = FB_VISUAL_TRUECOLOR;
    fi->line_length = FB_LINE_LENGTH;
    fi->accel = FB_ACCEL_NONE;
}

static void fill_fb_var(struct fb_var_screeninfo *vi)
{
    memset(vi, 0, sizeof(*vi));
    vi->xres = FB_WIDTH;
    vi->yres = FB_HEIGHT;
    vi->xres_virtual = FB_WIDTH;
    vi->yres_virtual = FB_HEIGHT;
    vi->bits_per_pixel = FB_BPP;
    vi->grayscale = 1;
    vi->width = 90;  /* mm */
    vi->height = 122;
}

int ioctl(int fd, unsigned long request, ...)
{
    static int (*real)(int, unsigned long, ...);
    if (!real) real = dlsym(RTLD_NEXT, "ioctl");

    va_list ap;
    va_start(ap, request);
    void *arg = va_arg(ap, void *);
    va_end(ap);

    if (fd >= 0 && fd == fake_input_fd) {
        unsigned long type = _IOC_TYPE(request);
        unsigned long nr = _IOC_NR(request);
        unsigned long size = _IOC_SIZE(request);
        if (type == 'E') {
            if (nr == 0x01) { *(int *)arg = 0x010001; return 0; }
            if (nr == 0x02) {
                struct input_id *id = (struct input_id *)arg;
                id->bustype = 0x18;
                id->vendor = 0x1;
                id->product = 0x2;
                id->version = 0x3;
                return 0;
            }
            if (nr == 0x06) {
                strncpy((char *)arg, "fake touchscreen", size);
                return 0;
            }
            if (nr >= 0x20 && nr <= 0x3f) {
                unsigned long ev = nr - 0x20;
                unsigned long *bits = (unsigned long *)arg;
                memset(bits, 0, size);
                if (ev == 0) {
                    bits[0] |= (1UL << EV_SYN) | (1UL << EV_KEY) | (1UL << EV_ABS);
                } else if (ev == EV_KEY) {
                    unsigned long codes[] = { BTN_TOUCH, BTN_TOOL_FINGER };
                    for (size_t i = 0; i < sizeof(codes) / sizeof(codes[0]); i++) {
                        bits[codes[i] / (8 * sizeof(unsigned long))] |=
                            1UL << (codes[i] % (8 * sizeof(unsigned long)));
                    }
                } else if (ev == EV_ABS) {
                    unsigned long codes[] = {
                        ABS_X, ABS_Y, ABS_PRESSURE, ABS_MT_SLOT,
                        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID,
                    };
                    for (size_t i = 0; i < sizeof(codes) / sizeof(codes[0]); i++) {
                        bits[codes[i] / (8 * sizeof(unsigned long))] |=
                            1UL << (codes[i] % (8 * sizeof(unsigned long)));
                    }
                }
                return 0;
            }
            if (nr >= 0x40 && nr <= 0x7f) {
                unsigned long axis = nr - 0x40;
                struct input_absinfo *ai = (struct input_absinfo *)arg;
                memset(ai, 0, size < sizeof(*ai) ? size : sizeof(*ai));
                switch (axis) {
                case ABS_X: case ABS_MT_POSITION_X:
                    ai->maximum = FB_WIDTH - 1; ai->resolution = 100; break;
                case ABS_Y: case ABS_MT_POSITION_Y:
                    ai->maximum = FB_HEIGHT - 1; ai->resolution = 100; break;
                case ABS_PRESSURE:
                    ai->maximum = 255; break;
                case ABS_MT_TRACKING_ID:
                    ai->maximum = 65535; break;
                default:
                    break;
                }
                return 0;
            }
            if (nr == 0x09) { memset(arg, 0, size); return 0; }
        }
        return 0;
    }

    if (fd >= 0 && fd == fake_fb_fd) {
        switch (request) {
        case FBIOGET_FSCREENINFO:
            fill_fb_fix((struct fb_fix_screeninfo *)arg);
            return 0;
        case FBIOGET_VSCREENINFO:
            fill_fb_var((struct fb_var_screeninfo *)arg);
            return 0;
        default:
            return 0;  /* accept refresh/blank/eink updates */
        }
    }
    return real(fd, request, arg);
}



/* ---- pipe write-end keepalive ------------------------------------------ */

static int kept_write_ends[16];
static int kept_write_end_count = 0;

static void keep_write_end(int fd)
{
    if (kept_write_end_count >= (int)(sizeof(kept_write_ends) / sizeof(kept_write_ends[0]))) {
        return;
    }
    int dup = fcntl(fd, F_DUPFD, 600);
    if (dup >= 0) {
        kept_write_ends[kept_write_end_count++] = dup;
    }
}

int pipe(int pipefd[2])
{
    static int (*real)(int[2]);
    if (!real) real = dlsym(RTLD_NEXT, "pipe");
    int r = real(pipefd);
    if (r == 0) keep_write_end(pipefd[1]);
    return r;
}

int pipe2(int pipefd[2], int flags)
{
    static int (*real)(int[2], int);
    if (!real) real = dlsym(RTLD_NEXT, "pipe2");
    int r = real(pipefd, flags);
    if (r == 0) keep_write_end(pipefd[1]);
    return r;
}

/* ---- exec interception ------------------------------------------------- */

static void park_lipc_child(void)
{
    for (;;) {
        pause();
    }
}

static int is_lipc_program(const char *path)
{
    return path != NULL && strstr(path, "lipc-wait-event") != NULL;
}

int execve(const char *path, char *const argv[], char *const envp[])
{
    static int (*real)(const char *, char *const[], char *const[]);
    if (!real) real = dlsym(RTLD_NEXT, "execve");
    if (is_lipc_program(path)) park_lipc_child();
    return real(path, argv, envp);
}

int execv(const char *path, char *const argv[])
{
    static int (*real)(const char *, char *const[]);
    if (!real) real = dlsym(RTLD_NEXT, "execv");
    if (is_lipc_program(path)) park_lipc_child();
    return real(path, argv);
}

int execvp(const char *file, char *const argv[])
{
    static int (*real)(const char *, char *const[]);
    if (!real) real = dlsym(RTLD_NEXT, "execvp");
    if (is_lipc_program(file)) park_lipc_child();
    return real(file, argv);
}

/* ---- tap injection ----------------------------------------------------- */

static int touch_active = 0;

static void send_events(struct input_event *evs, int count)
{
    if (fake_input_write_fd < 0) {
        return;
    }
    /* KOReader's gesture code uses event timestamps (tap/hold/swipe timing);
     * zeros make synthetic events look like a zero-length gesture. */
    struct timeval now;
    gettimeofday(&now, NULL);
    for (int i = 0; i < count; i++) {
        evs[i].time = now;
    }
    syscall(SYS_write, fake_input_write_fd, evs, (size_t)count * sizeof(struct input_event));
}

/* Touch protocol B down: slot 0, tracking id, both MT and legacy ABS axes. */
static void touch_down(int x, int y)
{
    struct input_event evs[10];
    memset(evs, 0, sizeof(evs));
    int i = 0;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_SLOT;        evs[i].value = 0; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_TRACKING_ID; evs[i].value = 1; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_X;              evs[i].value = x; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_Y;              evs[i].value = y; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_POSITION_X;  evs[i].value = x; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_POSITION_Y;  evs[i].value = y; i++;
    evs[i].type = EV_KEY; evs[i].code = BTN_TOOL_FINGER;    evs[i].value = 1; i++;
    evs[i].type = EV_KEY; evs[i].code = BTN_TOUCH;          evs[i].value = 1; i++;
    evs[i].type = EV_SYN; evs[i].code = SYN_REPORT;         evs[i].value = 0; i++;
    send_events(evs, i);
    touch_active = 1;
}

static void touch_move(int x, int y)
{
    struct input_event evs[6];
    memset(evs, 0, sizeof(evs));
    int i = 0;
    evs[i].type = EV_ABS; evs[i].code = ABS_X;             evs[i].value = x; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_Y;             evs[i].value = y; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_POSITION_X; evs[i].value = x; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_POSITION_Y; evs[i].value = y; i++;
    evs[i].type = EV_SYN; evs[i].code = SYN_REPORT;        evs[i].value = 0; i++;
    send_events(evs, i);
}

static void touch_up(void)
{
    /* Select slot 0 first: KOReader's MT decoder tracks a per-slot contact,
     * and a release frame without ABS_MT_SLOT is attached to no slot, so the
     * gesture detector never sees the contact lift. */
    struct input_event evs[6];
    memset(evs, 0, sizeof(evs));
    int i = 0;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_SLOT;        evs[i].value = 0; i++;
    evs[i].type = EV_ABS; evs[i].code = ABS_MT_TRACKING_ID; evs[i].value = -1; i++;
    evs[i].type = EV_KEY; evs[i].code = BTN_TOUCH;          evs[i].value = 0; i++;
    evs[i].type = EV_KEY; evs[i].code = BTN_TOOL_FINGER;    evs[i].value = 0; i++;
    evs[i].type = EV_SYN; evs[i].code = SYN_REPORT;         evs[i].value = 0; i++;
    send_events(evs, i);
    touch_active = 0;
}

static void inject_tap(int x, int y)
{
    touch_down(x, y);
    usleep(60000);
    touch_up();
    fprintf(stderr, "[fakekindle] tap %d,%d\n", x, y);
}

static void *tap_thread(void *unused)
{
    (void)unused;
    while (fake_input_write_fd < 0) {
        usleep(50000);
    }

    const char *delay_env = getenv("FAKEKINDLE_TAP_DELAY");
    if (delay_env && *delay_env) {
        sleep((unsigned)atoi(delay_env));
        int x = 225, y = 610;
        const char *env;
        if ((env = getenv("FAKEKINDLE_TAP_X"))) x = atoi(env);
        if ((env = getenv("FAKEKINDLE_TAP_Y"))) y = atoi(env);
        inject_tap(x, y);
    }

    char fifo[256];
    const char *fifo_env = getenv("FAKEKINDLE_TAP_FIFO");
    if (fifo_env && *fifo_env) {
        snprintf(fifo, sizeof(fifo), "%s", fifo_env);
    } else {
        build_path("taps", fifo, sizeof(fifo));
    }
    /* A reusable FIFO: reopen on EOF so the viewer can restart. */
    for (;;) {
        int fd = open(fifo, O_RDONLY | O_NONBLOCK);
        if (fd < 0) {
            usleep(200000);
            continue;
        }
        char buf[128];
        size_t used = 0;
        for (;;) {
            ssize_t n = read(fd, buf + used, sizeof(buf) - 1 - used);
            if (n > 0) {
                used += (size_t)n;
                buf[used] = '\0';
                char *line = buf;
                char *nl;
                while ((nl = strchr(line, '\n')) != NULL) {
                    *nl = '\0';
                    int x = 0, y = 0;
                    char cmd = line[0] == '\0' ? 't' : line[0];
                    if (cmd == 'd' || cmd == 'm' || cmd == 'u') {
                        if (cmd == 'u') {
                            if (touch_active) touch_up();
                        } else if (sscanf(line + 1, "%d %d", &x, &y) == 2) {
                            if (cmd == 'd') {
                                touch_down(x, y);
                            } else if (touch_active) {
                                touch_move(x, y);
                            }
                        }
                    } else if (cmd == 't') {
                        if (sscanf(line + 1, "%d %d", &x, &y) == 2) {
                            inject_tap(x, y);
                        }
                    } else if (sscanf(line, "%d %d", &x, &y) == 2) {
                        inject_tap(x, y);
                    }
                    line = nl + 1;
                }
                used = strlen(line);
                memmove(buf, line, used);
                continue;
            }
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                usleep(30000);
                continue;
            }
            break;  /* EOF: writer went away; reopen */
        }
        close(fd);
        usleep(100000);
    }
    return NULL;
}

__attribute__((constructor)) static void fakekindle_init(void)
{
    pthread_t thread;
    if (pthread_create(&thread, NULL, tap_thread, NULL) == 0) {
        pthread_detach(thread);
    }
}
