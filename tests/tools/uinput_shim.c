/* Test-only emulator for /dev/uinput: records the device setup ioctls and lets
   the event writes land in a file, so the real server code path can be run on a
   host whose /dev/uinput is unavailable. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <linux/uinput.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define MAXFD 4096
static char uinput_fd[MAXFD];
static FILE *logf;
static int (*real_open)(const char *, int, ...);
static int (*real_open64)(const char *, int, ...);
static int (*real_openat)(int, const char *, int, ...);
static int (*real_ioctl)(int, unsigned long, ...);
static int (*real_close)(int);

static void init(void) {
    if (!real_open) real_open = dlsym(RTLD_NEXT, "open");
    if (!real_open64) real_open64 = dlsym(RTLD_NEXT, "open64");
    if (!real_openat) real_openat = dlsym(RTLD_NEXT, "openat");
    if (!real_ioctl) real_ioctl = dlsym(RTLD_NEXT, "ioctl");
    if (!real_close) real_close = dlsym(RTLD_NEXT, "close");
    if (!logf) {
        const char *p = getenv("UINPUT_SHIM_LOG");
        logf = fopen(p ? p : "/tmp/uinput_shim.log", "a");
        if (logf) setvbuf(logf, NULL, _IOLBF, 0);
    }
}

/* Wrapped so the NULL test is not folded away: open()'s prototype declares the
   path nonnull, which makes the compiler assume the check cannot fail. */
static int is_uinput(const char *path) {
    return path != NULL && !strcmp(path, "/dev/uinput");
}

static int fake_open(void) {
    init();
    const char *stream = getenv("UINPUT_SHIM_STREAM");
    int fd = real_open64(stream ? stream : "/tmp/uinput_stream.bin",
                         O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0 && fd < MAXFD) {
        uinput_fd[fd] = 1;
        if (logf) fprintf(logf, "OPEN fd=%d\n", fd);
    }
    return fd;
}

int open(const char *path, int flags, ...) {
    init();
    if (is_uinput(path)) return fake_open();
    mode_t mode = 0; va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    return real_open(path, flags, mode);
}

int open64(const char *path, int flags, ...) {
    init();
    if (is_uinput(path)) return fake_open();
    mode_t mode = 0; va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    return real_open64(path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...) {
    init();
    if (is_uinput(path)) return fake_open();
    mode_t mode = 0; va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
    return real_openat(dirfd, path, flags, mode);
}

int close(int fd) {
    init();
    if (fd >= 0 && fd < MAXFD && uinput_fd[fd]) {
        uinput_fd[fd] = 0;
        if (logf) fprintf(logf, "CLOSE fd=%d\n", fd);
    }
    return real_close(fd);
}

int ioctl(int fd, unsigned long request, ...) {
    init();
    va_list ap; va_start(ap, request);
    void *arg = va_arg(ap, void *);
    va_end(ap);
    if (fd < 0 || fd >= MAXFD || !uinput_fd[fd]) return real_ioctl(fd, request, arg);
    if (!logf) return 0;
    if (request == UI_SET_EVBIT) fprintf(logf, "SET_EVBIT 0x%02lx\n", (unsigned long)arg);
    else if (request == UI_SET_KEYBIT) fprintf(logf, "SET_KEYBIT 0x%03lx\n", (unsigned long)arg);
    else if (request == UI_SET_ABSBIT) fprintf(logf, "SET_ABSBIT 0x%02lx\n", (unsigned long)arg);
    else if (request == UI_ABS_SETUP) {
        struct uinput_abs_setup *s = arg;
        fprintf(logf, "ABS_SETUP code=0x%02x value=%d min=%d max=%d fuzz=%d flat=%d res=%d\n",
                s->code, s->absinfo.value, s->absinfo.minimum, s->absinfo.maximum,
                s->absinfo.fuzz, s->absinfo.flat, s->absinfo.resolution);
    } else if (request == UI_DEV_SETUP) {
        struct uinput_setup *s = arg;
        fprintf(logf, "DEV_SETUP bus=0x%02x vendor=0x%04x product=0x%04x version=0x%04x ff=%u name=%s\n",
                s->id.bustype, s->id.vendor, s->id.product, s->id.version,
                s->ff_effects_max, s->name);
    } else if (request == UI_DEV_CREATE) fprintf(logf, "DEV_CREATE\n");
    else if (request == UI_DEV_DESTROY) fprintf(logf, "DEV_DESTROY\n");
    else if (_IOC_NR(request) == _IOC_NR(UI_GET_SYSNAME(0))) {
        const char *sysname = getenv("UINPUT_SHIM_SYSNAME");
        snprintf(arg, _IOC_SIZE(request), "%s", sysname ? sysname : "input999");
        fprintf(logf, "GET_SYSNAME -> %s\n", (char *)arg);
    } else fprintf(logf, "IOCTL other request=0x%lx\n", request);
    return 0;
}
