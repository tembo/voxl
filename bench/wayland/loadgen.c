// Identical CPU-rendered workloads on native Wayland and X11. See README.md.
#define _GNU_SOURCE
#include "xdg-shell-client-protocol.h"
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

static int W, H, configured, closed;
static volatile sig_atomic_t steps;
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm;
struct buffer {
  uint32_t *pixels;
  struct wl_buffer *wl;
  int busy;
  XImage *x;
  XShmSegmentInfo si;
};
static void die(const char *what) {
  perror(what);
  exit(1);
}
static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}
static void step(int sig) {
  (void)sig;
  steps++;
}
static void release(void *data, struct wl_buffer *b) {
  (void)b;
  ((struct buffer *)data)->busy = 0;
}
static const struct wl_buffer_listener buffer_listener = {.release = release};
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial) {
  (void)data;
  xdg_wm_base_pong(base, serial);
}
static const struct xdg_wm_base_listener wm_listener = {.ping = ping};
static void global(void *data, struct wl_registry *r, uint32_t name,
                   const char *iface, uint32_t version) {
  (void)data;
  if (!strcmp(iface, "wl_compositor"))
    compositor = wl_registry_bind(r, name, &wl_compositor_interface,
                                  version < 4 ? version : 4);
  if (!strcmp(iface, "wl_shm"))
    shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
  if (!strcmp(iface, "xdg_wm_base")) {
    wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
    xdg_wm_base_add_listener(wm, &wm_listener, NULL);
  }
}
static void removed(void *d, struct wl_registry *r, uint32_t name) {
  (void)d;
  (void)r;
  (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = global, .global_remove = removed};
static void configure(void *d, struct xdg_surface *s, uint32_t serial) {
  (void)d;
  xdg_surface_ack_configure(s, serial);
  configured = 1;
}
static const struct xdg_surface_listener surface_listener = {.configure =
                                                                 configure};
static void size(void *d, struct xdg_toplevel *t, int32_t w, int32_t h,
                 struct wl_array *states) {
  (void)d;
  (void)t;
  (void)states;
  if (w && h && (w != W || h != H)) {
    fprintf(stderr, "output is %dx%d, expected %dx%d\n", w, h, W, H);
    exit(1);
  }
}
static void close_window(void *d, struct xdg_toplevel *t) {
  (void)d;
  (void)t;
  closed = 1;
}
static const struct xdg_toplevel_listener top_listener = {
    .configure = size, .close = close_window};
static void pump(void) {
  while (wl_display_prepare_read(display) != 0)
    if (wl_display_dispatch_pending(display) < 0)
      die("dispatch");
  wl_display_flush(display);
  struct pollfd p = {wl_display_get_fd(display), POLLIN, 0};
  if (poll(&p, 1, 0) > 0) {
    if (wl_display_read_events(display) < 0)
      die("read events");
  } else
    wl_display_cancel_read(display);
  if (wl_display_dispatch_pending(display) < 0)
    die("dispatch pending");
}
static void rect(uint32_t *p, int x, int y, int w, int h, uint32_t color) {
  for (int row = y; row < y + h; row++)
    for (int col = x; col < x + w; col++)
      p[(size_t)row * W + col] = color;
}
static void marker(uint32_t *p, long f) {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  uint32_t ms = (uint32_t)((tv.tv_sec * 1000LL + tv.tv_usec / 1000) & 0xffffff);
  uint64_t v = ((uint64_t)(f & 0xffff) << 24) | ms;
  for (int b = 0; b < 40; b++)
    rect(p, b % 20 * 24, b / 20 * 24, 24, 24,
         (v >> b) & 1 ? 0xffffffff : 0xff000000);
}
int main(int argc, char **argv) {
  if (argc != 8) {
    fprintf(stderr,
            "usage: %s x11|wayland scroll|sparse|idle|manual width height hz "
            "seconds label\n",
            argv[0]);
    return 1;
  }
  int wayland = !strcmp(argv[1], "wayland"),
      scroll = !strcmp(argv[2], "scroll"), manual = !strcmp(argv[2], "manual"),
      idle = !strcmp(argv[2], "idle");
  W = atoi(argv[3]);
  H = atoi(argv[4]);
  double hz = atof(argv[5]), secs = atof(argv[6]);
  // Last argument reserves a label for the run, included in the result.
  if (W < 1280 || H < 1200 || hz <= 0 || secs <= 0)
    return 1;
  struct buffer buffers[2] = {0};
  struct wl_surface *surface = NULL;
  Display *xd = NULL;
  Window win = 0;
  GC gc = 0;
  if (wayland) {
    display = wl_display_connect(NULL);
    if (!display)
      die("wl_display_connect");
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    if (!compositor || !shm || !wm) {
      fprintf(stderr, "missing Wayland globals\n");
      return 1;
    }
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xdg = xdg_wm_base_get_xdg_surface(wm, surface);
    xdg_surface_add_listener(xdg, &surface_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xdg);
    xdg_toplevel_add_listener(top, &top_listener, NULL);
    xdg_toplevel_set_title(top, "voxl native capture benchmark");
    xdg_toplevel_set_fullscreen(top, NULL);
    wl_surface_commit(surface);
    while (!configured)
      if (wl_display_dispatch(display) < 0)
        die("configure");
    for (int i = 0; i < 2; i++) {
      int fd = memfd_create("voxl-loadgen", MFD_CLOEXEC);
      if (fd < 0 || ftruncate(fd, (off_t)W * H * 4))
        die("memfd");
      buffers[i].pixels = mmap(NULL, (size_t)W * H * 4, PROT_READ | PROT_WRITE,
                               MAP_SHARED, fd, 0);
      if (buffers[i].pixels == MAP_FAILED)
        die("mmap");
      struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, W * H * 4);
      buffers[i].wl = wl_shm_pool_create_buffer(pool, 0, W, H, W * 4,
                                                WL_SHM_FORMAT_XRGB8888);
      wl_buffer_add_listener(buffers[i].wl, &buffer_listener, &buffers[i]);
      wl_shm_pool_destroy(pool);
      close(fd);
    }
  } else {
    xd = XOpenDisplay(NULL);
    if (!xd)
      die("XOpenDisplay");
    int screen = DefaultScreen(xd);
    win = XCreateSimpleWindow(xd, RootWindow(xd, screen), 0, 0, W, H, 0, 0, 0);
    XSetWindowAttributes a = {.override_redirect = True};
    XChangeWindowAttributes(xd, win, CWOverrideRedirect, &a);
    XMapRaised(xd, win);
    XSync(xd, False);
    gc = XCreateGC(xd, win, 0, NULL);
    for (int i = 0; i < 2; i++) {
      struct buffer *b = &buffers[i];
      b->x = XShmCreateImage(xd, DefaultVisual(xd, screen), 24, ZPixmap, NULL,
                             &b->si, W, H);
      if (!b->x || b->x->bytes_per_line != W * 4)
        die("XShmCreateImage");
      b->si.shmid = shmget(IPC_PRIVATE, (size_t)W * H * 4, IPC_CREAT | 0600);
      if (b->si.shmid < 0)
        die("shmget");
      b->pixels =
          (void *)(b->si.shmaddr = b->x->data = shmat(b->si.shmid, NULL, 0));
      b->si.readOnly = False;
      if (b->si.shmaddr == (void *)-1)
        die("shmat");
      XShmAttach(xd, &b->si);
      XSync(xd, False);
      shmctl(b->si.shmid, IPC_RMID, NULL);
    }
  }
  int TH = H * 4;
  uint32_t *texture = malloc((size_t)W * TH * 4);
  if (!texture)
    die("malloc");
  for (int y = 0; y < TH; y++)
    for (int x = 0; x < W; x++) {
      int line = y / 24, iny = y % 24;
      uint32_t c = 0xfff5f5f5;
      if (iny > 4 && iny < 18 && x > 40 && x < 40 + (line * 7919) % (W - 200)) {
        int glyph = (x / 9 + line * 31) % 7;
        if (glyph && x % 9 < 7 && (x * 13 + y * 7 + line) % 5 < 3)
          c = 0xff202020 + (line % 3) * 0x200000;
      }
      if (line % 17 == 0)
        c = 0xff3070d0;
      texture[(size_t)y * W + x] = c;
    }
  for (int i = 0; i < 2; i++)
    memcpy(buffers[i].pixels, texture, (size_t)W * H * 4);
  signal(SIGUSR1, step);
  double start = now(), next = start;
  long f = 0;
  int last_step = -1;
  while (!closed && now() - start < secs) {
    if (wayland)
      pump();
    if ((idle && f) || (manual && last_step == steps)) {
      usleep(1000);
      continue;
    }
    struct buffer *b = NULL;
    for (int i = 0; i < 2; i++)
      if (!buffers[i].busy) {
        b = &buffers[i];
        break;
      }
    if (!b) {
      usleep(500);
      continue;
    }
    if (manual) {
      last_step = steps;
      f = steps;
    }
    if (scroll) {
      int offset = (int)(f * 6) % (TH - H);
      memcpy(b->pixels, texture + (size_t)offset * W, (size_t)W * H * 4);
    }
    if (manual) {
      rect(b->pixels, 600, 600, 64, 64,
           ((f + 1) / 2) % 2 ? 0xffff0000 : 0xff000000);
      rect(b->pixels, 1000, 1000, 64, 64,
           (f / 2) % 2 ? 0xff00ff00 : 0xff000000);
    }
    marker(b->pixels, f);
    int full = scroll || f == 0;
    if (wayland) {
      wl_surface_attach(surface, b->wl, 0, 0);
      wl_surface_damage_buffer(surface, 0, 0, full ? W : 480, full ? H : 48);
      if (manual && !full)
        wl_surface_damage_buffer(surface, f % 2 ? 600 : 1000,
                                 f % 2 ? 600 : 1000, 64, 64);
      b->busy = 1;
      wl_surface_commit(surface);
      wl_display_flush(display);
    } else {
      XShmPutImage(xd, win, gc, b->x, 0, 0, 0, 0, full ? W : 480, full ? H : 48,
                   False);
      if (manual && !full) {
        int p = f % 2 ? 600 : 1000;
        XShmPutImage(xd, win, gc, b->x, p, p, p, p, 64, 64, False);
      }
      XSync(xd, False);
    }
    if (!manual)
      f++;
    next += 1 / hz;
    double delay = next - now();
    if (delay > 0) {
      struct timespec ts = {(time_t)delay,
                            (long)((delay - (time_t)delay) * 1e9)};
      nanosleep(&ts, NULL);
    }
  }
  fprintf(stderr,
          "{\"backend\":\"%s\",\"mode\":\"%s\",\"frames\":%ld,\"seconds\":%.3f,"
          "\"label\":\"%s\"}\n",
          argv[1], argv[2], f, now() - start, argv[7]);
  return 0;
}
