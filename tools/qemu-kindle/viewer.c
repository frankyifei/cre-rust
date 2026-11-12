/*
 * qemu-kindle-viewer: shows the emulated Kindle framebuffer in a window and
 * forwards mouse clicks into the guest as touch taps.
 *
 *   viewer <fb-file> <tap-fifo> [scale] [title]
 *
 * The framebuffer file is 758x1024, 8bpp grayscale (einkfb layout).
 * Mouse press maps window pixel -> framebuffer pixel and appends "x y\n" to
 * the tap FIFO, which fakekindle.so turns into evdev events.
 *
 * Build (host): cc -O2 -o qemu-kindle-viewer viewer.c -lX11
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <time.h>
#include <sys/stat.h>
#include <unistd.h>

#define FB_WIDTH 758
#define FB_HEIGHT 1024

static unsigned long last_hash = 0;

/* Window pixel -> framebuffer pixel.
 *
 * The window can be resized by the window manager (or scaled by the compositor,
 * e.g. WSLg on a HiDPI screen), so the mapping must use the window's *current*
 * size rather than the scale the window was created with; otherwise clicks land
 * on the wrong menu item. */
static int map_x(int x, int win_w)
{
    if (win_w <= 0) {
        return 0;
    }
    int fb_x = (int)((double)x * FB_WIDTH / (double)win_w);
    if (fb_x < 0) {
        fb_x = 0;
    }
    if (fb_x >= FB_WIDTH) {
        fb_x = FB_WIDTH - 1;
    }
    return fb_x;
}

static int map_y(int y, int win_h)
{
    if (win_h <= 0) {
        return 0;
    }
    int fb_y = (int)((double)y * FB_HEIGHT / (double)win_h);
    if (fb_y < 0) {
        fb_y = 0;
    }
    if (fb_y >= FB_HEIGHT) {
        fb_y = FB_HEIGHT - 1;
    }
    return fb_y;
}

static unsigned long frame_hash(const unsigned char *data, size_t size)
{
    unsigned long hash = 1469598103934665603UL;
    for (size_t i = 0; i < size; i++) {
        hash ^= data[i];
        hash *= 1099511628211UL;
    }
    return hash;
}

static void send_cmd(const char *fifo, char cmd, int x, int y)
{
    int fd = open(fifo, O_WRONLY | O_NONBLOCK);
    if (fd < 0) {
        return;
    }
    char line[64];
    int n = (cmd == 'u') ? snprintf(line, sizeof(line), "u\n")
                         : snprintf(line, sizeof(line), "%c %d %d\n", cmd, x, y);
    if (write(fd, line, (size_t)n) < 0) {
        fprintf(stderr, "viewer: write failed: %s\n", strerror(errno));
    }
    close(fd);
}

static int load_frame(const char *path, unsigned char *gray)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return 0;
    }
    size_t want = (size_t)FB_WIDTH * FB_HEIGHT;
    size_t got = 0;
    while (got < want) {
        ssize_t n = read(fd, gray + got, want - got);
        if (n <= 0) {
            break;
        }
        got += (size_t)n;
    }
    close(fd);
    return got == want;
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s <fb-file> <tap-fifo> [scale] [title]\n", argv[0]);
        return 2;
    }
    const char *fb_path = argv[1];
    const char *tap_path = argv[2];
    double scale = argc > 3 ? atof(argv[3]) : 0.75;
    if (scale <= 0.1 || scale > 2.0) {
        scale = 0.75;
    }
    const char *title = argc > 4 ? argv[4] : "Kindle PW2 - qemu";

    unsigned char *gray = malloc((size_t)FB_WIDTH * FB_HEIGHT);
    unsigned char *rgb = malloc((size_t)FB_WIDTH * FB_HEIGHT * 4);
    if (!gray || !rgb) {
        return 2;
    }

    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "viewer: cannot open display (is WSLg running?)\n");
        return 1;
    }
    int screen = DefaultScreen(dpy);
    unsigned depth = (unsigned)DefaultDepth(dpy, screen);
    Visual *visual = DefaultVisual(dpy, screen);
    if (depth != 24 && depth != 32) {
        fprintf(stderr, "viewer: unsupported depth %u\n", depth);
        return 1;
    }

    int win_w = (int)(FB_WIDTH * scale);
    int win_h = (int)(FB_HEIGHT * scale);
    Window win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 0, 0, (unsigned)win_w,
                                     (unsigned)win_h, 0, BlackPixel(dpy, screen),
                                     WhitePixel(dpy, screen));
    XStoreName(dpy, win, title);
    XSelectInput(dpy, win, ExposureMask | ButtonPressMask | ButtonReleaseMask |
                  Button1MotionMask | PointerMotionMask | StructureNotifyMask | KeyPressMask);
    XMapRaised(dpy, win);
    /* The window manager (or WSLg's compositor) may change the size we asked
     * for; the mapping and the drawing both have to follow the real one. */
    {
        XWindowAttributes attrs;
        if (XGetWindowAttributes(dpy, win, &attrs) && attrs.width > 0 && attrs.height > 0) {
            win_w = attrs.width;
            win_h = attrs.height;
        }
    }

    fprintf(stderr, "viewer: window mapped on %s (%dx%d, scale %.2f)\n",
            DisplayString(dpy), win_w, win_h, scale);
    GC gc = XCreateGC(dpy, win, 0, NULL);
    XImage *img = XCreateImage(dpy, visual, depth, ZPixmap, 0, (char *)rgb, (unsigned)FB_WIDTH,
                               (unsigned)FB_HEIGHT, 32, 0);
    if (!img) {
        fprintf(stderr, "viewer: XCreateImage failed\n");
        return 1;
    }

    int needs_redraw = 1;
    int running = 1;
    int dragging = 0;
    long last_motion_ms = 0;
    int last_mx = -1, last_my = -1;
    int press_x = -1, press_y = -1;
    int moved = 0;
    while (running) {
        fd_set fds;
        FD_ZERO(&fds);
        FD_SET(ConnectionNumber(dpy), &fds);
        struct timeval tv = { 0, 200000 };
        int ready = select(ConnectionNumber(dpy) + 1, &fds, NULL, NULL, &tv);
        if (ready > 0 && XPending(dpy)) {
            XEvent ev;
            XNextEvent(dpy, &ev);
            if (ev.type == Expose) {
                needs_redraw = 1;
            } else if (ev.type == ConfigureNotify) {
                if (ev.xconfigure.width != win_w || ev.xconfigure.height != win_h) {
                    win_w = ev.xconfigure.width;
                    win_h = ev.xconfigure.height;
                    needs_redraw = 1;
                }
            } else if (ev.type == ButtonPress && ev.xbutton.button == Button1) {
                int fb_x = map_x(ev.xbutton.x, win_w);
                int fb_y = map_y(ev.xbutton.y, win_h);
                dragging = 1;
                moved = 0;
                press_x = fb_x;
                press_y = fb_y;
                last_mx = fb_x;
                last_my = fb_y;
                send_cmd(tap_path, 'd', fb_x, fb_y);
            } else if (ev.type == MotionNotify && dragging) {
                int fb_x = map_x(ev.xmotion.x, win_w);
                int fb_y = map_y(ev.xmotion.y, win_h);
                struct timespec now;
                clock_gettime(CLOCK_MONOTONIC, &now);
                long now_ms = now.tv_sec * 1000L + now.tv_nsec / 1000000L;
                if (abs(fb_x - press_x) >= 8 || abs(fb_y - press_y) >= 8) {
                    moved = 1;
                }
                if (now_ms - last_motion_ms >= 30 &&
                    (abs(fb_x - last_mx) >= 2 || abs(fb_y - last_my) >= 2)) {
                    last_motion_ms = now_ms;
                    last_mx = fb_x;
                    last_my = fb_y;
                    send_cmd(tap_path, 'm', fb_x, fb_y);
                }
            } else if (ev.type == ButtonRelease && ev.xbutton.button == Button1) {
                if (dragging) {
                    int fb_x = map_x(ev.xbutton.x, win_w);
                    int fb_y = map_y(ev.xbutton.y, win_h);
                    if (moved) {
                        send_cmd(tap_path, 'm', fb_x, fb_y);
                        send_cmd(tap_path, 'u', 0, 0);
                    } else {
                        /* A click is a click regardless of how slowly the
                         * guest processes events: emit a complete tap and skip
                         * the up event so the guest cannot count it twice. */
                        send_cmd(tap_path, 't', press_x, press_y);
                    }
                    dragging = 0;
                }
            } else if (ev.type == DestroyNotify) {
                running = 0;
            } else if (ev.type == KeyPress) {
                KeySym sym = XLookupKeysym(&ev.xkey, 0);
                if (sym == XK_Escape) {
                    running = 0;
                }
            }
        }

        if (load_frame(fb_path, gray)) {
            unsigned long hash = frame_hash(gray, (size_t)FB_WIDTH * FB_HEIGHT);
            if (hash != last_hash) {
                last_hash = hash;
                needs_redraw = 1;
                for (int y = 0; y < FB_HEIGHT; y++) {
                    for (int x = 0; x < FB_WIDTH; x++) {
                        unsigned char v = gray[(size_t)y * FB_WIDTH + x];
                        size_t o = ((size_t)y * FB_WIDTH + x) * 4;
                        rgb[o + 0] = v;
                        rgb[o + 1] = v;
                        rgb[o + 2] = v;
                        rgb[o + 3] = 0;
                    }
                }
            }
        }

        if (needs_redraw) {
            if (win_w == FB_WIDTH && win_h == FB_HEIGHT) {
                XPutImage(dpy, win, gc, img, 0, 0, 0, 0, FB_WIDTH, FB_HEIGHT);
            } else {
                /* Scale by rendering into a window-sized image. */
                XImage *scaled = XCreateImage(dpy, visual, depth, ZPixmap, 0, NULL,
                                              (unsigned)win_w, (unsigned)win_h, 32, 0);
                scaled->data = calloc((size_t)win_w * win_h, 4);
                for (int y = 0; y < win_h; y++) {
                    int sy = y * FB_HEIGHT / win_h;
                    for (int x = 0; x < win_w; x++) {
                        int sx = x * FB_WIDTH / win_w;
                        unsigned char v = gray[(size_t)sy * FB_WIDTH + sx];
                        size_t o = (size_t)y * win_w + x;
                        unsigned char *p = (unsigned char *)scaled->data;
                        p[o * 4 + 0] = v;
                        p[o * 4 + 1] = v;
                        p[o * 4 + 2] = v;
                        p[o * 4 + 3] = 0;
                    }
                }
                XPutImage(dpy, win, gc, scaled, 0, 0, 0, 0, (unsigned)win_w, (unsigned)win_h);
                free(scaled->data);
                scaled->data = NULL;
                XDestroyImage(scaled);
            }
            XFlush(dpy);
            needs_redraw = 0;
        }
    }

    XDestroyImage(img);
    XFreeGC(dpy, gc);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    free(gray);
    free(rgb);
    return 0;
}
