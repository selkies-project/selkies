/*
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.
*/

/*
    Selkies V4L2 Interposer

    An LD_PRELOAD library that presents a virtual V4L2 capture device
    (/dev/videoN) backed by a Unix domain socket to the Selkies backend. It
    emulates the observable half of a fixed-function MJPEG webcam: the
    VIDIOC_* ioctl surface, MMAP streaming buffers, and poll readiness, so
    that unmodified consumers capture frames pushed from the browser without
    any kernel module or elevated privilege in the container.

    Design mirrors the sibling joystick interposer: the application-facing fd
    is the connected socket itself, so poll/select/epoll/dup/fork work with no
    interception. Frame delivery is pull-model at VIDIOC_DQBUF time, woken by a
    one-byte doorbell the backend sends per staged frame. Frame pixels never
    cross the socket; they live in a shared-memory staging ring the backend
    passes to each client once via SCM_RIGHTS. mmap() of the device fd is
    redirected onto a per-handle buffer memfd, so the application maps real
    shared memory and DQBUF copies one frame into it.

    Two additional surfaces cover consumers that bypass the plain libc calls.
    The libv4l2 wrapper library reaches the kernel through the libc syscall()
    entry point instead of open()/ioctl() to keep out of the way of libv4l's
    own LD_PRELOAD shims, so syscall() is interposed and routes only
    device-path opens and interposer-owned fds into the emulation. Camera
    discovery works by listing a directory rather than probing the device
    path, so scans of exactly /dev and /sys/class/video4linux get the device
    entry injected into their readdir() stream; every other directory stream
    passes through untouched.

    Duplicated fds (dup/dup2/dup3, fcntl F_DUPFD) are tracked as aliases of
    the originating handle so ioctls on any duplicate resolve to the same
    emulated device, and fcntl(F_SETFL) keeps the handle's O_NONBLOCK
    semantics in sync after open.
*/

#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE 1
#include <dirent.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdarg.h>
#include <fcntl.h>
#include <string.h>
#include <stdint.h>
#include <limits.h>
#include <stdlib.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <linux/ioctl.h>
#include <linux/version.h>
#include <linux/videodev2.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <pthread.h>

/* Only glibc has separate large-file entry points. musl's off_t is always
 * 64-bit and its headers alias the names, so defining the *64 variants there
 * would redefine the plain interposer. */
#if defined(_LARGEFILE64_SOURCE) && defined(__GLIBC__)
#define SWC_LFS64 1
#endif

/* Interposed libc entry points whose pathname is __nonnull can still be called
 * with NULL by a real caller; the guards forward it for the real EFAULT. */
#pragma GCC diagnostic ignored "-Wnonnull-compare"

#ifndef O_TMPFILE
#define __O_TMPFILE     020000000
#define O_TMPFILE       (__O_TMPFILE | O_DIRECTORY)
#endif
#define NEEDS_MODE(flags) (((flags) & O_CREAT) || (((flags) & O_TMPFILE) == O_TMPFILE))

#ifdef __GLIBC__
typedef unsigned long ioctl_request_t;
#else
typedef int ioctl_request_t;
#endif

#define SOCKET_CONNECT_TIMEOUT_MS 250
#define SOCKET_CONFIG_READ_TIMEOUT_MS 5000

/* Default virtual device and its backing socket. The device index is
 * overridable with SELKIES_WEBCAM_DEVICE; the socket directory with
 * SELKIES_WEBCAM_SOCKET_PATH (basename kept), matching the backend. */
#define WC_DEFAULT_DEVICE_PATH "/dev/video0"
#define WC_DEFAULT_SOCKET_PATH "/tmp/selkies_webcam0.sock"
#define WC_VIDEO_MAJOR 81

/* Shared-memory staging layout constants, identical in the Python backend.
 * Page 0 holds the header and the per-slot control blocks; frame bytes start
 * at WC_SHM_DATA_OFFSET. */
#define WC_SHM_MAGIC 0x434B5753u /* 'SKWC' */
#define WC_SHM_VERSION 1u
#define WC_SHM_CTRL_OFFSET 128u
#define WC_SHM_CTRL_STRIDE 64u
#define WC_SHM_DATA_OFFSET 4096u
#define WC_MAX_SLOTS 4u

/* Buffer bounds for the emulated MMAP queue. */
#define WC_MIN_BUFFERS 2u
#define WC_MAX_BUFFERS 8u
#define WC_MAX_HANDLES 16

/* --- Logging (WEBCAM_LOG enables stderr diagnostics) --- */
static int g_swc_log_enabled = 0;

#define SWC_LOG_DEBUG "[DEBUG]"
#define SWC_LOG_INFO  "[INFO]"
#define SWC_LOG_WARN  "[WARN]"
#define SWC_LOG_ERROR "[ERROR]"

static int (*real_open)(const char *pathname, int flags, ...) = NULL;
static int (*real_open64)(const char *pathname, int flags, ...) = NULL;
static int (*real_openat)(int dirfd, const char *pathname, int flags, ...) = NULL;
static int (*real_openat64)(int dirfd, const char *pathname, int flags, ...) = NULL;
static int (*real_ioctl)(int fd, ioctl_request_t request, ...) = NULL;
static int (*real_close)(int fd) = NULL;
static ssize_t (*real_read)(int fd, void *buf, size_t count) = NULL;
static ssize_t (*real_write)(int fd, const void *buf, size_t count) = NULL;
static int (*real_access)(const char *pathname, int mode) = NULL;
static int (*real_dup)(int oldfd) = NULL;
static int (*real_dup2)(int oldfd, int newfd) = NULL;
static int (*real_dup3)(int oldfd, int newfd, int flags) = NULL;
static int (*real_fcntl)(int fd, int cmd, ...) = NULL;
static long (*real_syscall)(long number, ...) = NULL;
static DIR *(*real_opendir)(const char *name) = NULL;
static struct dirent *(*real_readdir)(DIR *dirp) = NULL;
static int (*real_closedir)(DIR *dirp) = NULL;
static void (*real_rewinddir)(DIR *dirp) = NULL;
static int (*real_fstat)(int fd, struct stat *buf) = NULL;
static int (*real_stat)(const char *pathname, struct stat *buf) = NULL;
static int (*real_lstat)(const char *pathname, struct stat *buf) = NULL;
static void *(*real_mmap)(void *addr, size_t length, int prot, int flags, int fd, off_t offset) = NULL;
#ifdef SWC_LFS64
static struct dirent64 *(*real_readdir64)(DIR *dirp) = NULL;
static int (*real_stat64)(const char *pathname, struct stat64 *buf) = NULL;
static int (*real_lstat64)(const char *pathname, struct stat64 *buf) = NULL;
static int (*real_fstat64)(int fd, struct stat64 *buf) = NULL;
static void *(*real_mmap64)(void *addr, size_t length, int prot, int flags, int fd, off64_t offset) = NULL;
#endif
#ifdef __GLIBC__
/* Fortified open entry points: binaries built with _FORTIFY_SOURCE (the default
 * on Ubuntu and other hardened distros) lower a two-argument open() to these,
 * bypassing the open()/openat() wrappers above. They never carry a mode. */
static int (*real_fcntl64)(int fd, int cmd, ...) = NULL;
static int (*real___open_2)(const char *file, int oflag) = NULL;
static int (*real___open64_2)(const char *file, int oflag) = NULL;
static int (*real___openat_2)(int dirfd, const char *file, int oflag) = NULL;
static int (*real___openat64_2)(int dirfd, const char *file, int oflag) = NULL;
static int (*real___xstat)(int ver, const char *pathname, struct stat *buf) = NULL;
static int (*real___lxstat)(int ver, const char *pathname, struct stat *buf) = NULL;
static int (*real___fxstat)(int ver, int fd, struct stat *buf) = NULL;
static int (*real___xstat64)(int ver, const char *pathname, struct stat64 *buf) = NULL;
static int (*real___lxstat64)(int ver, const char *pathname, struct stat64 *buf) = NULL;
static int (*real___fxstat64)(int ver, int fd, struct stat64 *buf) = NULL;
#endif

static void swc_logging_init(void) {
    if (getenv("WEBCAM_LOG") != NULL) {
        g_swc_log_enabled = 1;
    }
}

static void interposer_log(const char *level, const char *func_name, int line_num, const char *format, ...) {
    if (!g_swc_log_enabled || real_write == NULL) {
        return;
    }
    char buffer[2048];
    size_t pos = 0;
    int printed = snprintf(buffer, sizeof(buffer), "[%lu][SWC]%s[%s:%d] ",
                           (unsigned long)time(NULL), level, func_name, line_num);
    if (printed > 0) {
        pos = ((size_t)printed < sizeof(buffer) - 1) ? (size_t)printed : sizeof(buffer) - 1;
    }
    if (pos < sizeof(buffer) - 1) {
        va_list argp;
        va_start(argp, format);
        printed = vsnprintf(buffer + pos, sizeof(buffer) - pos, format, argp);
        va_end(argp);
        if (printed > 0) {
            pos += ((size_t)printed < sizeof(buffer) - pos - 1) ? (size_t)printed : sizeof(buffer) - pos - 1;
        }
    }
    buffer[pos++] = '\n';
    (void)real_write(STDERR_FILENO, buffer, pos);
}

#define swc_log_debug(...) interposer_log(SWC_LOG_DEBUG, __func__, __LINE__, __VA_ARGS__)
#define swc_log_info(...)  interposer_log(SWC_LOG_INFO,  __func__, __LINE__, __VA_ARGS__)
#define swc_log_warn(...)  interposer_log(SWC_LOG_WARN,  __func__, __LINE__, __VA_ARGS__)
#define swc_log_error(...) interposer_log(SWC_LOG_ERROR, __func__, __LINE__, __VA_ARGS__)

static int load_real_func(void (**target)(void), const char *name) {
    if (*target != NULL) {
        return 0;
    }
    *target = dlsym(RTLD_NEXT, name);
    if (*target == NULL) {
        swc_log_error("Failed to load real '%s': %s.", name, dlerror());
        return -1;
    }
    return 0;
}

/* --- Wire configuration, identical layout in the Python backend --- */
/* Sent once by the backend on accept, ahead of the doorbell stream, with the
 * staging memfd attached as SCM_RIGHTS ancillary data. */
typedef struct {
    uint32_t magic;       /* WC_SHM_MAGIC */
    uint32_t version;     /* WC_SHM_VERSION */
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;      /* V4L2_PIX_FMT_MJPEG */
    uint32_t fps_num;
    uint32_t fps_den;
    uint32_t n_slots;
    uint32_t slot_size;   /* max bytes of one staged frame */
    uint32_t data_offset; /* WC_SHM_DATA_OFFSET */
    uint32_t ctrl_offset; /* WC_SHM_CTRL_OFFSET */
    uint32_t ctrl_stride; /* WC_SHM_CTRL_STRIDE */
    uint8_t  reserved[16];
} webcam_config_t;

/* Header at offset 0 of the staging memfd. Writer publishes latest_slot and
 * latest_frame_seq last; a zero latest_frame_seq means no frame yet. */
typedef struct {
    uint32_t magic;
    uint32_t version;
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t fps_num;
    uint32_t fps_den;
    uint32_t n_slots;
    uint32_t slot_size;
    uint32_t data_offset;
    uint32_t latest_slot;
    uint32_t _pad;
    uint64_t latest_frame_seq;
} wc_shm_header_t;

/* One per staging slot at ctrl_offset + i*ctrl_stride. seq is a seqlock:
 * odd while the writer is mid-update, even when the slot is consistent. */
typedef struct {
    uint32_t seq;
    uint32_t bytesused;
    uint64_t frame_seq;
    uint64_t ts_ns;
} wc_shm_ctrl_t;

typedef enum {
    WC_BUF_DEQUEUED = 0, /* owned by the application */
    WC_BUF_QUEUED = 1,   /* handed to us, awaiting a frame */
    WC_BUF_DONE = 2      /* filled, awaiting DQBUF (transient) */
} wc_buf_state_t;

/* One application open() handle: its own socket, staging mapping and buffers. */
typedef struct {
    int fd;                 /* connected socket, returned to the application */
    int open_flags;
    uint32_t priority;      /* VIDIOC_G/S_PRIORITY state */
    webcam_config_t cfg;

    void *staging_map;      /* read-only mapping of the staging memfd */
    size_t staging_size;

    int buf_fd;             /* memfd backing the MMAP buffers (-1 until REQBUFS) */
    void *buf_map;          /* our writable mapping of buf_fd, for the copy */
    size_t buf_stride;      /* per-buffer offset stride (page aligned) */
    uint32_t n_buffers;
    wc_buf_state_t buf_state[WC_MAX_BUFFERS];

    uint32_t queue_fifo[WC_MAX_BUFFERS]; /* queued buffer indices, FIFO */
    uint32_t queue_head;
    uint32_t queue_count;

    int streaming;
    uint32_t sequence;      /* frames delivered, v4l2_buffer.sequence */
    uint64_t last_frame_seq;/* last staging frame consumed by this handle */
} wc_handle_t;

static wc_handle_t handles[WC_MAX_HANDLES];
static int handle_count = 0;
static char g_device_path[256] = WC_DEFAULT_DEVICE_PATH;
static char g_socket_path[256] = WC_DEFAULT_SOCKET_PATH;

/* Duplicated fds referring to a handle's file description. Guarded by
 * handles_mutex like the handle table itself. */
#define WC_MAX_ALIASES 64
typedef struct {
    int fd;
    int primary_fd;
} wc_fd_alias_t;
static wc_fd_alias_t fd_aliases[WC_MAX_ALIASES];
static int alias_count = 0;

/* Guards the handles[] table lookups and state transitions. Never held across
 * the blocking connect/config-read on the open path nor the blocking doorbell
 * wait in DQBUF; those run on a private fd or after the entry is published. */
static pthread_mutex_t handles_mutex = PTHREAD_MUTEX_INITIALIZER;

static wc_handle_t *find_handle_for_fd_locked(int fd) {
    if (fd < 0) {
        return NULL;
    }
    for (int i = 0; i < handle_count; i++) {
        if (handles[i].fd == fd) {
            return &handles[i];
        }
    }
    for (int i = 0; i < alias_count; i++) {
        if (fd_aliases[i].fd == fd) {
            for (int j = 0; j < handle_count; j++) {
                if (handles[j].fd == fd_aliases[i].primary_fd) {
                    return &handles[j];
                }
            }
            return NULL;
        }
    }
    return NULL;
}

static int is_our_device_path(const char *pathname) {
    return pathname && strcmp(pathname, g_device_path) == 0;
}

static const char *device_basename(void) {
    const char *slash = strrchr(g_device_path, '/');
    return slash ? slash + 1 : g_device_path;
}

/* Static (local symbol) and uniquely named on purpose: the sibling joystick
 * interposer also has a constructor, and a shared global constructor name makes
 * the dynamic linker resolve one library's INIT_ARRAY entry to the other's
 * constructor, so this one would never run when both are preloaded together. */
__attribute__((constructor)) static void swc_init_interposer(void) {
    swc_logging_init();

    const char *dev_index = getenv("SELKIES_WEBCAM_DEVICE");
    if (dev_index && dev_index[0]) {
        long idx = strtol(dev_index, NULL, 10);
        if (idx >= 0 && idx < 256) {
            snprintf(g_device_path, sizeof(g_device_path), "/dev/video%ld", idx);
            snprintf(g_socket_path, sizeof(g_socket_path), "/tmp/selkies_webcam%ld.sock", idx);
        }
    }
    const char *sock_dir = getenv("SELKIES_WEBCAM_SOCKET_PATH");
    if (sock_dir && sock_dir[0]) {
        const char *slash = strrchr(g_socket_path, '/');
        const char *base = slash ? slash + 1 : g_socket_path;
        char newpath[sizeof(g_socket_path)];
        int n = snprintf(newpath, sizeof(newpath), "%s/%s", sock_dir, base);
        if (n > 0 && (size_t)n < sizeof(newpath)) {
            strncpy(g_socket_path, newpath, sizeof(g_socket_path) - 1);
            g_socket_path[sizeof(g_socket_path) - 1] = '\0';
        }
    }

    if (load_real_func((void *)&real_open, "open") < 0) swc_log_error("CRITICAL: no real 'open'.");
    if (load_real_func((void *)&real_ioctl, "ioctl") < 0) swc_log_error("CRITICAL: no real 'ioctl'.");
    if (load_real_func((void *)&real_close, "close") < 0) swc_log_error("CRITICAL: no real 'close'.");
    if (load_real_func((void *)&real_read, "read") < 0) swc_log_error("CRITICAL: no real 'read'.");
    if (load_real_func((void *)&real_write, "write") < 0) swc_log_error("CRITICAL: no real 'write'.");
    if (load_real_func((void *)&real_mmap, "mmap") < 0) swc_log_error("CRITICAL: no real 'mmap'.");
    if (load_real_func((void *)&real_access, "access") < 0) swc_log_error("CRITICAL: no real 'access'.");
    if (load_real_func((void *)&real_fstat, "fstat") < 0) swc_log_error("CRITICAL: no real 'fstat'.");
    if (load_real_func((void *)&real_stat, "stat") < 0) swc_log_error("CRITICAL: no real 'stat'.");
    if (load_real_func((void *)&real_lstat, "lstat") < 0) swc_log_error("CRITICAL: no real 'lstat'.");
    load_real_func((void *)&real_open64, "open64");
    load_real_func((void *)&real_openat, "openat");
    load_real_func((void *)&real_openat64, "openat64");
    load_real_func((void *)&real_dup, "dup");
    load_real_func((void *)&real_dup2, "dup2");
    load_real_func((void *)&real_dup3, "dup3");
    load_real_func((void *)&real_fcntl, "fcntl");
#ifdef __GLIBC__
    load_real_func((void *)&real_fcntl64, "fcntl64");
#endif
    load_real_func((void *)&real_syscall, "syscall");
    load_real_func((void *)&real_opendir, "opendir");
    load_real_func((void *)&real_readdir, "readdir");
    load_real_func((void *)&real_closedir, "closedir");
    load_real_func((void *)&real_rewinddir, "rewinddir");
#ifdef __GLIBC__
    load_real_func((void *)&real___open_2, "__open_2");
    load_real_func((void *)&real___open64_2, "__open64_2");
    load_real_func((void *)&real___openat_2, "__openat_2");
    load_real_func((void *)&real___openat64_2, "__openat64_2");
#endif
#ifdef SWC_LFS64
    load_real_func((void *)&real_readdir64, "readdir64");
    load_real_func((void *)&real_mmap64, "mmap64");
    load_real_func((void *)&real_stat64, "stat64");
    load_real_func((void *)&real_lstat64, "lstat64");
    load_real_func((void *)&real_fstat64, "fstat64");
#endif
    swc_log_info("Selkies V4L2 Interposer initialized for %s -> %s. Logging %s.",
                 g_device_path, g_socket_path, g_swc_log_enabled ? "ENABLED" : "DISABLED");
}

/* --- Forged device metadata so directory scans and stat() see a char device --- */
#define FILL_FAKE_STAT_FIELDS(buf) do {                 \
    (buf)->st_mode = S_IFCHR | 0666;                    \
    (buf)->st_rdev = makedev(WC_VIDEO_MAJOR, 0);        \
    (buf)->st_uid = 0;                                  \
    (buf)->st_gid = 0;                                  \
    (buf)->st_size = 0;                                 \
    (buf)->st_blksize = 4096;                           \
    (buf)->st_blocks = 0;                               \
    (buf)->st_nlink = 1;                                \
} while (0)

int access(const char *pathname, int mode) {
    if (!real_access && load_real_func((void *)&real_access, "access") < 0) {
        errno = EFAULT;
        return -1;
    }
    if (is_our_device_path(pathname)) {
        errno = 0;
        return 0;
    }
    return real_access(pathname, mode);
}

int stat(const char *pathname, struct stat *buf) {
    if (!real_stat && load_real_func((void *)&real_stat, "stat") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) {
        memset(buf, 0, sizeof(*buf));
        FILL_FAKE_STAT_FIELDS(buf);
        return 0;
    }
    return real_stat(pathname, buf);
}

int lstat(const char *pathname, struct stat *buf) {
    if (!real_lstat && load_real_func((void *)&real_lstat, "lstat") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) {
        memset(buf, 0, sizeof(*buf));
        FILL_FAKE_STAT_FIELDS(buf);
        return 0;
    }
    return real_lstat(pathname, buf);
}

int fstat(int fd, struct stat *buf) {
    if (!real_fstat && load_real_func((void *)&real_fstat, "fstat") < 0) { errno = EFAULT; return -1; }
    pthread_mutex_lock(&handles_mutex);
    int ours = find_handle_for_fd_locked(fd) != NULL;
    pthread_mutex_unlock(&handles_mutex);
    if (ours) {
        memset(buf, 0, sizeof(*buf));
        FILL_FAKE_STAT_FIELDS(buf);
        return 0;
    }
    return real_fstat(fd, buf);
}

#ifdef SWC_LFS64
int stat64(const char *pathname, struct stat64 *buf) {
    if (!real_stat64 && load_real_func((void *)&real_stat64, "stat64") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real_stat64(pathname, buf);
}
int lstat64(const char *pathname, struct stat64 *buf) {
    if (!real_lstat64 && load_real_func((void *)&real_lstat64, "lstat64") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real_lstat64(pathname, buf);
}
int fstat64(int fd, struct stat64 *buf) {
    if (!real_fstat64 && load_real_func((void *)&real_fstat64, "fstat64") < 0) { errno = EFAULT; return -1; }
    pthread_mutex_lock(&handles_mutex);
    int ours = find_handle_for_fd_locked(fd) != NULL;
    pthread_mutex_unlock(&handles_mutex);
    if (ours) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real_fstat64(fd, buf);
}
#endif

#ifdef __GLIBC__
int __xstat(int ver, const char *pathname, struct stat *buf) {
    if (!real___xstat && load_real_func((void *)&real___xstat, "__xstat") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___xstat(ver, pathname, buf);
}
int __lxstat(int ver, const char *pathname, struct stat *buf) {
    if (!real___lxstat && load_real_func((void *)&real___lxstat, "__lxstat") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___lxstat(ver, pathname, buf);
}
int __fxstat(int ver, int fd, struct stat *buf) {
    if (!real___fxstat && load_real_func((void *)&real___fxstat, "__fxstat") < 0) { errno = EFAULT; return -1; }
    pthread_mutex_lock(&handles_mutex);
    int ours = find_handle_for_fd_locked(fd) != NULL;
    pthread_mutex_unlock(&handles_mutex);
    if (ours) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___fxstat(ver, fd, buf);
}
int __xstat64(int ver, const char *pathname, struct stat64 *buf) {
    if (!real___xstat64 && load_real_func((void *)&real___xstat64, "__xstat64") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___xstat64(ver, pathname, buf);
}
int __lxstat64(int ver, const char *pathname, struct stat64 *buf) {
    if (!real___lxstat64 && load_real_func((void *)&real___lxstat64, "__lxstat64") < 0) { errno = EFAULT; return -1; }
    if (is_our_device_path(pathname)) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___lxstat64(ver, pathname, buf);
}
int __fxstat64(int ver, int fd, struct stat64 *buf) {
    if (!real___fxstat64 && load_real_func((void *)&real___fxstat64, "__fxstat64") < 0) { errno = EFAULT; return -1; }
    pthread_mutex_lock(&handles_mutex);
    int ours = find_handle_for_fd_locked(fd) != NULL;
    pthread_mutex_unlock(&handles_mutex);
    if (ours) { memset(buf, 0, sizeof(*buf)); FILL_FAKE_STAT_FIELDS(buf); return 0; }
    return real___fxstat64(ver, fd, buf);
}
#endif

/* --- Device enumeration: directory listing injection --- */
/* Consumers discover cameras by listing a directory, not by probing the
 * device path: some scan /dev for videoN nodes, others scan
 * /sys/class/video4linux for class entries. Streams opened on exactly those
 * two directories are tracked, and the device entry is
 * appended once at end-of-listing unless a real entry of the same name already
 * appeared (e.g. a placeholder node or a real camera). When the sysfs class
 * directory does not exist at all, the stream is backed by an existing
 * directory purely to obtain a valid DIR handle and only synthetic entries are
 * emitted. Every other directory stream passes through untouched, gated by a
 * tracked-stream counter so the common path stays lock-free. */
#define WC_MAX_TRACKED_DIRS 8

typedef struct {
    DIR *dir;
    int fake_only;          /* placeholder-backed: emit synthetic entries only */
    unsigned char d_type;   /* DT_CHR for /dev, DT_LNK for sysfs */
    int synth_state;
    int injected;
    int suppress;           /* real listing already carries the device name */
    struct dirent ent;
#ifdef SWC_LFS64
    struct dirent64 ent64;
#endif
} wc_dir_t;

static wc_dir_t tracked_dirs[WC_MAX_TRACKED_DIRS];
static int tracked_dir_count = 0;
static pthread_mutex_t dirs_mutex = PTHREAD_MUTEX_INITIALIZER;

static int dir_path_wants_device(const char *name, unsigned char *d_type) {
    if (name == NULL) {
        return 0;
    }
    size_t len = strlen(name);
    while (len > 1 && name[len - 1] == '/') {
        len--;
    }
    if (len == 4 && strncmp(name, "/dev", 4) == 0) {
        *d_type = DT_CHR;
        return 1;
    }
    if (len == 22 && strncmp(name, "/sys/class/video4linux", 22) == 0) {
        *d_type = DT_LNK;
        return 1;
    }
    return 0;
}

static wc_dir_t *find_tracked_dir_locked(DIR *dirp) {
    for (int i = 0; i < WC_MAX_TRACKED_DIRS; i++) {
        if (tracked_dirs[i].dir == dirp) {
            return &tracked_dirs[i];
        }
    }
    return NULL;
}

DIR *opendir(const char *name) {
    if (!real_opendir && load_real_func((void *)&real_opendir, "opendir") < 0) {
        errno = ENOENT;
        return NULL;
    }
    unsigned char d_type = 0;
    if (!dir_path_wants_device(name, &d_type)) {
        return real_opendir(name);
    }
    DIR *dir = real_opendir(name);
    int fake_only = 0;
    if (dir == NULL) {
        /* /dev always exists; only the sysfs class dir is worth fabricating. */
        if (d_type != DT_LNK) {
            return NULL;
        }
        int saved_errno = errno;
        static const char *placeholders[] = { "/sys/class", "/sys", "/" };
        for (size_t i = 0; i < sizeof(placeholders) / sizeof(placeholders[0]) && dir == NULL; i++) {
            dir = real_opendir(placeholders[i]);
        }
        if (dir == NULL) {
            errno = saved_errno;
            return NULL;
        }
        fake_only = 1;
    }
    pthread_mutex_lock(&dirs_mutex);
    wc_dir_t *slot = NULL;
    for (int i = 0; i < WC_MAX_TRACKED_DIRS; i++) {
        if (tracked_dirs[i].dir == NULL) {
            slot = &tracked_dirs[i];
            break;
        }
    }
    if (slot == NULL) {
        pthread_mutex_unlock(&dirs_mutex);
        if (fake_only) {
            if (real_closedir || load_real_func((void *)&real_closedir, "closedir") == 0) {
                real_closedir(dir);
            }
            errno = ENOENT;
            return NULL;
        }
        return dir;
    }
    memset(slot, 0, sizeof(*slot));
    slot->dir = dir;
    slot->fake_only = fake_only;
    slot->d_type = d_type;
    __atomic_add_fetch(&tracked_dir_count, 1, __ATOMIC_RELEASE);
    pthread_mutex_unlock(&dirs_mutex);
    return dir;
}

/* The synthetic listing is ".", "..", then the device; the wrapped listing is
 * the real entries with the device appended once at end-of-stream. The entry
 * struct lives in the tracked slot, matching readdir()'s per-stream lifetime. */
#define WC_FILL_DIR_ENTRY(entp, nm, tp, ino) do {                       \
    memset((entp), 0, sizeof(*(entp)));                                 \
    snprintf((entp)->d_name, sizeof((entp)->d_name), "%s", (nm));       \
    (entp)->d_type = (tp);                                              \
    (entp)->d_ino = (ino);                                              \
    (entp)->d_reclen = (unsigned short)sizeof(*(entp));                 \
} while (0)

struct dirent *readdir(DIR *dirp) {
    if (!real_readdir && load_real_func((void *)&real_readdir, "readdir") < 0) {
        errno = EBADF;
        return NULL;
    }
    if (__atomic_load_n(&tracked_dir_count, __ATOMIC_ACQUIRE) == 0) {
        return real_readdir(dirp);
    }
    pthread_mutex_lock(&dirs_mutex);
    wc_dir_t *t = find_tracked_dir_locked(dirp);
    if (t == NULL) {
        pthread_mutex_unlock(&dirs_mutex);
        return real_readdir(dirp);
    }
    if (t->fake_only) {
        struct dirent *ret = NULL;
        if (t->synth_state < 3) {
            const char *nm = (t->synth_state == 0) ? "." :
                             (t->synth_state == 1) ? ".." : device_basename();
            unsigned char tp = (t->synth_state < 2) ? DT_DIR : t->d_type;
            WC_FILL_DIR_ENTRY(&t->ent, nm, tp, (ino_t)(1 + t->synth_state));
            t->synth_state++;
            ret = &t->ent;
        }
        pthread_mutex_unlock(&dirs_mutex);
        return ret;
    }
    pthread_mutex_unlock(&dirs_mutex);
    struct dirent *e = real_readdir(dirp);
    pthread_mutex_lock(&dirs_mutex);
    t = find_tracked_dir_locked(dirp);
    if (t == NULL) {
        pthread_mutex_unlock(&dirs_mutex);
        return e;
    }
    struct dirent *ret = e;
    if (e != NULL) {
        if (strcmp(e->d_name, device_basename()) == 0) {
            t->suppress = 1;
        }
    } else if (!t->suppress && !t->injected) {
        t->injected = 1;
        WC_FILL_DIR_ENTRY(&t->ent, device_basename(), t->d_type, (ino_t)1);
        ret = &t->ent;
    }
    pthread_mutex_unlock(&dirs_mutex);
    return ret;
}

#ifdef SWC_LFS64
struct dirent64 *readdir64(DIR *dirp) {
    if (!real_readdir64 && load_real_func((void *)&real_readdir64, "readdir64") < 0) {
        errno = EBADF;
        return NULL;
    }
    if (__atomic_load_n(&tracked_dir_count, __ATOMIC_ACQUIRE) == 0) {
        return real_readdir64(dirp);
    }
    pthread_mutex_lock(&dirs_mutex);
    wc_dir_t *t = find_tracked_dir_locked(dirp);
    if (t == NULL) {
        pthread_mutex_unlock(&dirs_mutex);
        return real_readdir64(dirp);
    }
    if (t->fake_only) {
        struct dirent64 *ret = NULL;
        if (t->synth_state < 3) {
            const char *nm = (t->synth_state == 0) ? "." :
                             (t->synth_state == 1) ? ".." : device_basename();
            unsigned char tp = (t->synth_state < 2) ? DT_DIR : t->d_type;
            WC_FILL_DIR_ENTRY(&t->ent64, nm, tp, (ino64_t)(1 + t->synth_state));
            t->synth_state++;
            ret = &t->ent64;
        }
        pthread_mutex_unlock(&dirs_mutex);
        return ret;
    }
    pthread_mutex_unlock(&dirs_mutex);
    struct dirent64 *e = real_readdir64(dirp);
    pthread_mutex_lock(&dirs_mutex);
    t = find_tracked_dir_locked(dirp);
    if (t == NULL) {
        pthread_mutex_unlock(&dirs_mutex);
        return e;
    }
    struct dirent64 *ret = e;
    if (e != NULL) {
        if (strcmp(e->d_name, device_basename()) == 0) {
            t->suppress = 1;
        }
    } else if (!t->suppress && !t->injected) {
        t->injected = 1;
        WC_FILL_DIR_ENTRY(&t->ent64, device_basename(), t->d_type, (ino64_t)1);
        ret = &t->ent64;
    }
    pthread_mutex_unlock(&dirs_mutex);
    return ret;
}
#endif

void rewinddir(DIR *dirp) {
    if (!real_rewinddir && load_real_func((void *)&real_rewinddir, "rewinddir") < 0) {
        return;
    }
    if (__atomic_load_n(&tracked_dir_count, __ATOMIC_ACQUIRE) != 0) {
        pthread_mutex_lock(&dirs_mutex);
        wc_dir_t *t = find_tracked_dir_locked(dirp);
        if (t != NULL) {
            t->synth_state = 0;
            t->injected = 0;
            t->suppress = 0;
            if (t->fake_only) {
                pthread_mutex_unlock(&dirs_mutex);
                return;
            }
        }
        pthread_mutex_unlock(&dirs_mutex);
    }
    real_rewinddir(dirp);
}

int closedir(DIR *dirp) {
    if (!real_closedir && load_real_func((void *)&real_closedir, "closedir") < 0) {
        errno = EBADF;
        return -1;
    }
    if (__atomic_load_n(&tracked_dir_count, __ATOMIC_ACQUIRE) != 0) {
        pthread_mutex_lock(&dirs_mutex);
        wc_dir_t *t = find_tracked_dir_locked(dirp);
        if (t != NULL) {
            t->dir = NULL;
            __atomic_sub_fetch(&tracked_dir_count, 1, __ATOMIC_RELEASE);
        }
        pthread_mutex_unlock(&dirs_mutex);
    }
    return real_closedir(dirp);
}

/* --- Socket connection and config handshake --- */

/* Receives the config struct plus the staging memfd (SCM_RIGHTS) in one
 * message. Runs on the private open() fd, so it may block briefly without the
 * global lock held. Returns the received memfd (>=0) on success, -1 on error. */
static int recv_config_and_fd(int sockfd, webcam_config_t *cfg) {
    struct iovec iov = { .iov_base = cfg, .iov_len = sizeof(*cfg) };
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } cmsgu;
    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsgu.buf;
    msg.msg_controllen = sizeof(cmsgu.buf);

    struct timeval rcv_timeout = { .tv_sec = 5, .tv_usec = 0 };
    setsockopt(sockfd, SOL_SOCKET, SO_RCVTIMEO, &rcv_timeout, sizeof(rcv_timeout));

    ssize_t n = recvmsg(sockfd, &msg, 0);
    if (n != (ssize_t)sizeof(*cfg)) {
        swc_log_error("config recvmsg returned %zd (want %zu): %s", n, sizeof(*cfg),
                      n < 0 ? strerror(errno) : "short read");
        return -1;
    }
    if (cfg->magic != WC_SHM_MAGIC || cfg->version != WC_SHM_VERSION) {
        swc_log_error("config magic/version mismatch: 0x%08x v%u", cfg->magic, cfg->version);
        return -1;
    }
    if (cfg->n_slots == 0 || cfg->n_slots > WC_MAX_SLOTS || cfg->slot_size == 0) {
        swc_log_error("config slots/size out of range: n_slots=%u slot_size=%u", cfg->n_slots, cfg->slot_size);
        return -1;
    }

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    if (!cmsg || cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS ||
        cmsg->cmsg_len != CMSG_LEN(sizeof(int))) {
        swc_log_error("config message carried no staging fd");
        return -1;
    }
    int staging_fd;
    memcpy(&staging_fd, CMSG_DATA(cmsg), sizeof(int));
    return staging_fd;
}

/* Connect, receive config + staging fd, map the staging memfd read-only, and
 * emit the one-byte arch specifier (sizeof(long)) as a handshake ack. On
 * success returns the socket fd and fills *out; the staging fd is consumed.
 * O_CLOEXEC from the application's open() carries onto the socket so exec'd
 * children can't hold a dead connection open and keep the backend feeding. */
static int connect_and_configure(wc_handle_t *out, int open_flags) {
    int sock_type = SOCK_STREAM | ((open_flags & O_CLOEXEC) ? SOCK_CLOEXEC : 0);
    int sockfd = socket(AF_UNIX, sock_type, 0);
    if (sockfd == -1) {
        swc_log_error("socket() failed: %s", strerror(errno));
        return -1;
    }
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, g_socket_path, sizeof(addr.sun_path) - 1);

    long slept_us = 0;
    const long timeout_us = SOCKET_CONNECT_TIMEOUT_MS * 1000;
    while (connect(sockfd, (struct sockaddr *)&addr, sizeof(addr)) == -1) {
        if ((errno == ENOENT || errno == ECONNREFUSED) && slept_us < timeout_us) {
            usleep(10000);
            slept_us += 10000;
            continue;
        }
        swc_log_error("connect(%s) failed: %s", g_socket_path, strerror(errno));
        real_close(sockfd);
        return -1;
    }

    webcam_config_t cfg;
    memset(&cfg, 0, sizeof(cfg));
    int staging_fd = recv_config_and_fd(sockfd, &cfg);
    if (staging_fd < 0) {
        real_close(sockfd);
        return -1;
    }

    size_t staging_size = (size_t)cfg.data_offset + (size_t)cfg.n_slots * (size_t)cfg.slot_size;
    void *map = real_mmap(NULL, staging_size, PROT_READ, MAP_SHARED, staging_fd, 0);
    real_close(staging_fd);
    if (map == MAP_FAILED) {
        swc_log_error("mmap staging (%zu bytes) failed: %s", staging_size, strerror(errno));
        real_close(sockfd);
        return -1;
    }

    unsigned char arch_byte = (unsigned char)sizeof(long);
    if (real_write(sockfd, &arch_byte, 1) != 1) {
        swc_log_warn("failed to send arch ack: %s", strerror(errno));
    }

    memset(out, 0, sizeof(*out));
    out->fd = sockfd;
    out->cfg = cfg;
    out->staging_map = map;
    out->staging_size = staging_size;
    out->buf_fd = -1;
    out->buf_map = NULL;
    out->priority = V4L2_PRIORITY_DEFAULT;
    swc_log_info("configured %ux%u @ %u/%u fps, %u slots x %u bytes",
                 cfg.width, cfg.height, cfg.fps_num, cfg.fps_den, cfg.n_slots, cfg.slot_size);
    return sockfd;
}

/* -2 not our device (caller falls back to real open); -1 error (errno set);
 * >=0 the socket fd handed to the application. */
static int common_open_logic(const char *pathname, int flags) {
    if (pathname == NULL || !is_our_device_path(pathname)) {
        return -2;
    }
    wc_handle_t pending;
    int new_fd = connect_and_configure(&pending, flags);
    if (new_fd == -1) {
        errno = EIO;
        return -1;
    }
    pending.open_flags = flags;

    pthread_mutex_lock(&handles_mutex);
    if (handle_count >= WC_MAX_HANDLES) {
        pthread_mutex_unlock(&handles_mutex);
        munmap(pending.staging_map, pending.staging_size);
        real_close(new_fd);
        errno = EMFILE;
        return -1;
    }
    handles[handle_count] = pending;
    handle_count++;
    pthread_mutex_unlock(&handles_mutex);
    swc_log_info("opened %s -> fd %d (%d handle(s))", pathname, new_fd, handle_count);
    return new_fd;
}

int open(const char *pathname, int flags, ...) {
    if (!real_open) { errno = EFAULT; return -1; }
    int fd = common_open_logic(pathname, flags);
    if (fd != -2) {
        return fd;
    }
    if (NEEDS_MODE(flags)) {
        va_list args; va_start(args, flags);
        mode_t mode = va_arg(args, mode_t); va_end(args);
        return real_open(pathname, flags, mode);
    }
    return real_open(pathname, flags);
}

#ifdef open64
#undef open64
#endif
int open64(const char *pathname, int flags, ...) {
    if (!real_open64 && !real_open) { errno = EFAULT; return -1; }
    int fd = common_open_logic(pathname, flags);
    if (fd != -2) {
        return fd;
    }
    if (NEEDS_MODE(flags)) {
        va_list args; va_start(args, flags);
        mode_t mode = va_arg(args, mode_t); va_end(args);
        return real_open64 ? real_open64(pathname, flags, mode) : real_open(pathname, flags, mode);
    }
    return real_open64 ? real_open64(pathname, flags) : real_open(pathname, flags);
}

/* Resolve a possibly-relative openat() path against dirfd for the match. */
static const char *resolve_at(int dirfd, const char *pathname, char *full, size_t full_sz) {
    if (pathname && pathname[0] != '/' && dirfd != AT_FDCWD) {
        char procfd[64];
        snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", dirfd);
        ssize_t len = readlink(procfd, full, full_sz - 1);
        if (len > 0 && (size_t)len < full_sz - 1) {
            int w = snprintf(full + len, full_sz - (size_t)len, "/%s", pathname);
            if (w > 0 && (size_t)w < full_sz - (size_t)len) {
                return full;
            }
        }
    }
    return pathname;
}

int openat(int dirfd, const char *pathname, int flags, ...) {
    if (!real_openat) { errno = EFAULT; return -1; }
    char full[4096];
    const char *check = resolve_at(dirfd, pathname, full, sizeof(full));
    int fd = common_open_logic(check, flags);
    if (fd != -2) {
        return fd;
    }
    if (NEEDS_MODE(flags)) {
        va_list args; va_start(args, flags);
        mode_t mode = va_arg(args, mode_t); va_end(args);
        return real_openat(dirfd, pathname, flags, mode);
    }
    return real_openat(dirfd, pathname, flags);
}

#ifdef openat64
#undef openat64
#endif
int openat64(int dirfd, const char *pathname, int flags, ...) {
    if (!real_openat64 && !real_openat) { errno = EFAULT; return -1; }
    char full[4096];
    const char *check = resolve_at(dirfd, pathname, full, sizeof(full));
    int fd = common_open_logic(check, flags);
    if (fd != -2) {
        return fd;
    }
    if (NEEDS_MODE(flags)) {
        va_list args; va_start(args, flags);
        mode_t mode = va_arg(args, mode_t); va_end(args);
        return real_openat64 ? real_openat64(dirfd, pathname, flags, mode) : real_openat(dirfd, pathname, flags, mode);
    }
    return real_openat64 ? real_openat64(dirfd, pathname, flags) : real_openat(dirfd, pathname, flags);
}

#ifdef __GLIBC__
int __open_2(const char *file, int oflag) {
    if (!real___open_2 && load_real_func((void *)&real___open_2, "__open_2") < 0) { errno = EFAULT; return -1; }
    int fd = common_open_logic(file, oflag);
    return fd != -2 ? fd : real___open_2(file, oflag);
}

int __open64_2(const char *file, int oflag) {
    if (!real___open64_2 && load_real_func((void *)&real___open64_2, "__open64_2") < 0) { errno = EFAULT; return -1; }
    int fd = common_open_logic(file, oflag);
    return fd != -2 ? fd : real___open64_2(file, oflag);
}

int __openat_2(int dirfd, const char *file, int oflag) {
    if (!real___openat_2 && load_real_func((void *)&real___openat_2, "__openat_2") < 0) { errno = EFAULT; return -1; }
    char full[4096];
    const char *check = resolve_at(dirfd, file, full, sizeof(full));
    int fd = common_open_logic(check, oflag);
    return fd != -2 ? fd : real___openat_2(dirfd, file, oflag);
}

int __openat64_2(int dirfd, const char *file, int oflag) {
    if (!real___openat64_2 && load_real_func((void *)&real___openat64_2, "__openat64_2") < 0) { errno = EFAULT; return -1; }
    char full[4096];
    const char *check = resolve_at(dirfd, file, full, sizeof(full));
    int fd = common_open_logic(check, oflag);
    return fd != -2 ? fd : real___openat64_2(dirfd, file, oflag);
}
#endif

/* --- Buffer memfd allocation --- */
static size_t round_up_page(size_t n) {
    long pg = sysconf(_SC_PAGESIZE);
    size_t page = (pg > 0) ? (size_t)pg : 4096;
    return (n + page - 1) & ~(page - 1);
}

static void release_buffers_locked(wc_handle_t *h) {
    if (h->buf_map) {
        munmap(h->buf_map, (size_t)h->n_buffers * h->buf_stride);
        h->buf_map = NULL;
    }
    if (h->buf_fd >= 0) {
        real_close(h->buf_fd);
        h->buf_fd = -1;
    }
    h->n_buffers = 0;
    h->buf_stride = 0;
    h->queue_head = 0;
    h->queue_count = 0;
    h->streaming = 0;
}

static int allocate_buffers_locked(wc_handle_t *h, uint32_t count) {
    release_buffers_locked(h);
    size_t stride = round_up_page(h->cfg.slot_size);
    int mfd = (int)syscall(SYS_memfd_create, "selkies-webcam-buffers", MFD_CLOEXEC);
    if (mfd < 0) {
        swc_log_error("memfd_create failed: %s", strerror(errno));
        return -1;
    }
    size_t total = (size_t)count * stride;
    if (ftruncate(mfd, (off_t)total) != 0) {
        swc_log_error("ftruncate(%zu) failed: %s", total, strerror(errno));
        real_close(mfd);
        return -1;
    }
    void *map = real_mmap(NULL, total, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if (map == MAP_FAILED) {
        swc_log_error("mmap buffers failed: %s", strerror(errno));
        real_close(mfd);
        return -1;
    }
    h->buf_fd = mfd;
    h->buf_map = map;
    h->buf_stride = stride;
    h->n_buffers = count;
    for (uint32_t i = 0; i < count; i++) {
        h->buf_state[i] = WC_BUF_DEQUEUED;
    }
    return 0;
}

/* --- Staging frame read (seqlock) --- */
/* Copies the newest staged frame into dest if it is newer than h->last_frame_seq.
 * Returns 1 on a fresh frame copied (updates bytesused, ts_ns, last_frame_seq),
 * 0 if no new frame, -1 on inconsistency after retries. */
static int read_latest_frame(wc_handle_t *h, void *dest, uint32_t dest_cap,
                             uint32_t *bytesused, uint64_t *ts_ns) {
    volatile wc_shm_header_t *hdr = (volatile wc_shm_header_t *)h->staging_map;
    uint64_t fseq = __atomic_load_n(&hdr->latest_frame_seq, __ATOMIC_ACQUIRE);
    if (fseq == 0 || fseq == h->last_frame_seq) {
        return 0;
    }
    uint32_t slot = __atomic_load_n(&hdr->latest_slot, __ATOMIC_ACQUIRE);
    if (slot >= h->cfg.n_slots) {
        return -1;
    }
    volatile wc_shm_ctrl_t *ctrl = (volatile wc_shm_ctrl_t *)
        ((char *)h->staging_map + h->cfg.ctrl_offset + (size_t)slot * h->cfg.ctrl_stride);
    const char *slot_data = (const char *)h->staging_map + h->cfg.data_offset + (size_t)slot * h->cfg.slot_size;

    for (int attempt = 0; attempt < 8; attempt++) {
        uint32_t s1 = __atomic_load_n(&ctrl->seq, __ATOMIC_ACQUIRE);
        if (s1 & 1u) {
            usleep(200);
            continue;
        }
        uint32_t used = ctrl->bytesused;
        uint64_t cts = ctrl->ts_ns;
        uint64_t cfseq = ctrl->frame_seq;
        if (used > h->cfg.slot_size) {
            used = h->cfg.slot_size;
        }
        if (used > dest_cap) {
            used = dest_cap;
        }
        memcpy(dest, slot_data, used);
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        uint32_t s2 = __atomic_load_n(&ctrl->seq, __ATOMIC_ACQUIRE);
        if (s1 == s2 && !(s2 & 1u)) {
            *bytesused = used;
            *ts_ns = cts;
            h->last_frame_seq = cfseq;
            return 1;
        }
    }
    swc_log_warn("staging read torn after retries (slot churn)");
    return -1;
}

/* Drain any pending doorbell bytes without blocking. */
static void drain_doorbell(int fd) {
    unsigned char tmp[64];
    while (recv(fd, tmp, sizeof(tmp), MSG_DONTWAIT) > 0) {
        /* discard */
    }
}

/* --- ioctl emulation --- */

static void fill_pix_format(wc_handle_t *h, struct v4l2_pix_format *pix) {
    memset(pix, 0, sizeof(*pix));
    pix->width = h->cfg.width;
    pix->height = h->cfg.height;
    pix->pixelformat = h->cfg.fourcc;
    pix->field = V4L2_FIELD_NONE;
    pix->bytesperline = 0; /* compressed */
    pix->sizeimage = h->cfg.slot_size;
    pix->colorspace = V4L2_COLORSPACE_SRGB;
}

/* Self-locking: the blocking doorbell wait must not hold handles_mutex, or a
 * concurrent STREAMOFF/close on another thread (the usual way to interrupt a
 * blocked DQBUF) would deadlock. State is inspected and mutated under the lock;
 * the lock is dropped for the recv() wait and the handle re-looked-up after. */
static int handle_dqbuf(int fd, struct v4l2_buffer *b) {
    for (;;) {
        pthread_mutex_lock(&handles_mutex);
        wc_handle_t *h = find_handle_for_fd_locked(fd);
        if (h == NULL) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EBADF;
            return -1;
        }
        if (!h->streaming || b->memory != V4L2_MEMORY_MMAP) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EINVAL;
            return -1;
        }
        if (h->queue_count == 0) {
            /* No buffer to fill: real drivers return EINVAL for capture. */
            pthread_mutex_unlock(&handles_mutex);
            errno = EINVAL;
            return -1;
        }
        /* Doorbells are only wakeups; the shm sequence is the source of truth.
         * Draining stale wakeups here keeps poll() from spinning when frames
         * were coalesced or already consumed. */
        drain_doorbell(h->fd);
        uint32_t idx = h->queue_fifo[h->queue_head];
        void *dest = (char *)h->buf_map + (size_t)idx * h->buf_stride;
        uint32_t bytesused = 0;
        uint64_t ts_ns = 0;
        int r = read_latest_frame(h, dest, (uint32_t)h->buf_stride, &bytesused, &ts_ns);
        if (r == 1) {
            h->queue_head = (h->queue_head + 1) % h->n_buffers;
            h->queue_count--;
            h->buf_state[idx] = WC_BUF_DEQUEUED;
            uint32_t stride = (uint32_t)h->buf_stride;
            uint32_t seq = h->sequence++;
            pthread_mutex_unlock(&handles_mutex);

            memset(b, 0, sizeof(*b));
            b->index = idx;
            b->type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            b->memory = V4L2_MEMORY_MMAP;
            b->bytesused = bytesused;
            b->length = stride;
            b->flags = V4L2_BUF_FLAG_MAPPED | V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC;
            b->field = V4L2_FIELD_NONE;
            b->sequence = seq;
            b->m.offset = idx * stride;
            b->timestamp.tv_sec = (time_t)(ts_ns / 1000000000ULL);
            b->timestamp.tv_usec = (suseconds_t)((ts_ns % 1000000000ULL) / 1000ULL);
            return 0;
        }
        if (r < 0) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EIO;
            return -1;
        }
        /* No fresh frame yet. */
        int nonblock = h->open_flags & O_NONBLOCK;
        int sockfd = h->fd;
        pthread_mutex_unlock(&handles_mutex);
        if (nonblock) {
            errno = EAGAIN;
            return -1;
        }
        unsigned char tmp[64];
        ssize_t got = recv(sockfd, tmp, sizeof(tmp), 0);
        if (got <= 0) {
            if (got < 0 && errno == EINTR) {
                continue;
            }
            if (got == 0) {
                errno = ENODEV;
            }
            return -1;
        }
    }
}

/* Runs with handles_mutex held. Returns the ioctl result; sets errno on -1. */
static int intercept_ioctl_locked(wc_handle_t *h, ioctl_request_t request, void *arg) {
    if (_IOC_TYPE(request) != 'V') {
        errno = ENOTTY;
        return -1;
    }
    switch (_IOC_NR(request)) {
    case _IOC_NR(VIDIOC_QUERYCAP): {
        struct v4l2_capability *cap = arg;
        if (!cap) { errno = EFAULT; return -1; }
        memset(cap, 0, sizeof(*cap));
        strncpy((char *)cap->driver, "selkies", sizeof(cap->driver) - 1);
        strncpy((char *)cap->card, "Selkies Virtual Camera", sizeof(cap->card) - 1);
        strncpy((char *)cap->bus_info, "platform:selkies-webcam", sizeof(cap->bus_info) - 1);
        cap->version = KERNEL_VERSION(6, 1, 0);
        cap->device_caps = V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING | V4L2_CAP_READWRITE;
        cap->capabilities = cap->device_caps | V4L2_CAP_DEVICE_CAPS;
        return 0;
    }
    case _IOC_NR(VIDIOC_ENUM_FMT): {
        struct v4l2_fmtdesc *f = arg;
        if (!f) { errno = EFAULT; return -1; }
        if (f->type != V4L2_BUF_TYPE_VIDEO_CAPTURE || f->index != 0) { errno = EINVAL; return -1; }
        f->flags = V4L2_FMT_FLAG_COMPRESSED;
        strncpy((char *)f->description, "Motion-JPEG", sizeof(f->description) - 1);
        f->pixelformat = h->cfg.fourcc;
        return 0;
    }
    case _IOC_NR(VIDIOC_G_FMT):
    case _IOC_NR(VIDIOC_S_FMT):
    case _IOC_NR(VIDIOC_TRY_FMT): {
        struct v4l2_format *f = arg;
        if (!f) { errno = EFAULT; return -1; }
        if (f->type != V4L2_BUF_TYPE_VIDEO_CAPTURE) { errno = EINVAL; return -1; }
        fill_pix_format(h, &f->fmt.pix);
        return 0;
    }
    case _IOC_NR(VIDIOC_ENUM_FRAMESIZES): {
        struct v4l2_frmsizeenum *fs = arg;
        if (!fs) { errno = EFAULT; return -1; }
        if (fs->index != 0 || fs->pixel_format != h->cfg.fourcc) { errno = EINVAL; return -1; }
        fs->type = V4L2_FRMSIZE_TYPE_DISCRETE;
        fs->discrete.width = h->cfg.width;
        fs->discrete.height = h->cfg.height;
        return 0;
    }
    case _IOC_NR(VIDIOC_ENUM_FRAMEINTERVALS): {
        struct v4l2_frmivalenum *fi = arg;
        if (!fi) { errno = EFAULT; return -1; }
        if (fi->index != 0 || fi->pixel_format != h->cfg.fourcc ||
            fi->width != h->cfg.width || fi->height != h->cfg.height) { errno = EINVAL; return -1; }
        fi->type = V4L2_FRMIVAL_TYPE_DISCRETE;
        fi->discrete.numerator = h->cfg.fps_den;   /* interval = 1/fps */
        fi->discrete.denominator = h->cfg.fps_num;
        return 0;
    }
    case _IOC_NR(VIDIOC_REQBUFS): {
        struct v4l2_requestbuffers *rb = arg;
        if (!rb) { errno = EFAULT; return -1; }
        if (rb->type != V4L2_BUF_TYPE_VIDEO_CAPTURE || rb->memory != V4L2_MEMORY_MMAP) {
            errno = EINVAL; return -1;
        }
        if (rb->count == 0) {
            release_buffers_locked(h);
            rb->count = 0;
        } else {
            uint32_t count = rb->count;
            if (count < WC_MIN_BUFFERS) count = WC_MIN_BUFFERS;
            if (count > WC_MAX_BUFFERS) count = WC_MAX_BUFFERS;
            if (allocate_buffers_locked(h, count) != 0) { errno = ENOMEM; return -1; }
            rb->count = count;
        }
#ifdef V4L2_BUF_CAP_SUPPORTS_MMAP
        rb->capabilities = V4L2_BUF_CAP_SUPPORTS_MMAP;
#endif
        return 0;
    }
    case _IOC_NR(VIDIOC_QUERYBUF): {
        struct v4l2_buffer *b = arg;
        if (!b) { errno = EFAULT; return -1; }
        if (b->type != V4L2_BUF_TYPE_VIDEO_CAPTURE || b->index >= h->n_buffers) { errno = EINVAL; return -1; }
        uint32_t idx = b->index;
        memset(b, 0, sizeof(*b));
        b->index = idx;
        b->type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        b->memory = V4L2_MEMORY_MMAP;
        b->length = (uint32_t)h->buf_stride;
        b->m.offset = idx * (uint32_t)h->buf_stride;
        b->flags = 0;
        if (h->buf_state[idx] == WC_BUF_QUEUED) b->flags |= V4L2_BUF_FLAG_QUEUED;
        b->field = V4L2_FIELD_NONE;
        return 0;
    }
    case _IOC_NR(VIDIOC_QBUF): {
        struct v4l2_buffer *b = arg;
        if (!b) { errno = EFAULT; return -1; }
        if (b->type != V4L2_BUF_TYPE_VIDEO_CAPTURE || b->memory != V4L2_MEMORY_MMAP ||
            b->index >= h->n_buffers) { errno = EINVAL; return -1; }
        if (h->buf_state[b->index] == WC_BUF_QUEUED) { errno = EINVAL; return -1; }
        h->buf_state[b->index] = WC_BUF_QUEUED;
        h->queue_fifo[(h->queue_head + h->queue_count) % h->n_buffers] = b->index;
        h->queue_count++;
        b->flags |= V4L2_BUF_FLAG_QUEUED;
        return 0;
    }
    case _IOC_NR(VIDIOC_STREAMON): {
        int *type = arg;
        if (!type || *type != V4L2_BUF_TYPE_VIDEO_CAPTURE) { errno = EINVAL; return -1; }
        if (h->buf_fd < 0) { errno = EINVAL; return -1; }
        h->streaming = 1;
        /* Start from the current live frame, not a backlog. */
        volatile wc_shm_header_t *hdr = (volatile wc_shm_header_t *)h->staging_map;
        h->last_frame_seq = __atomic_load_n(&hdr->latest_frame_seq, __ATOMIC_ACQUIRE);
        drain_doorbell(h->fd);
        return 0;
    }
    case _IOC_NR(VIDIOC_STREAMOFF): {
        int *type = arg;
        if (!type || *type != V4L2_BUF_TYPE_VIDEO_CAPTURE) { errno = EINVAL; return -1; }
        h->streaming = 0;
        h->queue_head = 0;
        h->queue_count = 0;
        for (uint32_t i = 0; i < h->n_buffers; i++) {
            h->buf_state[i] = WC_BUF_DEQUEUED;
        }
        return 0;
    }
    case _IOC_NR(VIDIOC_G_PARM):
    case _IOC_NR(VIDIOC_S_PARM): {
        struct v4l2_streamparm *p = arg;
        if (!p) { errno = EFAULT; return -1; }
        if (p->type != V4L2_BUF_TYPE_VIDEO_CAPTURE) { errno = EINVAL; return -1; }
        memset(&p->parm.capture, 0, sizeof(p->parm.capture));
        p->parm.capture.capability = V4L2_CAP_TIMEPERFRAME;
        p->parm.capture.timeperframe.numerator = h->cfg.fps_den;
        p->parm.capture.timeperframe.denominator = h->cfg.fps_num;
        p->parm.capture.readbuffers = WC_MIN_BUFFERS;
        return 0;
    }
    case _IOC_NR(VIDIOC_ENUMINPUT): {
        struct v4l2_input *in = arg;
        if (!in) { errno = EFAULT; return -1; }
        if (in->index != 0) { errno = EINVAL; return -1; }
        uint32_t idx = in->index;
        memset(in, 0, sizeof(*in));
        in->index = idx;
        strncpy((char *)in->name, "Camera", sizeof(in->name) - 1);
        in->type = V4L2_INPUT_TYPE_CAMERA;
        return 0;
    }
    case _IOC_NR(VIDIOC_G_INPUT): {
        int *i = arg;
        if (!i) { errno = EFAULT; return -1; }
        *i = 0;
        return 0;
    }
    case _IOC_NR(VIDIOC_S_INPUT): {
        int *i = arg;
        if (!i) { errno = EFAULT; return -1; }
        if (*i != 0) { errno = EINVAL; return -1; }
        return 0;
    }
    case _IOC_NR(VIDIOC_G_PRIORITY): {
        uint32_t *p = arg;
        if (!p) { errno = EFAULT; return -1; }
        *p = h->priority;
        return 0;
    }
    case _IOC_NR(VIDIOC_S_PRIORITY): {
        uint32_t *p = arg;
        if (!p) { errno = EFAULT; return -1; }
        if (*p > V4L2_PRIORITY_RECORD) { errno = EINVAL; return -1; }
        h->priority = *p;
        return 0;
    }
    /* The kernel implements the control ioctls for every video device and
     * reports each unsupported control id as EINVAL, never ENOTTY; control
     * enumeration loops (V4L2_CTRL_FLAG_NEXT_CTRL) terminate on EINVAL. */
    case _IOC_NR(VIDIOC_QUERYCTRL):
    case _IOC_NR(VIDIOC_QUERYMENU):
    case _IOC_NR(VIDIOC_G_CTRL):
    case _IOC_NR(VIDIOC_S_CTRL):
#ifdef VIDIOC_QUERY_EXT_CTRL
    case _IOC_NR(VIDIOC_QUERY_EXT_CTRL):
#endif
#ifdef VIDIOC_G_EXT_CTRLS
    case _IOC_NR(VIDIOC_G_EXT_CTRLS):
    case _IOC_NR(VIDIOC_S_EXT_CTRLS):
    case _IOC_NR(VIDIOC_TRY_EXT_CTRLS):
#endif
        errno = EINVAL;
        return -1;
    default:
        /* Events, cropping, output, standards: a fixed camera advertises
         * none, and ENOTTY here matches minimal real webcams. */
        errno = ENOTTY;
        return -1;
    }
}

/* Shared by the libc ioctl() wrapper and the syscall() route. Sets *handled
 * when fd is an interposer handle; otherwise the caller forwards to its own
 * real entry point. DQBUF may block waiting for a frame; it manages the lock
 * itself so the wait never stalls a concurrent STREAMOFF/close. */
static int wc_ioctl_common(int fd, ioctl_request_t request, void *arg, int *handled) {
    pthread_mutex_lock(&handles_mutex);
    wc_handle_t *h = find_handle_for_fd_locked(fd);
    if (h == NULL) {
        pthread_mutex_unlock(&handles_mutex);
        *handled = 0;
        return -1;
    }
    *handled = 1;
    if (_IOC_TYPE(request) == 'V' && _IOC_NR(request) == _IOC_NR(VIDIOC_DQBUF)) {
        pthread_mutex_unlock(&handles_mutex);
        return handle_dqbuf(fd, arg);
    }
    errno = 0;
    int ret = intercept_ioctl_locked(h, request, arg);
    int saved_errno = errno;
    pthread_mutex_unlock(&handles_mutex);
    errno = saved_errno;
    return ret;
}

int ioctl(int fd, ioctl_request_t request, ...) {
    va_list args;
    va_start(args, request);
    void *arg = va_arg(args, void *);
    va_end(args);

    if (!real_ioctl && load_real_func((void *)&real_ioctl, "ioctl") < 0) {
        errno = EFAULT;
        return -1;
    }
    int handled = 0;
    int ret = wc_ioctl_common(fd, request, arg, &handled);
    return handled ? ret : real_ioctl(fd, request, arg);
}

/* mmap of the device fd is redirected onto that handle's buffer memfd, so the
 * application maps the real shared buffer at the QUERYBUF offset. */
static int wc_buf_fd_for_fd(int fd) {
    pthread_mutex_lock(&handles_mutex);
    wc_handle_t *h = find_handle_for_fd_locked(fd);
    int buf_fd = (h != NULL && h->buf_fd >= 0) ? h->buf_fd : -1;
    pthread_mutex_unlock(&handles_mutex);
    return buf_fd;
}

void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset) {
    if (!real_mmap && load_real_func((void *)&real_mmap, "mmap") < 0) {
        errno = ENOMEM;
        return MAP_FAILED;
    }
    int buf_fd = wc_buf_fd_for_fd(fd);
    if (buf_fd >= 0) {
        return real_mmap(addr, length, prot, flags, buf_fd, offset);
    }
    return real_mmap(addr, length, prot, flags, fd, offset);
}

#ifdef SWC_LFS64
void *mmap64(void *addr, size_t length, int prot, int flags, int fd, off64_t offset) {
    if (!real_mmap64 && load_real_func((void *)&real_mmap64, "mmap64") < 0) {
        errno = ENOMEM;
        return MAP_FAILED;
    }
    int buf_fd = wc_buf_fd_for_fd(fd);
    if (buf_fd >= 0) {
        return real_mmap64(addr, length, prot, flags, buf_fd, offset);
    }
    return real_mmap64(addr, length, prot, flags, fd, offset);
}
#endif

/* read() I/O mode: deliver one JPEG frame per call (implicit streaming), for
 * consumers that skip MMAP. A handle used in MMAP mode never calls read().
 * Shared by the libc read() wrapper and the syscall() route; sets *handled
 * when fd is an interposer handle. */
static ssize_t wc_read_common(int fd, void *buf, size_t count, int *handled) {
    pthread_mutex_lock(&handles_mutex);
    wc_handle_t *h = find_handle_for_fd_locked(fd);
    if (h == NULL) {
        pthread_mutex_unlock(&handles_mutex);
        *handled = 0;
        return -1;
    }
    *handled = 1;
    if (!h->streaming) {
        volatile wc_shm_header_t *hdr = (volatile wc_shm_header_t *)h->staging_map;
        h->last_frame_seq = __atomic_load_n(&hdr->latest_frame_seq, __ATOMIC_ACQUIRE);
        h->streaming = 1;
    }
    int nonblock = h->open_flags & O_NONBLOCK;
    int sockfd = h->fd;
    for (;;) {
        uint32_t bytesused = 0;
        uint64_t ts_ns = 0;
        int r = read_latest_frame(h, buf, (uint32_t)count, &bytesused, &ts_ns);
        if (r == 1) {
            pthread_mutex_unlock(&handles_mutex);
            return (ssize_t)bytesused;
        }
        if (r < 0) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EIO;
            return -1;
        }
        if (nonblock) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EAGAIN;
            return -1;
        }
        pthread_mutex_unlock(&handles_mutex);
        unsigned char tmp[64];
        ssize_t got = recv(sockfd, tmp, sizeof(tmp), 0);
        if (got <= 0 && !(got < 0 && errno == EINTR)) {
            return (got == 0) ? 0 : -1;
        }
        pthread_mutex_lock(&handles_mutex);
        h = find_handle_for_fd_locked(fd);
        if (h == NULL) {
            pthread_mutex_unlock(&handles_mutex);
            errno = EBADF;
            return -1;
        }
    }
}

ssize_t read(int fd, void *buf, size_t count) {
    if (!real_read && load_real_func((void *)&real_read, "read") < 0) {
        errno = EFAULT;
        return -1;
    }
    int handled = 0;
    ssize_t ret = wc_read_common(fd, buf, count, &handled);
    return handled ? ret : real_read(fd, buf, count);
}

/* Retires the fd's tracking before the caller issues its own real close, so
 * a reused fd number can't alias a stale handle. A dup alias only drops its
 * entry; a primary fd with surviving duplicates promotes one of them, since
 * the file description (and the emulated device state) is still open. The
 * handle's resources are released only with the last referencing fd.
 * Returns 1 when fd was tracked. */
static int wc_close_untrack(int fd) {
    pthread_mutex_lock(&handles_mutex);
    for (int i = 0; i < alias_count; i++) {
        if (fd_aliases[i].fd == fd) {
            fd_aliases[i] = fd_aliases[alias_count - 1];
            alias_count--;
            pthread_mutex_unlock(&handles_mutex);
            return 1;
        }
    }
    int found = -1;
    for (int i = 0; i < handle_count; i++) {
        if (handles[i].fd == fd) {
            found = i;
            break;
        }
    }
    if (found >= 0) {
        int promoted = 0;
        for (int i = 0; i < alias_count; i++) {
            if (fd_aliases[i].primary_fd == fd) {
                int new_primary = fd_aliases[i].fd;
                handles[found].fd = new_primary;
                fd_aliases[i] = fd_aliases[alias_count - 1];
                alias_count--;
                for (int j = 0; j < alias_count; j++) {
                    if (fd_aliases[j].primary_fd == fd) {
                        fd_aliases[j].primary_fd = new_primary;
                    }
                }
                promoted = 1;
                break;
            }
        }
        if (!promoted) {
            wc_handle_t *h = &handles[found];
            release_buffers_locked(h);
            if (h->staging_map) {
                munmap(h->staging_map, h->staging_size);
            }
            handles[found] = handles[handle_count - 1];
            handle_count--;
        }
    }
    pthread_mutex_unlock(&handles_mutex);
    return found >= 0;
}

int close(int fd) {
    if (!real_close) {
        errno = EFAULT;
        return -1;
    }
    wc_close_untrack(fd);
    return real_close(fd);
}

/* Registers newfd as an alias of oldfd's handle after a successful
 * duplication. A full alias table only degrades the duplicate to poll-only
 * (same socket description), so the drop is logged rather than failed. */
static void wc_register_dup(int oldfd, int newfd) {
    if (handle_count == 0) {
        return;
    }
    pthread_mutex_lock(&handles_mutex);
    wc_handle_t *h = find_handle_for_fd_locked(oldfd);
    if (h != NULL) {
        if (alias_count < WC_MAX_ALIASES) {
            fd_aliases[alias_count].fd = newfd;
            fd_aliases[alias_count].primary_fd = h->fd;
            alias_count++;
            swc_log_info("dup fd %d -> %d aliases handle fd %d", oldfd, newfd, h->fd);
        } else {
            swc_log_warn("alias table full; fd %d not tracked", newfd);
        }
    }
    pthread_mutex_unlock(&handles_mutex);
}

int dup(int oldfd) {
    if (!real_dup && load_real_func((void *)&real_dup, "dup") < 0) {
        errno = EBADF;
        return -1;
    }
    int newfd = real_dup(oldfd);
    if (newfd >= 0) {
        wc_register_dup(oldfd, newfd);
    }
    return newfd;
}

int dup2(int oldfd, int newfd) {
    if (!real_dup2 && load_real_func((void *)&real_dup2, "dup2") < 0) {
        errno = EBADF;
        return -1;
    }
    int ret = real_dup2(oldfd, newfd);
    if (ret >= 0 && oldfd != newfd) {
        wc_close_untrack(newfd);
        wc_register_dup(oldfd, newfd);
    }
    return ret;
}

int dup3(int oldfd, int newfd, int flags) {
    if (!real_dup3 && load_real_func((void *)&real_dup3, "dup3") < 0) {
        errno = EBADF;
        return -1;
    }
    int ret = real_dup3(oldfd, newfd, flags);
    if (ret >= 0) {
        wc_close_untrack(newfd);
        wc_register_dup(oldfd, newfd);
    }
    return ret;
}

/* Bookkeeping on top of the real call: F_DUPFD variants create an alias like
 * dup(), and F_SETFL refreshes the stored open flags so a later toggle of
 * O_NONBLOCK changes DQBUF/read blocking behavior like it would on a real
 * device. The argument is a single machine word for every fcntl command, so
 * forwarding one long is exact. */
static int wc_fcntl_bookkeep(int fd, int cmd, long arg, int ret) {
    if (ret >= 0) {
        if (cmd == F_DUPFD
#ifdef F_DUPFD_CLOEXEC
            || cmd == F_DUPFD_CLOEXEC
#endif
        ) {
            wc_register_dup(fd, ret);
        } else if (cmd == F_SETFL && handle_count > 0) {
            pthread_mutex_lock(&handles_mutex);
            wc_handle_t *h = find_handle_for_fd_locked(fd);
            if (h != NULL) {
                h->open_flags = (int)arg;
            }
            pthread_mutex_unlock(&handles_mutex);
        }
    }
    return ret;
}

int fcntl(int fd, int cmd, ...) {
    va_list ap;
    va_start(ap, cmd);
    long arg = va_arg(ap, long);
    va_end(ap);
    if (!real_fcntl && load_real_func((void *)&real_fcntl, "fcntl") < 0) {
        errno = EBADF;
        return -1;
    }
    return wc_fcntl_bookkeep(fd, cmd, arg, real_fcntl(fd, cmd, arg));
}

#ifdef __GLIBC__
int fcntl64(int fd, int cmd, ...) {
    va_list ap;
    va_start(ap, cmd);
    long arg = va_arg(ap, long);
    va_end(ap);
    if (!real_fcntl64 && load_real_func((void *)&real_fcntl64, "fcntl64") < 0) {
        if (!real_fcntl && load_real_func((void *)&real_fcntl, "fcntl") < 0) {
            errno = EBADF;
            return -1;
        }
        return wc_fcntl_bookkeep(fd, cmd, arg, real_fcntl(fd, cmd, arg));
    }
    return wc_fcntl_bookkeep(fd, cmd, arg, real_fcntl64(fd, cmd, arg));
}
#endif

/* The libv4l2 wrapper library reaches the kernel through the libc syscall()
 * entry point rather than open()/ioctl(), so that libv4l's own
 * v4l1compat/v4l2convert LD_PRELOAD shims never recurse into it; that same
 * design detail routes it straight past every wrapper above. Interposing
 * syscall() catches exactly the device-path opens and operations on
 * interposer-owned fds; everything else is forwarded to the real syscall()
 * with the full six-argument window, which is safe for any Linux syscall.
 * SYS_mmap is only claimed where SYS_mmap2 is absent (64-bit ABIs): on 32-bit
 * ABIs SYS_mmap is the legacy one-struct-argument form and must pass through. */
long syscall(long number, ...) {
    long a[6];
    va_list ap;
    va_start(ap, number);
    for (int i = 0; i < 6; i++) {
        a[i] = va_arg(ap, long);
    }
    va_end(ap);

    if (!real_syscall && load_real_func((void *)&real_syscall, "syscall") < 0) {
        errno = ENOSYS;
        return -1;
    }

    switch (number) {
#ifdef SYS_open
    case SYS_open:
        if (is_our_device_path((const char *)a[0])) {
            int fd = common_open_logic((const char *)a[0], (int)a[1]);
            if (fd != -2) {
                return fd;
            }
        }
        break;
#endif
#ifdef SYS_openat
    case SYS_openat: {
        char full[4096];
        const char *check = resolve_at((int)a[0], (const char *)a[1], full, sizeof(full));
        if (is_our_device_path(check)) {
            int fd = common_open_logic(check, (int)a[2]);
            if (fd != -2) {
                return fd;
            }
        }
        break;
    }
#endif
    case SYS_ioctl: {
        int handled = 0;
        int ret = wc_ioctl_common((int)a[0], (ioctl_request_t)a[1], (void *)a[2], &handled);
        if (handled) {
            return ret;
        }
        break;
    }
    case SYS_read: {
        int handled = 0;
        ssize_t ret = wc_read_common((int)a[0], (void *)a[1], (size_t)a[2], &handled);
        if (handled) {
            return (long)ret;
        }
        break;
    }
    case SYS_close:
        wc_close_untrack((int)a[0]);
        break;
#if defined(SYS_mmap) && !defined(SYS_mmap2)
    case SYS_mmap: {
        int buf_fd = wc_buf_fd_for_fd((int)a[4]);
        if (buf_fd >= 0) {
            if (!real_mmap && load_real_func((void *)&real_mmap, "mmap") < 0) {
                errno = ENOMEM;
                return -1;
            }
            void *map = real_mmap((void *)a[0], (size_t)a[1], (int)a[2], (int)a[3], buf_fd, (off_t)a[5]);
            return map == MAP_FAILED ? -1 : (long)map;
        }
        break;
    }
#endif
#ifdef SYS_mmap2
    case SYS_mmap2: {
        int buf_fd = wc_buf_fd_for_fd((int)a[4]);
        if (buf_fd >= 0) {
            if (!real_mmap && load_real_func((void *)&real_mmap, "mmap") < 0) {
                errno = ENOMEM;
                return -1;
            }
            void *map = real_mmap((void *)a[0], (size_t)a[1], (int)a[2], (int)a[3], buf_fd, (off_t)a[5] * 4096);
            return map == MAP_FAILED ? -1 : (long)map;
        }
        break;
    }
#endif
    default:
        break;
    }
    return real_syscall(number, a[0], a[1], a[2], a[3], a[4], a[5]);
}
