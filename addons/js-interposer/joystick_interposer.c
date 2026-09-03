/*
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.
*/

/*
    Selkies Joystick Interposer

    An LD_PRELOAD library that redirects /dev/input/jsN and /dev/input/eventN
    access onto Unix domain sockets served by the Selkies backend, so gamepad
    input forwarded from the browser reaches unmodified applications without a
    kernel device. Four js nodes and four evdev nodes (event1000..event1003,
    numbered clear of real devices) map to "selkies_<sysname>.sock" in the
    socket directory (SELKIES_JS_SOCKET_PATH, default /tmp), which must match
    the backend's js_socket_path.

    Every open() of a device gets its own socket connection, so each handle is
    a distinct fd as POSIX requires, O_NONBLOCK applies per handle, and every
    handle receives every event (the server broadcasts per device). The fd
    handed to the application is the connected socket itself, so poll/select/
    epoll/dup work with no interception. On connect the server sends one
    js_config_t (identity, button and axis maps), then the client answers with
    a one-byte architecture specifier (sizeof(long)) and the server streams
    struct js_event or struct input_event records. read() only ever delivers
    whole events: a short consume would desync the SOCK_STREAM for every later
    read, so partially drained events are stashed per handle and completed on
    the next call.

    Device identity (name, VID/PID, uniq) answered through the ioctls is hard
    coded to the same values the sibling fake-udev library publishes, so udev,
    joydev and evdev consumers agree on one device. stat()/fstat() families
    forge a character device with major 13 and the node's index as minor,
    which SDL uses to dedupe devices, and /dev/input listings (readdir/scandir)
    gain the evdev nodes whose sockets are currently bound, for scanners that
    bypass libudev. JS_LOG in the environment enables stderr diagnostics.
*/

#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE 1
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
#include <arpa/inet.h>
#include <sys/un.h>
#include <sys/ioctl.h>
#include <linux/ioctl.h>
#include <sys/epoll.h>
#include <poll.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <dirent.h>
#include <sys/inotify.h>
#include <linux/joystick.h>
#include <linux/input.h>
#include <linux/input-event-codes.h>
#include <pthread.h>

/* Only glibc has separate large-file entry points. musl's off_t is always 64-bit
 * and its headers alias the names (`#define stat64 stat`), so defining stat64()
 * there would redefine the plain stat() interposer. */
#if defined(_LARGEFILE64_SOURCE) && defined(__GLIBC__)
#define SJI_LFS64 1
#endif

/* We interpose libc entry points whose `pathname` is __nonnull, but a real caller
 * can pass NULL and our `if (pathname)` guards forward it for the real EFAULT.
 * The guards are intentional, so silence the false-positive -Wnonnull-compare. */
#pragma GCC diagnostic ignored "-Wnonnull-compare"

/* O_TMPFILE carries a mode argument like O_CREAT; NEEDS_MODE tells the open
 * wrappers when to pull it from the varargs. */
#ifndef O_TMPFILE
#define __O_TMPFILE     020000000
#define O_TMPFILE       (__O_TMPFILE | O_DIRECTORY)
#endif
#define NEEDS_MODE(flags) (((flags) & O_CREAT) || (((flags) & O_TMPFILE) == O_TMPFILE))

/* glibc declares ioctl's request as unsigned long, musl as int. */
#ifdef __GLIBC__
typedef unsigned long ioctl_request_t;
#else
typedef int ioctl_request_t;
#endif

#define SOCKET_CONNECT_TIMEOUT_MS 250
/* Bounds the config wait inside the intercepted open(), so a connected-but-silent
 * peer cannot hang the opening thread. */
#define SOCKET_CONFIG_READ_TIMEOUT_MS 5000

#define JS0_DEVICE_PATH "/dev/input/js0"
#define JS0_SOCKET_PATH "/tmp/selkies_js0.sock"
#define JS1_DEVICE_PATH "/dev/input/js1"
#define JS1_SOCKET_PATH "/tmp/selkies_js1.sock"
#define JS2_DEVICE_PATH "/dev/input/js2"
#define JS2_SOCKET_PATH "/tmp/selkies_js2.sock"
#define JS3_DEVICE_PATH "/dev/input/js3"
#define JS3_SOCKET_PATH "/tmp/selkies_js3.sock"
#define NUM_JS_INTERPOSERS 4

#define EV0_DEVICE_PATH "/dev/input/event1000"
#define EV0_SOCKET_PATH "/tmp/selkies_event1000.sock"
#define EV1_DEVICE_PATH "/dev/input/event1001"
#define EV1_SOCKET_PATH "/tmp/selkies_event1001.sock"
#define EV2_DEVICE_PATH "/dev/input/event1002"
#define EV2_SOCKET_PATH "/tmp/selkies_event1002.sock"
#define EV3_DEVICE_PATH "/dev/input/event1003"
#define EV3_SOCKET_PATH "/tmp/selkies_event1003.sock"
#define NUM_EV_INTERPOSERS 4

#define NUM_INTERPOSERS() (NUM_JS_INTERPOSERS + NUM_EV_INTERPOSERS)

/* Identity answered by the ioctls; must match the fake-udev definitions. */
#define FAKE_UDEV_DEVICE_NAME "Microsoft X-Box 360 pad"
#define FAKE_UDEV_VENDOR_ID   0x045e
#define FAKE_UDEV_PRODUCT_ID  0x028e
#define FAKE_UDEV_VERSION_ID  0x0114
#define FAKE_UDEV_BUS_TYPE    BUS_USB

static int g_sji_log_enabled = 0;

#define SJI_LOG_LEVEL_DEBUG "[DEBUG]"
#define SJI_LOG_LEVEL_INFO  "[INFO]"
#define SJI_LOG_LEVEL_WARN  "[WARN]"
#define SJI_LOG_LEVEL_ERROR "[ERROR]"

/* Real libc entry points, resolved with dlsym(RTLD_NEXT) by the constructor. */
static int (*real_open)(const char *pathname, int flags, ...) = NULL;
static int (*real_open64)(const char *pathname, int flags, ...) = NULL;
static int (*real_openat)(int dirfd, const char *pathname, int flags, ...) = NULL;
static int (*real_openat64)(int dirfd, const char *pathname, int flags, ...) = NULL;
static int (*real_ioctl)(int fd, ioctl_request_t request, ...) = NULL;
static int (*real_epoll_ctl)(int epfd, int op, int fd, struct epoll_event *event) = NULL;
static int (*real_close)(int fd) = NULL;
static ssize_t (*real_read)(int fd, void *buf, size_t count) = NULL;
static ssize_t (*real_write)(int fd, const void *buf, size_t count) = NULL;
static int (*real_access)(const char *pathname, int mode) = NULL;
static int (*real_fstat)(int fd, struct stat *buf) = NULL;
static int (*real_stat)(const char *pathname, struct stat *buf) = NULL;
static int (*real_lstat)(const char *pathname, struct stat *buf) = NULL;
#ifdef SJI_LFS64
static int (*real_stat64)(const char *pathname, struct stat64 *buf) = NULL;
static int (*real_lstat64)(const char *pathname, struct stat64 *buf) = NULL;
static int (*real_fstat64)(int fd, struct stat64 *buf) = NULL;
#endif
/* Pre-2.33 glibc lowers stat()/fstat()/lstat() at compile time to these versioned
 * __*xstat symbols (with a leading struct-version int), so binaries built against
 * old glibc -- Wine/Lutris/Steam runtimes and 32-bit builds -- never reach the
 * stat() wrappers above. Interpose the versioned entry points too. */
#ifdef __GLIBC__
static int (*real___xstat)(int ver, const char *pathname, struct stat *buf) = NULL;
static int (*real___lxstat)(int ver, const char *pathname, struct stat *buf) = NULL;
static int (*real___fxstat)(int ver, int fd, struct stat *buf) = NULL;
static int (*real___xstat64)(int ver, const char *pathname, struct stat64 *buf) = NULL;
static int (*real___lxstat64)(int ver, const char *pathname, struct stat64 *buf) = NULL;
static int (*real___fxstat64)(int ver, int fd, struct stat64 *buf) = NULL;
#endif
/* Directory listing: apps that scan /dev/input directly instead of asking
 * libudev (SDL with udev disabled or a sandbox build, GLFW, evtest) find the
 * gamepads only if the evdev nodes appear in the enumeration. */
static DIR *(*real_opendir)(const char *name) = NULL;
static struct dirent *(*real_readdir)(DIR *dirp) = NULL;
static int (*real_closedir)(DIR *dirp) = NULL;
static int (*real_scandir)(const char *dirp, struct dirent ***namelist,
                           int (*filter)(const struct dirent *),
                           int (*compar)(const struct dirent **, const struct dirent **)) = NULL;
#ifdef SJI_LFS64
static struct dirent64 *(*real_readdir64)(DIR *dirp) = NULL;
static int (*real_scandir64)(const char *dirp, struct dirent64 ***namelist,
                             int (*filter)(const struct dirent64 *),
                             int (*compar)(const struct dirent64 **, const struct dirent64 **)) = NULL;
#endif
static int (*real_inotify_add_watch)(int fd, const char *pathname, uint32_t mask) = NULL;
static int (*real_inotify_rm_watch)(int fd, int wd) = NULL;

static void sji_logging_init() {
    if (getenv("JS_LOG") != NULL) {
        g_sji_log_enabled = 1;
    }
}

/**
 * Writes one timestamped, level-tagged line to stderr through real_write (never
 * the interposed write) when logging is enabled.
 */
static void interposer_log(const char *level, const char *func_name, int line_num, const char *format, ...) {
    if (!g_sji_log_enabled) {
        return;
    }

    if (real_write == NULL) {
        return;
    }

    char buffer[2048];
    size_t current_pos = 0;
    ssize_t written_bytes_count;
    int printed_len;

    printed_len = snprintf(buffer + current_pos, sizeof(buffer) - current_pos, "[%lu]", (unsigned long)time(NULL));
    if (printed_len > 0) {
        current_pos += ((size_t)printed_len < (sizeof(buffer) - current_pos)) ? (size_t)printed_len : (sizeof(buffer) - current_pos - 1);
    }

    if (current_pos < sizeof(buffer) - 1) {
        printed_len = snprintf(buffer + current_pos, sizeof(buffer) - current_pos,
                                "[SJI]%s[%s:%d] ", level, func_name, line_num);
        if (printed_len > 0) {
            current_pos += ((size_t)printed_len < (sizeof(buffer) - current_pos)) ? (size_t)printed_len : (sizeof(buffer) - current_pos - 1);
        }
    }

    if (current_pos < sizeof(buffer) - 1) {
        va_list argp;
        va_start(argp, format);
        printed_len = vsnprintf(buffer + current_pos, sizeof(buffer) - current_pos, format, argp);
        va_end(argp);
        if (printed_len > 0) {
            current_pos += ((size_t)printed_len < (sizeof(buffer) - current_pos)) ? (size_t)printed_len : (sizeof(buffer) - current_pos - 1);
        }
    }

    if (current_pos < sizeof(buffer) - 1) {
        buffer[current_pos++] = '\n';
    } else if (current_pos < sizeof(buffer)) {
        buffer[sizeof(buffer) - 1] = '\n';
        current_pos = sizeof(buffer);
    } else {
         buffer[sizeof(buffer) - 1] = '\n';
         current_pos = sizeof(buffer);
    }

    buffer[ (current_pos < sizeof(buffer)) ? current_pos : (sizeof(buffer)-1) ] = '\0';

    size_t len_to_write = (current_pos < sizeof(buffer)) ? current_pos : (sizeof(buffer)-1);
    if(len_to_write > 0 && buffer[len_to_write-1] != '\n' && len_to_write < sizeof(buffer)-1) {
         buffer[len_to_write++] = '\n';
    }

    if (len_to_write > 0) {
        written_bytes_count = real_write(STDERR_FILENO, buffer, len_to_write);
        if (written_bytes_count < 0) {
        }
    }
}

#define sji_log_debug(...) interposer_log(SJI_LOG_LEVEL_DEBUG, __func__, __LINE__, __VA_ARGS__)
#define sji_log_info(...)  interposer_log(SJI_LOG_LEVEL_INFO,  __func__, __LINE__, __VA_ARGS__)
#define sji_log_warn(...)  interposer_log(SJI_LOG_LEVEL_WARN,  __func__, __LINE__, __VA_ARGS__)
#define sji_log_error(...) interposer_log(SJI_LOG_LEVEL_ERROR, __func__, __LINE__, __VA_ARGS__)

/**
 * Resolves `name` with dlsym(RTLD_NEXT) into *target_func_ptr unless already
 * set. Returns 0, or -1 when dlsym fails.
 */
static int load_real_func(void (**target_func_ptr)(void), const char *name) {
    if (*target_func_ptr != NULL) {
        return 0;
    }
    *target_func_ptr = dlsym(RTLD_NEXT, name);
    if (*target_func_ptr == NULL) {
        sji_log_error("Failed to load real '%s': %s. Interposer functionality may be compromised.", name, dlerror());
        return -1;
    }
    return 0;
}

/* Opaque JSIOCSCORR/JSIOCGCORR payload, stored and returned verbatim. */
typedef struct js_corr js_corr_t;

#define CONTROLLER_NAME_MAX_LEN 255
#define INTERPOSER_MAX_BTNS 512
#define INTERPOSER_MAX_AXES 64

/**
 * Device configuration the server sends first on every connection; layout and
 * size must match the server's byte for byte. btn_map and axes_map map logical
 * button and axis indices to evdev key and abs codes.
 */
typedef struct {
    char name[CONTROLLER_NAME_MAX_LEN];
    uint16_t vendor;
    uint16_t product;
    uint16_t version;
    uint16_t num_btns;
    uint16_t num_axes;
    uint16_t btn_map[INTERPOSER_MAX_BTNS];
    uint8_t axes_map[INTERPOSER_MAX_AXES];
    uint8_t final_alignment_padding[6];
} js_config_t;

/* Socket connections per device; real applications hold one handle (two briefly
 * when an enumeration pass overlaps use). Opens beyond this fail with EMFILE. */
#define SJI_MAX_HANDLES_PER_DEVICE 16

/* Largest event read in one go (input_event > js_event); bounds the partial stash. */
#define SJI_MAX_EVENT_SIZE (sizeof(struct input_event))

/**
 * One application open() handle: its own socket connection (fd) and open()
 * flags. `partial` holds the leading bytes of an event a non-blocking read
 * dequeued but could not complete within its budget: recv() removed them from
 * the kernel buffer, so they cannot be re-peeked and are prepended on this
 * handle's next read(). Accessed only under interposers_mutex.
 */
typedef struct {
    int fd;
    int open_flags;
    unsigned char partial[SJI_MAX_EVENT_SIZE];
    size_t partial_len;
} sji_handle_t;

/**
 * One interposed device: its type (DEV_TYPE_JS/DEV_TYPE_EV), device path, socket
 * path and the table of outstanding open() handles. handle_count is statically
 * zero, so fd lookups match nothing before the first open() even when an
 * intercepted call runs before the constructor. corr is device-global like the
 * kernel joystick driver's correction state; js_config is the server's
 * per-device configuration, identical on every connection and refreshed by
 * each successful open().
 */
typedef struct {
    uint8_t type;
    char open_dev_name[255];
    char socket_path[255];
    sji_handle_t handles[SJI_MAX_HANDLES_PER_DEVICE];
    int handle_count;
    js_corr_t corr;
    js_config_t js_config;
} js_interposer_t;

#define DEV_TYPE_JS 0
#define DEV_TYPE_EV 1

/* EVIOCGABS ranges: sticks and triggers, and the HAT/D-pad axes. */
#define ABS_AXIS_MIN_DEFAULT -32767
#define ABS_AXIS_MAX_DEFAULT 32767
#define ABS_HAT_MIN_DEFAULT -1
#define ABS_HAT_MAX_DEFAULT 1

static js_interposer_t interposers[NUM_INTERPOSERS()] = {
    { .type = DEV_TYPE_JS, .open_dev_name = JS0_DEVICE_PATH, .socket_path = JS0_SOCKET_PATH },
    { .type = DEV_TYPE_JS, .open_dev_name = JS1_DEVICE_PATH, .socket_path = JS1_SOCKET_PATH },
    { .type = DEV_TYPE_JS, .open_dev_name = JS2_DEVICE_PATH, .socket_path = JS2_SOCKET_PATH },
    { .type = DEV_TYPE_JS, .open_dev_name = JS3_DEVICE_PATH, .socket_path = JS3_SOCKET_PATH },
    { .type = DEV_TYPE_EV, .open_dev_name = EV0_DEVICE_PATH, .socket_path = EV0_SOCKET_PATH },
    { .type = DEV_TYPE_EV, .open_dev_name = EV1_DEVICE_PATH, .socket_path = EV1_SOCKET_PATH },
    { .type = DEV_TYPE_EV, .open_dev_name = EV2_DEVICE_PATH, .socket_path = EV2_SOCKET_PATH },
    { .type = DEV_TYPE_EV, .open_dev_name = EV3_DEVICE_PATH, .socket_path = EV3_SOCKET_PATH },
};

/**
 * Guards the interposers[] handle tables. Hosts are multithreaded (SDL runs
 * joystick handling on its own thread), so fd lookups and the open/close
 * transitions are serialized against torn js_config and use of a handle another
 * thread is retiring. The lock covers only the brief lookups and transitions,
 * never blocking socket I/O: the event recv() in read() and the connect/config
 * read in open() run unlocked, and open() publishes its private fd into the
 * table only once fully configured, so lookups never see a half-built handle.
 */
static pthread_mutex_t interposers_mutex = PTHREAD_MUTEX_INITIALIZER;

/**
 * The slot owning application fd `fd`, or NULL when it is not interposed.
 * Caller holds interposers_mutex. The optional outputs receive the handle's
 * open() flags and its index in the slot's handles[].
 */
static js_interposer_t *find_interposer_for_fd_locked(int fd, int *open_flags_out, int *handle_idx_out) {
    if (fd < 0) {
        return NULL;
    }
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        for (int h = 0; h < interposers[i].handle_count; h++) {
            if (interposers[i].handles[h].fd == fd) {
                if (open_flags_out != NULL) {
                    *open_flags_out = interposers[i].handles[h].open_flags;
                }
                if (handle_idx_out != NULL) {
                    *handle_idx_out = h;
                }
                return &interposers[i];
            }
        }
    }
    return NULL;
}

/* Constructor: logging, socket directory override, real libc entry points. */
__attribute__((constructor)) void init_interposer() {
    sji_logging_init();

    /* SELKIES_JS_SOCKET_PATH relocates the sockets (basename kept) to match the backend. */
    const char *sock_dir = getenv("SELKIES_JS_SOCKET_PATH");
    if (sock_dir && sock_dir[0]) {
        for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
            const char *slash = strrchr(interposers[i].socket_path, '/');
            const char *base = slash ? slash + 1 : interposers[i].socket_path;
            char newpath[sizeof(interposers[i].socket_path)];
            int n = snprintf(newpath, sizeof(newpath), "%s/%s", sock_dir, base);
            if (n > 0 && (size_t)n < sizeof(newpath)) {
                strncpy(interposers[i].socket_path, newpath, sizeof(interposers[i].socket_path) - 1);
                interposers[i].socket_path[sizeof(interposers[i].socket_path) - 1] = '\0';
            }
        }
    }

    if (load_real_func((void *)&real_open, "open") < 0) sji_log_error("CRITICAL: Failed to load real 'open'.");
    if (load_real_func((void *)&real_ioctl, "ioctl") < 0) sji_log_error("CRITICAL: Failed to load real 'ioctl'.");
    if (load_real_func((void *)&real_epoll_ctl, "epoll_ctl") < 0) sji_log_error("CRITICAL: Failed to load real 'epoll_ctl'.");
    if (load_real_func((void *)&real_close, "close") < 0) sji_log_error("CRITICAL: Failed to load real 'close'.");
    if (load_real_func((void *)&real_read, "read") < 0) sji_log_error("CRITICAL: Failed to load real 'read'.");
    if (load_real_func((void *)&real_write, "write") < 0) sji_log_error("CRITICAL: Failed to load real 'write'.");
    if (load_real_func((void *)&real_access, "access") < 0) sji_log_error("CRITICAL: Failed to load real 'access'.");
    if (load_real_func((void *)&real_fstat, "fstat") < 0) sji_log_error("CRITICAL: Failed to load real 'fstat'.");
    if (load_real_func((void *)&real_stat, "stat") < 0) sji_log_error("CRITICAL: Failed to load real 'stat'.");
    if (load_real_func((void *)&real_lstat, "lstat") < 0) sji_log_error("CRITICAL: Failed to load real 'lstat'.");
    load_real_func((void *)&real_open64, "open64");
    load_real_func((void *)&real_openat, "openat");
    load_real_func((void *)&real_openat64, "openat64");
    load_real_func((void *)&real_opendir, "opendir");
    load_real_func((void *)&real_readdir, "readdir");
    load_real_func((void *)&real_closedir, "closedir");
    load_real_func((void *)&real_scandir, "scandir");
#ifdef SJI_LFS64
    load_real_func((void *)&real_readdir64, "readdir64");
    load_real_func((void *)&real_scandir64, "scandir64");
#endif
    load_real_func((void *)&real_inotify_add_watch, "inotify_add_watch");
    load_real_func((void *)&real_inotify_rm_watch, "inotify_rm_watch");
    sji_log_info("Selkies Joystick Interposer initialized. Logging is %s.", g_sji_log_enabled ? "ENABLED" : "DISABLED");
}

static int make_socket_nonblocking(int sockfd) {
    int flags = fcntl(sockfd, F_GETFL, 0);
    if (flags == -1) {
        sji_log_error("make_socket_nonblocking: fcntl(F_GETFL) failed for fd %d: %s", sockfd, strerror(errno));
        return -1;
    }
    if (!(flags & O_NONBLOCK)) {
        if (fcntl(sockfd, F_SETFL, flags | O_NONBLOCK) == -1) {
            sji_log_error("make_socket_nonblocking: fcntl(F_SETFL, O_NONBLOCK) failed for fd %d: %s", sockfd, strerror(errno));
            return -1;
        }
        sji_log_info("Socket fd %d successfully set to O_NONBLOCK.", sockfd);
    } else {
        sji_log_debug("Socket fd %d was already O_NONBLOCK.", sockfd);
    }
    return 0;
}

/**
 * Interposed access(): the device paths are always accessible (the real call
 * is made only for the log); everything else passes through.
 */
int access(const char *pathname, int mode) {
    if (!real_access) {
        if (load_real_func((void *)&real_access, "access") < 0 || !real_access) {
            fprintf(stderr, "[SJI][CRITICAL][access] Real 'access' function not loaded and couldn't be loaded on demand for path: %s\n", pathname ? pathname : "NULL_PATH");
            errno = EFAULT;
            return -1;
        }
    }

    int is_our_target_device = 0;
    if (pathname) {
        for (size_t i = 0; i < NUM_INTERPOSERS(); ++i) {
            if (strcmp(pathname, interposers[i].open_dev_name) == 0) {
                is_our_target_device = 1;
                break;
            }
        }
    }

    if (is_our_target_device) {
        sji_log_info("Intercepted access for OUR DEVICE: '%s' (mode: 0x%x)", pathname, mode);

        int original_errno = errno;
        int real_return_value = real_access(pathname, mode);
        int real_errno_after_call = errno;
        
        sji_log_info("Real access for '%s' (mode 0x%x) would have returned %d (errno: %d - %s)",
                     pathname, mode, real_return_value, real_errno_after_call,
                     (real_errno_after_call != 0 ? strerror(real_errno_after_call) : "Success (errno 0)"));
        
        errno = original_errno;

        sji_log_info("Forcing SUCCESS (return 0) for access on '%s'", pathname);
        errno = 0;
        return 0;

    } else {
        return real_access(pathname, mode);
    }
}

/* The device index after `prefix`, or -1 when `path` is not that prefix followed
 * by digits alone. Parsed by hand because glibc 2.38 redirects sscanf to
 * __isoc23_sscanf, and referencing that symbol would stop this library from
 * loading on every distribution older than the one it was built on. */
static int dev_index_after(const char *path, const char *prefix) {
    size_t prefix_len = strlen(prefix);
    if (strncmp(path, prefix, prefix_len) != 0) return -1;
    const char *digits = path + prefix_len;
    if (*digits == '\0') return -1;
    int index = 0;
    for (const char *c = digits; *c != '\0'; c++) {
        if (*c < '0' || *c > '9') return -1;
        if (index > (INT_MAX - (*c - '0')) / 10) return -1;
        index = index * 10 + (*c - '0');
    }
    return index;
}

/* Forged character device: SDL dedupes devices by st_rdev, and a socket would
 * report 0 or a generic id, so each node gets input major 13 with its own index
 * as minor. Field names are identical in struct stat and stat64, so one macro
 * fills either. */
#define FILL_FAKE_STAT_FIELDS(buf, path) do {                              \
    (buf)->st_mode = S_IFCHR | 0666;                                       \
    int _dev_num = dev_index_after((path), "/dev/input/event");            \
    if (_dev_num < 0) _dev_num = dev_index_after((path), "/dev/input/js"); \
    (buf)->st_rdev = makedev(13, _dev_num < 0 ? 9999 : _dev_num);          \
    (buf)->st_uid = 0;                                                     \
    (buf)->st_gid = 0;                                                     \
    (buf)->st_size = 0;                                                    \
    (buf)->st_blksize = 4096;                                              \
    (buf)->st_blocks = 0;                                                  \
    (buf)->st_nlink = 1;                                                   \
} while (0)

static void fill_fake_stat(const char* path, struct stat *buf) {
    FILL_FAKE_STAT_FIELDS(buf, path);
}

#ifdef SJI_LFS64
static void fill_fake_stat64(const char* path, struct stat64 *buf) {
    FILL_FAKE_STAT_FIELDS(buf, path);
}
#endif

int fstat(int fd, struct stat *buf) {
    if (!real_fstat) {
         if (load_real_func((void *)&real_fstat, "fstat") < 0) {
             errno = EFAULT;
             return -1;
         }
    }

    pthread_mutex_lock(&interposers_mutex);
    js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL, NULL);
    if (interposer != NULL) {
        memset(buf, 0, sizeof(struct stat));
        fill_fake_stat(interposer->open_dev_name, buf);
        /* Log after unlock so a blocked stderr can't stall other hooked calls. */
        const char *dev = interposer->open_dev_name;
        pthread_mutex_unlock(&interposers_mutex);
        sji_log_debug("Intercepted fstat for fd %d (%s), returning fake rdev %d:%d",
            fd, dev, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real_fstat(fd, buf);
}

int stat(const char *pathname, struct stat *buf) {
    if (!real_stat) {
        if (load_real_func((void *)&real_stat, "stat") < 0) {
            errno = EFAULT;
            return -1;
        }
    }

    if (pathname) {
        for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
            if (strcmp(pathname, interposers[i].open_dev_name) == 0) {
                memset(buf, 0, sizeof(struct stat));
                fill_fake_stat(pathname, buf);
                
                sji_log_debug("Intercepted stat for %s, returning fake rdev %d:%d", 
                    pathname, major(buf->st_rdev), minor(buf->st_rdev));
                return 0;
            }
        }
    }
    return real_stat(pathname, buf);
}

int lstat(const char *pathname, struct stat *buf) {
    if (!real_lstat) {
        if (load_real_func((void *)&real_lstat, "lstat") < 0) {
            errno = EFAULT;
            return -1;
        }
    }

    if (pathname) {
        for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
            if (strcmp(pathname, interposers[i].open_dev_name) == 0) {
                memset(buf, 0, sizeof(struct stat));
                fill_fake_stat(pathname, buf);
                
                sji_log_debug("Intercepted lstat for %s, returning fake rdev %d:%d", 
                    pathname, major(buf->st_rdev), minor(buf->st_rdev));
                return 0;
            }
        }
    }
    return real_lstat(pathname, buf);
}

/* inline: only the glibc-specific wrappers below call it, so musl builds would
 * otherwise warn about an unused static. */
static inline int is_interposed_path(const char *pathname) {
    if (!pathname) return 0;
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (strcmp(pathname, interposers[i].open_dev_name) == 0) return 1;
    }
    return 0;
}

#ifdef SJI_LFS64
int stat64(const char *pathname, struct stat64 *buf) {
    if (!real_stat64) {
        if (load_real_func((void *)&real_stat64, "stat64") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(pathname, buf);
        sji_log_debug("Intercepted stat64 for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real_stat64(pathname, buf);
}

int lstat64(const char *pathname, struct stat64 *buf) {
    if (!real_lstat64) {
        if (load_real_func((void *)&real_lstat64, "lstat64") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(pathname, buf);
        sji_log_debug("Intercepted lstat64 for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real_lstat64(pathname, buf);
}

int fstat64(int fd, struct stat64 *buf) {
    if (!real_fstat64) {
        if (load_real_func((void *)&real_fstat64, "fstat64") < 0) { errno = EFAULT; return -1; }
    }
    pthread_mutex_lock(&interposers_mutex);
    js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL, NULL);
    if (interposer != NULL) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(interposer->open_dev_name, buf);
        const char *dev = interposer->open_dev_name;
        pthread_mutex_unlock(&interposers_mutex);
        sji_log_debug("Intercepted fstat64 for fd %d (%s), returning fake rdev %d:%d",
            fd, dev, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real_fstat64(fd, buf);
}
#endif /* SJI_LFS64 */

#ifdef __GLIBC__
/* `ver` is the caller's struct-stat ABI version: irrelevant for the forged
 * nodes, forwarded verbatim otherwise. Same for the five variants below. */
int __xstat(int ver, const char *pathname, struct stat *buf) {
    if (!real___xstat) {
        if (load_real_func((void *)&real___xstat, "__xstat") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat));
        fill_fake_stat(pathname, buf);
        sji_log_debug("Intercepted __xstat for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real___xstat(ver, pathname, buf);
}

int __lxstat(int ver, const char *pathname, struct stat *buf) {
    if (!real___lxstat) {
        if (load_real_func((void *)&real___lxstat, "__lxstat") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat));
        fill_fake_stat(pathname, buf);
        sji_log_debug("Intercepted __lxstat for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real___lxstat(ver, pathname, buf);
}

int __fxstat(int ver, int fd, struct stat *buf) {
    if (!real___fxstat) {
        if (load_real_func((void *)&real___fxstat, "__fxstat") < 0) { errno = EFAULT; return -1; }
    }
    pthread_mutex_lock(&interposers_mutex);
    js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL, NULL);
    if (interposer != NULL) {
        memset(buf, 0, sizeof(struct stat));
        fill_fake_stat(interposer->open_dev_name, buf);
        const char *dev = interposer->open_dev_name;
        pthread_mutex_unlock(&interposers_mutex);
        sji_log_debug("Intercepted __fxstat for fd %d (%s), returning fake rdev %d:%d",
            fd, dev, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real___fxstat(ver, fd, buf);
}

int __xstat64(int ver, const char *pathname, struct stat64 *buf) {
    if (!real___xstat64) {
        if (load_real_func((void *)&real___xstat64, "__xstat64") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(pathname, buf);
        sji_log_debug("Intercepted __xstat64 for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real___xstat64(ver, pathname, buf);
}

int __lxstat64(int ver, const char *pathname, struct stat64 *buf) {
    if (!real___lxstat64) {
        if (load_real_func((void *)&real___lxstat64, "__lxstat64") < 0) { errno = EFAULT; return -1; }
    }
    if (is_interposed_path(pathname)) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(pathname, buf);
        sji_log_debug("Intercepted __lxstat64 for %s, returning fake rdev %d:%d",
            pathname, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    return real___lxstat64(ver, pathname, buf);
}

int __fxstat64(int ver, int fd, struct stat64 *buf) {
    if (!real___fxstat64) {
        if (load_real_func((void *)&real___fxstat64, "__fxstat64") < 0) { errno = EFAULT; return -1; }
    }
    pthread_mutex_lock(&interposers_mutex);
    js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL, NULL);
    if (interposer != NULL) {
        memset(buf, 0, sizeof(struct stat64));
        fill_fake_stat64(interposer->open_dev_name, buf);
        const char *dev = interposer->open_dev_name;
        pthread_mutex_unlock(&interposers_mutex);
        sji_log_debug("Intercepted __fxstat64 for fd %d (%s), returning fake rdev %d:%d",
            fd, dev, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real___fxstat64(ver, fd, buf);
}
#endif /* __GLIBC__ */

#define SJI_INPUT_DIR "/dev/input"

/* "SJI": a non-zero d_ino base that cannot collide with real /dev/input inodes. */
#define SJI_SYNTH_INO_BASE 0x00534A49u

/* True when `path` names /dev/input (trailing slashes tolerated), the only
 * directory whose listing is augmented. */
static int is_dev_input_dir(const char *path) {
    if (!path) return 0;
    size_t len = strlen(path);
    while (len > 1 && path[len - 1] == '/') len--;
    return len == strlen(SJI_INPUT_DIR) && strncmp(path, SJI_INPUT_DIR, len) == 0;
}

/* The evdev slot (0..NUM_EV_INTERPOSERS-1) whose node basename is `name`, or -1;
 * dedupes a synthetic entry against a real one of the same name. */
static int ev_slot_for_basename(const char *name) {
    if (!name) return -1;
    for (int i = 0; i < NUM_EV_INTERPOSERS; i++) {
        const char *dev = interposers[NUM_JS_INTERPOSERS + i].open_dev_name;
        const char *slash = strrchr(dev, '/');
        if (strcmp(name, slash ? slash + 1 : dev) == 0) return i;
    }
    return -1;
}

/* Whether evdev slot `i` has a bound server socket. Only bound slots are
 * advertised, so a scanner never opens a node the server is not serving (which
 * would cost the full connect timeout before failing). */
static int ev_slot_socket_live(int i) {
    if (!real_access) return 0;
    /* errno must survive: the readdir/scandir caller reads it to tell
     * end-of-directory from an error. */
    int saved = errno;
    int live = real_access(interposers[NUM_JS_INTERPOSERS + i].socket_path, F_OK) == 0;
    errno = saved;
    return live;
}

static const char *ev_slot_basename(int i) {
    const char *dev = interposers[NUM_JS_INTERPOSERS + i].open_dev_name;
    const char *slash = strrchr(dev, '/');
    return slash ? slash + 1 : dev;
}

/* Per-open() state for a /dev/input DIR* stream: which synthetic slots the
 * real listing already carried (never emit those twice), which this stream has
 * already emitted, the next slot to consider, and the buffers the emitted
 * dirent points into (owned by the stream, valid until the next readdir). */
typedef struct {
    DIR *dir;
    uint8_t seen_mask;
    uint8_t emitted_mask;
    int cursor;
    struct dirent buf;
#ifdef SJI_LFS64
    struct dirent64 buf64;
#endif
} sji_dir_stream_t;

#define SJI_MAX_DIR_STREAMS 32
static sji_dir_stream_t dir_streams[SJI_MAX_DIR_STREAMS];
static pthread_mutex_t dir_streams_mutex = PTHREAD_MUTEX_INITIALIZER;

/* Must hold dir_streams_mutex. */
static sji_dir_stream_t *find_dir_stream_locked(DIR *dir) {
    for (int i = 0; i < SJI_MAX_DIR_STREAMS; i++) {
        if (dir_streams[i].dir == dir) return &dir_streams[i];
    }
    return NULL;
}

/* The next unseen, unemitted, live evdev slot for this stream, or -1 when the
 * synthetic entries are exhausted. Marks the slot emitted. Holds the mutex. */
static int next_synth_slot_locked(sji_dir_stream_t *st) {
    for (; st->cursor < NUM_EV_INTERPOSERS; st->cursor++) {
        int slot = st->cursor;
        if (st->seen_mask & (1u << slot)) continue;
        if (st->emitted_mask & (1u << slot)) continue;
        if (!ev_slot_socket_live(slot)) continue;
        st->emitted_mask |= (1u << slot);
        st->cursor++;
        return slot;
    }
    return -1;
}

/* Tags a /dev/input stream so its readdir augments the listing; a stream that
 * cannot be tagged (table full, other directory) behaves like the real one. */
DIR *opendir(const char *name) {
    if (!real_opendir && load_real_func((void *)&real_opendir, "opendir") < 0) {
        errno = EFAULT;
        return NULL;
    }
    DIR *dir = real_opendir(name);
    if (dir && is_dev_input_dir(name)) {
        pthread_mutex_lock(&dir_streams_mutex);
        sji_dir_stream_t *slot = find_dir_stream_locked(NULL);
        if (slot) {
            memset(slot, 0, sizeof(*slot));
            slot->dir = dir;
        }
        pthread_mutex_unlock(&dir_streams_mutex);
        sji_log_debug("Intercepted opendir(%s); augmenting listing.", name);
    }
    return dir;
}

/* Releases the tag before the DIR* is freed, so a later opendir reusing the
 * pointer starts clean. */
int closedir(DIR *dirp) {
    if (!real_closedir && load_real_func((void *)&real_closedir, "closedir") < 0) {
        errno = EFAULT;
        return -1;
    }
    pthread_mutex_lock(&dir_streams_mutex);
    sji_dir_stream_t *st = find_dir_stream_locked(dirp);
    if (st) st->dir = NULL;
    pthread_mutex_unlock(&dir_streams_mutex);
    return real_closedir(dirp);
}

/* Real entries first (noting any that match an interposer node so it is not
 * duplicated), then one synthetic evdev entry per call until the bound slots
 * are exhausted, then NULL. readdir64 below is the same for dirent64. */
struct dirent *readdir(DIR *dirp) {
    if (!real_readdir && load_real_func((void *)&real_readdir, "readdir") < 0) {
        errno = EFAULT;
        return NULL;
    }
    pthread_mutex_lock(&dir_streams_mutex);
    int tagged = find_dir_stream_locked(dirp) != NULL;
    pthread_mutex_unlock(&dir_streams_mutex);
    if (!tagged) return real_readdir(dirp);

    int saved_errno = errno;
    struct dirent *ent = real_readdir(dirp);
    if (ent) {
        int slot = ev_slot_for_basename(ent->d_name);
        if (slot >= 0) {
            pthread_mutex_lock(&dir_streams_mutex);
            sji_dir_stream_t *st = find_dir_stream_locked(dirp);
            if (st) st->seen_mask |= (1u << slot);
            pthread_mutex_unlock(&dir_streams_mutex);
        }
        return ent;
    }

    pthread_mutex_lock(&dir_streams_mutex);
    sji_dir_stream_t *st = find_dir_stream_locked(dirp);
    struct dirent *out = NULL;
    if (st) {
        int slot = next_synth_slot_locked(st);
        if (slot >= 0) {
            memset(&st->buf, 0, sizeof(st->buf));
            st->buf.d_ino = SJI_SYNTH_INO_BASE + slot;
            st->buf.d_type = DT_CHR;
            st->buf.d_reclen = sizeof(st->buf);
            snprintf(st->buf.d_name, sizeof(st->buf.d_name), "%s", ev_slot_basename(slot));
            out = &st->buf;
        }
    }
    pthread_mutex_unlock(&dir_streams_mutex);
    /* The caller reads errno to tell EOF from error; hand back what it was. */
    errno = saved_errno;
    return out;
}

/* Appends the bound evdev nodes not already present and passing `passes` to a
 * scandir result (allocated like scandir's own entries, so the caller frees
 * them), then re-sorts with `compare`. Shared by the 32- and 64-bit variants
 * through the element size and dirent writer. */
static int augment_scandir(void ***namelist, int n, size_t entry_size,
                           int (*passes)(const void *),
                           int (*compare)(const void *, const void *),
                           void (*fill)(void *ent, int slot)) {
    if (n < 0) return n;
    uint8_t seen = 0;
    for (int i = 0; i < n; i++) {
        /* d_name sits at the same offset in dirent and dirent64. */
        int slot = ev_slot_for_basename(((struct dirent *)(*namelist)[i])->d_name);
        if (slot >= 0) seen |= (1u << slot);
    }
    for (int slot = 0; slot < NUM_EV_INTERPOSERS; slot++) {
        if (seen & (1u << slot)) continue;
        if (!ev_slot_socket_live(slot)) continue;
        void *ent = malloc(entry_size);
        if (!ent) break;
        fill(ent, slot);
        if (passes && !passes(ent)) {
            free(ent);
            continue;
        }
        void **grown = realloc(*namelist, (size_t)(n + 1) * sizeof(void *));
        if (!grown) {
            free(ent);
            break;
        }
        *namelist = grown;
        grown[n++] = ent;
    }
    if (compare && n > 1) qsort(*namelist, (size_t)n, sizeof(void *), compare);
    return n;
}

static void fill_scandir_dirent(void *ent, int slot) {
    struct dirent *d = ent;
    memset(d, 0, sizeof(*d));
    d->d_ino = SJI_SYNTH_INO_BASE + slot;
    d->d_type = DT_CHR;
    d->d_reclen = sizeof(*d);
    snprintf(d->d_name, sizeof(d->d_name), "%s", ev_slot_basename(slot));
}

/* scandir hands the comparator (const struct dirent **); qsort calls it with
 * the addresses of the array elements, which are exactly those pointers. */
struct sji_scandir_ctx {
    int (*filter)(const struct dirent *);
    int (*compar)(const struct dirent **, const struct dirent **);
};
static __thread struct sji_scandir_ctx sji_scandir_ctx;
static int sji_scandir_passes(const void *ent) {
    return sji_scandir_ctx.filter(ent);
}
static int sji_scandir_compare(const void *a, const void *b) {
    return sji_scandir_ctx.compar((const struct dirent **)a, (const struct dirent **)b);
}

int scandir(const char *dirp, struct dirent ***namelist,
            int (*filter)(const struct dirent *),
            int (*compar)(const struct dirent **, const struct dirent **)) {
    if (!real_scandir && load_real_func((void *)&real_scandir, "scandir") < 0) {
        errno = EFAULT;
        return -1;
    }
    int n = real_scandir(dirp, namelist, filter, compar);
    if (n < 0 || !is_dev_input_dir(dirp)) return n;
    sji_scandir_ctx.filter = filter;
    sji_scandir_ctx.compar = compar;
    return augment_scandir((void ***)namelist, n, sizeof(struct dirent),
                           filter ? sji_scandir_passes : NULL,
                           compar ? sji_scandir_compare : NULL,
                           fill_scandir_dirent);
}

#ifdef SJI_LFS64
struct dirent64 *readdir64(DIR *dirp) {
    if (!real_readdir64 && load_real_func((void *)&real_readdir64, "readdir64") < 0) {
        errno = EFAULT;
        return NULL;
    }
    pthread_mutex_lock(&dir_streams_mutex);
    int tagged = find_dir_stream_locked(dirp) != NULL;
    pthread_mutex_unlock(&dir_streams_mutex);
    if (!tagged) return real_readdir64(dirp);

    int saved_errno = errno;
    struct dirent64 *ent = real_readdir64(dirp);
    if (ent) {
        int slot = ev_slot_for_basename(ent->d_name);
        if (slot >= 0) {
            pthread_mutex_lock(&dir_streams_mutex);
            sji_dir_stream_t *st = find_dir_stream_locked(dirp);
            if (st) st->seen_mask |= (1u << slot);
            pthread_mutex_unlock(&dir_streams_mutex);
        }
        return ent;
    }

    pthread_mutex_lock(&dir_streams_mutex);
    sji_dir_stream_t *st = find_dir_stream_locked(dirp);
    struct dirent64 *out = NULL;
    if (st) {
        int slot = next_synth_slot_locked(st);
        if (slot >= 0) {
            memset(&st->buf64, 0, sizeof(st->buf64));
            st->buf64.d_ino = SJI_SYNTH_INO_BASE + slot;
            st->buf64.d_type = DT_CHR;
            st->buf64.d_reclen = sizeof(st->buf64);
            snprintf(st->buf64.d_name, sizeof(st->buf64.d_name), "%s", ev_slot_basename(slot));
            out = &st->buf64;
        }
    }
    pthread_mutex_unlock(&dir_streams_mutex);
    errno = saved_errno;
    return out;
}

static void fill_scandir_dirent64(void *ent, int slot) {
    struct dirent64 *d = ent;
    memset(d, 0, sizeof(*d));
    d->d_ino = SJI_SYNTH_INO_BASE + slot;
    d->d_type = DT_CHR;
    d->d_reclen = sizeof(*d);
    snprintf(d->d_name, sizeof(d->d_name), "%s", ev_slot_basename(slot));
}

struct sji_scandir64_ctx {
    int (*filter)(const struct dirent64 *);
    int (*compar)(const struct dirent64 **, const struct dirent64 **);
};
static __thread struct sji_scandir64_ctx sji_scandir64_ctx;
static int sji_scandir64_passes(const void *ent) {
    return sji_scandir64_ctx.filter(ent);
}
static int sji_scandir64_compare(const void *a, const void *b) {
    return sji_scandir64_ctx.compar((const struct dirent64 **)a, (const struct dirent64 **)b);
}

int scandir64(const char *dirp, struct dirent64 ***namelist,
              int (*filter)(const struct dirent64 *),
              int (*compar)(const struct dirent64 **, const struct dirent64 **)) {
    if (!real_scandir64 && load_real_func((void *)&real_scandir64, "scandir64") < 0) {
        errno = EFAULT;
        return -1;
    }
    int n = real_scandir64(dirp, namelist, filter, compar);
    if (n < 0 || !is_dev_input_dir(dirp)) return n;
    sji_scandir64_ctx.filter = filter;
    sji_scandir64_ctx.compar = compar;
    return augment_scandir((void ***)namelist, n, sizeof(struct dirent64),
                           filter ? sji_scandir64_passes : NULL,
                           compar ? sji_scandir64_compare : NULL,
                           fill_scandir_dirent64);
}
#endif /* SJI_LFS64 */

/**
 * Reads one js_config_t from a freshly connected socket. The socket is made
 * blocking for the read (and restored), while SO_RCVTIMEO makes real_read
 * return EAGAIN periodically so a monotonic deadline of
 * SOCKET_CONFIG_READ_TIMEOUT_MS caps the cumulative wait; a connected-but-silent
 * peer therefore cannot hang the opening thread. The peer-supplied button and
 * axis counts are clamped to the map array bounds.
 *
 * @return 0 with the config filled, -1 on read error, EOF or timeout.
 */
static int read_socket_config(int sockfd, js_config_t *config_dest) {
    ssize_t bytes_to_read = sizeof(js_config_t);
    ssize_t bytes_read_total = 0;
    char *buffer_ptr = (char *)config_dest;
    int original_socket_flags = fcntl(sockfd, F_GETFL, 0);
    int socket_was_nonblocking = 0;

    struct timeval rcv_timeout = { .tv_sec = 1, .tv_usec = 0 };
    struct timeval saved_rcv_timeout;
    socklen_t saved_rcv_timeout_len = sizeof(saved_rcv_timeout);
    int have_saved_rcv_timeout =
        (getsockopt(sockfd, SOL_SOCKET, SO_RCVTIMEO, &saved_rcv_timeout, &saved_rcv_timeout_len) == 0);
    if (setsockopt(sockfd, SOL_SOCKET, SO_RCVTIMEO, &rcv_timeout, sizeof(rcv_timeout)) == -1) {
        sji_log_warn("read_socket_config: setsockopt(SO_RCVTIMEO) failed for sockfd %d: %s.", sockfd, strerror(errno));
    }
    struct timespec config_read_start;
    clock_gettime(CLOCK_MONOTONIC, &config_read_start);

    if (original_socket_flags == -1) {
        sji_log_warn("read_socket_config: fcntl(F_GETFL) failed for sockfd %d: %s. Cannot ensure blocking for config read.", sockfd, strerror(errno));
    } else if (original_socket_flags & O_NONBLOCK) {
        socket_was_nonblocking = 1;
        sji_log_debug("read_socket_config: sockfd %d is O_NONBLOCK. Temporarily setting to blocking for config read.", sockfd);
        if (fcntl(sockfd, F_SETFL, original_socket_flags & ~O_NONBLOCK) == -1) {
            sji_log_warn("read_socket_config: Failed to make sockfd %d blocking for config read: %s. Proceeding with potentially non-blocking read.", sockfd, strerror(errno));
        }
    }

    sji_log_info("Attempting to read joystick config (%zd bytes) from sockfd %d.", bytes_to_read, sockfd);
    while (bytes_read_total < bytes_to_read) {
        ssize_t current_read = real_read(sockfd, buffer_ptr + bytes_read_total, bytes_to_read - bytes_read_total);
        if (current_read == -1) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                struct timespec config_read_now;
                clock_gettime(CLOCK_MONOTONIC, &config_read_now);
                long elapsed_ms = (config_read_now.tv_sec - config_read_start.tv_sec) * 1000L +
                                  (config_read_now.tv_nsec - config_read_start.tv_nsec) / 1000000L;
                if (elapsed_ms >= SOCKET_CONFIG_READ_TIMEOUT_MS) {
                    sji_log_error("read_socket_config: timed out after %ldms waiting for config on sockfd %d (got %zd/%zd bytes).",
                                  elapsed_ms, sockfd, bytes_read_total, bytes_to_read);
                    goto config_read_cleanup;
                }
                sji_log_warn("read_socket_config: real_read on sockfd %d returned EAGAIN/EWOULDBLOCK. Retrying (elapsed %ldms).", sockfd, elapsed_ms);
                usleep(100000);
                continue;
            }
            sji_log_error("read_socket_config: real_read failed on sockfd %d: %s", sockfd, strerror(errno));
            goto config_read_cleanup;
        } else if (current_read == 0) {
            sji_log_error("read_socket_config: EOF on sockfd %d after %zd bytes (expected %zd). Peer closed connection?", sockfd, bytes_read_total, bytes_to_read);
            goto config_read_cleanup;
        }
        bytes_read_total += current_read;
    }

    /* Terminate the peer-supplied name before the %s log below reads past it. */
    if (strnlen(config_dest->name, CONTROLLER_NAME_MAX_LEN) == CONTROLLER_NAME_MAX_LEN) {
        config_dest->name[CONTROLLER_NAME_MAX_LEN-1] = '\0';
        sji_log_warn("Config name from server was not null-terminated within max length; forced termination.");
    }

    sji_log_info("Successfully read joystick config from sockfd %d: Name='%s', Vnd=0x%04x, Prd=0x%04x, Ver=0x%04x, Btns=%u, Axes=%u",
                 sockfd, config_dest->name, config_dest->vendor, config_dest->product, config_dest->version,
                 config_dest->num_btns, config_dest->num_axes);

    /* Unclamped peer counts would drive out-of-bounds reads of btn_map/axes_map
     * in the EVIOCGBIT handlers. */
    if (config_dest->num_btns > INTERPOSER_MAX_BTNS) {
        sji_log_warn("read_socket_config: num_btns %u exceeds max %u; clamping.", config_dest->num_btns, INTERPOSER_MAX_BTNS);
        config_dest->num_btns = INTERPOSER_MAX_BTNS;
    }
    if (config_dest->num_axes > INTERPOSER_MAX_AXES) {
        sji_log_warn("read_socket_config: num_axes %u exceeds max %u; clamping.", config_dest->num_axes, INTERPOSER_MAX_AXES);
        config_dest->num_axes = INTERPOSER_MAX_AXES;
    }

config_read_cleanup:
    if (have_saved_rcv_timeout) {
        setsockopt(sockfd, SOL_SOCKET, SO_RCVTIMEO, &saved_rcv_timeout, sizeof(saved_rcv_timeout));
    }
    if (socket_was_nonblocking && original_socket_flags != -1) {
        sji_log_debug("read_socket_config: Restoring O_NONBLOCK to sockfd %d.", sockfd);
        if (fcntl(sockfd, F_SETFL, original_socket_flags) == -1) {
            sji_log_warn("read_socket_config: Failed to restore O_NONBLOCK to sockfd %d: %s", sockfd, strerror(errno));
        }
    }
    return (bytes_read_total == bytes_to_read) ? 0 : -1;
}

/**
 * Connects to a device socket (retrying ENOENT/ECONNREFUSED for up to
 * SOCKET_CONNECT_TIMEOUT_MS), reads the device configuration and sends the
 * one-byte architecture specifier. Works on locals and out-params only, never
 * the shared slot, so it runs without interposers_mutex; the caller publishes
 * the fd and config under the lock.
 *
 * @return The connected socket fd, or -1.
 */
static int connect_interposer_socket(const char *socket_path, js_config_t *config_dest) {
    int sockfd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (sockfd == -1) {
        sji_log_error("Failed to create socket for %s: %s", socket_path, strerror(errno));
        return -1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(struct sockaddr_un));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, socket_path, sizeof(addr.sun_path) - 1);

    int attempt = 0;
    long total_slept_us = 0;
    long timeout_us = SOCKET_CONNECT_TIMEOUT_MS * 1000;
    long sleep_interval_us = 10000;

    sji_log_info("Attempting to connect to %s (fd %d)...", socket_path, sockfd);
    while (connect(sockfd, (struct sockaddr *)&addr, sizeof(struct sockaddr_un)) == -1) {
        if (errno == ENOENT || errno == ECONNREFUSED) {
            if (total_slept_us >= timeout_us) {
                sji_log_error("Timed out connecting to socket %s after %dms.", socket_path, SOCKET_CONNECT_TIMEOUT_MS);
                goto connect_fail;
            }
            if (attempt == 0 || (attempt % 10 == 0)) {
                 sji_log_warn("Connection to %s refused/not found, retrying (attempt %d, elapsed %ldms)...",
                              socket_path, attempt + 1, total_slept_us / 1000);
            }
            usleep(sleep_interval_us);
            total_slept_us += sleep_interval_us;
            attempt++;
            continue;
        }
        sji_log_error("Failed to connect to socket %s: %s", socket_path, strerror(errno));
        goto connect_fail;
    }
    sji_log_info("Connected to socket %s (fd %d).", socket_path, sockfd);

    if (read_socket_config(sockfd, config_dest) != 0) {
        sji_log_error("Failed to read config from socket %s.", socket_path);
        goto connect_fail;
    }

    unsigned char arch_byte[1] = { (unsigned char)sizeof(long) };
    sji_log_info("Sending architecture specifier (%u bytes, value: %u) to %s.", (unsigned int)sizeof(arch_byte), arch_byte[0], socket_path);
    if (real_write(sockfd, arch_byte, sizeof(arch_byte)) != sizeof(arch_byte)) {
        sji_log_error("Failed to send architecture specifier to %s: %s", socket_path, strerror(errno));
        goto connect_fail;
    }
    return sockfd;

connect_fail:
    real_close(sockfd);
    return -1;
}

/**
 * Shared body of the open wrappers: a matched device gets its own socket
 * connection, built unlocked on a private fd (the connect can block on its
 * timeout and would stall every other interposed call) and published into the
 * handle table under the lock once configured.
 *
 * @return The socket fd; -1 with errno EIO (connect failed) or EMFILE (handle
 *         table full); -2 when the path is not interposed and the caller uses
 *         the real open (a NULL path included, for the real EFAULT).
 */
static int common_open_logic(const char *pathname, int flags, js_interposer_t **found_interposer_ptr) {
    *found_interposer_ptr = NULL;

    if (pathname == NULL) {
        return -2;
    }

    /* Unlocked: the name fields are set at static initialization and never mutated. */
    js_interposer_t *interposer = NULL;
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (strcmp(pathname, interposers[i].open_dev_name) == 0) {
            interposer = &interposers[i];
            break;
        }
    }
    if (interposer == NULL) {
        return -2;
    }
    *found_interposer_ptr = interposer;

    js_config_t pending_config;
    memset(&pending_config, 0, sizeof(pending_config));
    int new_fd = connect_interposer_socket(interposer->socket_path, &pending_config);
    if (new_fd == -1) {
        sji_log_error("Failed to establish socket connection for %s.", pathname);
        errno = EIO;
        return -1;
    }

    if (flags & O_NONBLOCK) {
        sji_log_info("Application opened %s with O_NONBLOCK. Setting socket fd %d to non-blocking.",
                     pathname, new_fd);
        if (make_socket_nonblocking(new_fd) == -1) {
            sji_log_warn("Failed to make socket fd %d non-blocking for %s as requested by app. Socket may remain blocking.",
                          new_fd, pathname);
        }
    }

    pthread_mutex_lock(&interposers_mutex);
    if (interposer->handle_count >= SJI_MAX_HANDLES_PER_DEVICE) {
        pthread_mutex_unlock(&interposers_mutex);
        real_close(new_fd);
        sji_log_error("open for %s rejected: device already has the maximum of %d open handles.",
                      pathname, SJI_MAX_HANDLES_PER_DEVICE);
        errno = EMFILE;
        return -1;
    }
    interposer->handles[interposer->handle_count].fd = new_fd;
    interposer->handles[interposer->handle_count].open_flags = flags;
    interposer->handle_count++;
    interposer->js_config = pending_config;
    int open_handles = interposer->handle_count;
    pthread_mutex_unlock(&interposers_mutex);

    /* Gated so the success path makes no extra syscall and leaves errno alone when
     * logging is off. */
    int sock_flags = g_sji_log_enabled ? fcntl(new_fd, F_GETFL, 0) : 0;
    sji_log_info("Successfully interposed 'open' for %s (app_flags=0x%x), socket_fd: %d (%d handle(s) open). Socket flags: 0x%x",
                 pathname, flags, new_fd, open_handles, sock_flags);
    return new_fd;
}

/* Device paths get a socket handle from common_open_logic(); everything else
 * goes to the real open(), with the mode argument pulled only when NEEDS_MODE. */
int open(const char *pathname, int flags, ...) {
    if (!real_open) {
        errno = EFAULT;
        return -1;
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(pathname, flags, &interposer);

    if (result_fd == -2) {
        if (NEEDS_MODE(flags)) {
            va_list args;
            va_start(args, flags);
            mode_t mode = va_arg(args, mode_t);
            va_end(args);
            result_fd = real_open(pathname, flags, mode);
        } else {
            result_fd = real_open(pathname, flags);
        }
    }
    return result_fd;
}

#ifdef open64
#undef open64
#endif

/* As open(); falls back to the real open() when no real open64 exists. */
int open64(const char *pathname, int flags, ...) {
    if (!real_open64 && !real_open) {
        errno = EFAULT;
        return -1;
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(pathname, flags, &interposer);

    if (result_fd == -2) {
        if (NEEDS_MODE(flags)) {
            va_list args;
            va_start(args, flags);
            mode_t mode = va_arg(args, mode_t);
            va_end(args);

            if (real_open64) {
                result_fd = real_open64(pathname, flags, mode);
            } else {
                result_fd = real_open(pathname, flags, mode);
            }
        } else {
            if (real_open64) {
                result_fd = real_open64(pathname, flags);
            } else {
                result_fd = real_open(pathname, flags);
            }
        }
    }
    return result_fd;
}

/* As open(), with a relative path resolved against dirfd through /proc/self/fd
 * for the device match only; the real call receives the original arguments. */
int openat(int dirfd, const char *pathname, int flags, ...) {
    if (!real_openat) {
        errno = EFAULT;
        return -1;
    }

    char full_path[4096];
    const char *check_path = pathname;

    if (pathname && pathname[0] != '/' && dirfd != AT_FDCWD) {
        char procfd[64];
        snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", dirfd);
        ssize_t len = readlink(procfd, full_path, sizeof(full_path) - 1);
        if (len > 0 && (size_t)len < sizeof(full_path) - 1) {
            int written = snprintf(full_path + len, sizeof(full_path) - (size_t)len, "/%s", pathname);
            if (written > 0 && (size_t)written < sizeof(full_path) - (size_t)len) {
                check_path = full_path;
            }
        }
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(check_path, flags, &interposer);

    if (result_fd == -2) {
        if (NEEDS_MODE(flags)) {
            va_list args;
            va_start(args, flags);
            mode_t mode = va_arg(args, mode_t);
            va_end(args);
            result_fd = real_openat(dirfd, pathname, flags, mode);
        } else {
            result_fd = real_openat(dirfd, pathname, flags);
        }
    }
    return result_fd;
}

#ifdef openat64
#undef openat64
#endif

/* As openat(); falls back to the real openat() when no real openat64 exists. */
int openat64(int dirfd, const char *pathname, int flags, ...) {
    if (!real_openat64 && !real_openat) {
        errno = EFAULT;
        return -1;
    }

    char full_path[4096];
    const char *check_path = pathname;

    if (pathname && pathname[0] != '/' && dirfd != AT_FDCWD) {
        char procfd[64];
        snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", dirfd);
        ssize_t len = readlink(procfd, full_path, sizeof(full_path) - 1);
        if (len > 0 && (size_t)len < sizeof(full_path) - 1) {
            int written = snprintf(full_path + len, sizeof(full_path) - (size_t)len, "/%s", pathname);
            if (written > 0 && (size_t)written < sizeof(full_path) - (size_t)len) {
                check_path = full_path;
            }
        }
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(check_path, flags, &interposer);

    if (result_fd == -2) {
        if (NEEDS_MODE(flags)) {
            va_list args;
            va_start(args, flags);
            mode_t mode = va_arg(args, mode_t);
            va_end(args);

            if (real_openat64) {
                result_fd = real_openat64(dirfd, pathname, flags, mode);
            } else {
                result_fd = real_openat(dirfd, pathname, flags, mode);
            }
        } else {
            if (real_openat64) {
                result_fd = real_openat64(dirfd, pathname, flags);
            } else {
                result_fd = real_openat(dirfd, pathname, flags);
            }
        }
    }
    return result_fd;
}

/* An interposed handle is retired from its device's table and its own socket
 * closed; other handles of the device are unaffected, and the last one to go
 * clears the cached config. */

/* Hotplug for scanners that watch /dev/input themselves (SDL without udev,
 * GLFW-style pollers): a watch an application puts on /dev/input is shadowed
 * by one on the socket directory, and the records that watch produces are
 * rewritten in read() into what the application asked for — the socket
 * "selkies_event1000.sock" appearing or vanishing becomes "event1000" under
 * its own watch descriptor. Only evdev slots surface, as in the listing; the
 * kernel's own readiness applies, since the records are real. */
typedef struct {
    int used;
    int fd;
    int app_wd;
    int sock_wd;
} sji_inotify_watch_t;

#define SJI_MAX_INOTIFY_WATCHES 16
static sji_inotify_watch_t inotify_watches[SJI_MAX_INOTIFY_WATCHES];
static pthread_mutex_t inotify_mutex = PTHREAD_MUTEX_INITIALIZER;

/* The directory the evdev sockets are bound in, as resolved at load time. */
static void ev_socket_dir(char *out, size_t n) {
    const char *path = interposers[NUM_JS_INTERPOSERS].socket_path;
    const char *slash = strrchr(path, '/');
    size_t len = slash ? (size_t)(slash - path) : 0;
    if (slash && len == 0) len = 1;
    if (len == 0) {
        snprintf(out, n, ".");
        return;
    }
    if (len >= n) len = n - 1;
    memcpy(out, path, len);
    out[len] = '\0';
}

/* The evdev node basename whose socket basename is `name`, NULL for any
 * other name (a js socket included). */
static const char *ev_node_for_socket_name(const char *name) {
    for (int i = 0; i < NUM_EV_INTERPOSERS; i++) {
        const char *sock = interposers[NUM_JS_INTERPOSERS + i].socket_path;
        const char *slash = strrchr(sock, '/');
        if (strcmp(name, slash ? slash + 1 : sock) == 0) return ev_slot_basename(i);
    }
    return NULL;
}

/* Whether `fd` carries a shadowed /dev/input watch. */
static int inotify_fd_tracked(int fd) {
    int tracked = 0;
    pthread_mutex_lock(&inotify_mutex);
    for (int i = 0; i < SJI_MAX_INOTIFY_WATCHES; i++) {
        if (inotify_watches[i].used && inotify_watches[i].fd == fd) {
            tracked = 1;
            break;
        }
    }
    pthread_mutex_unlock(&inotify_mutex);
    return tracked;
}

/* Drops every shadow watch of `fd`; the kernel releases the watches with the fd. */
static void inotify_forget_fd(int fd) {
    pthread_mutex_lock(&inotify_mutex);
    for (int i = 0; i < SJI_MAX_INOTIFY_WATCHES; i++) {
        if (inotify_watches[i].used && inotify_watches[i].fd == fd) inotify_watches[i].used = 0;
    }
    pthread_mutex_unlock(&inotify_mutex);
}

int inotify_add_watch(int fd, const char *pathname, uint32_t mask) {
    if (!real_inotify_add_watch && load_real_func((void *)&real_inotify_add_watch, "inotify_add_watch") < 0) {
        errno = EFAULT;
        return -1;
    }
    int wd = real_inotify_add_watch(fd, pathname, mask);
    if (wd < 0 || !is_dev_input_dir(pathname)) return wd;
    uint32_t wanted = mask & (IN_CREATE | IN_DELETE | IN_MOVED_TO | IN_MOVED_FROM);
    if (!wanted) return wd;
    char dir[sizeof(interposers[0].socket_path)];
    ev_socket_dir(dir, sizeof(dir));
    if (is_dev_input_dir(dir)) return wd;
    int saved = errno;
    int sock_wd = real_inotify_add_watch(fd, dir, wanted | IN_ONLYDIR);
    errno = saved;
    if (sock_wd < 0) {
        sji_log_error("inotify watch on %s failed: %s; no hotplug for the /dev/input watch on fd %d.",
                      dir, strerror(errno), fd);
        return wd;
    }
    pthread_mutex_lock(&inotify_mutex);
    sji_inotify_watch_t *free_slot = NULL;
    for (int i = 0; i < SJI_MAX_INOTIFY_WATCHES; i++) {
        sji_inotify_watch_t *w = &inotify_watches[i];
        if (w->used && w->fd == fd && w->app_wd == wd) {
            /* The kernel returns the same wd for a re-added path; the shadow
             * is one watch too, so keep the existing pair. */
            free_slot = NULL;
            break;
        }
        if (!w->used && !free_slot) free_slot = w;
    }
    if (free_slot) {
        free_slot->used = 1;
        free_slot->fd = fd;
        free_slot->app_wd = wd;
        free_slot->sock_wd = sock_wd;
        sock_wd = -1;
    }
    pthread_mutex_unlock(&inotify_mutex);
    if (sock_wd >= 0) {
        real_inotify_rm_watch(fd, sock_wd);
    }
    return wd;
}

int inotify_rm_watch(int fd, int wd) {
    if (!real_inotify_rm_watch && load_real_func((void *)&real_inotify_rm_watch, "inotify_rm_watch") < 0) {
        errno = EFAULT;
        return -1;
    }
    int sock_wd = -1;
    pthread_mutex_lock(&inotify_mutex);
    for (int i = 0; i < SJI_MAX_INOTIFY_WATCHES; i++) {
        sji_inotify_watch_t *w = &inotify_watches[i];
        if (w->used && w->fd == fd && w->app_wd == wd) {
            sock_wd = w->sock_wd;
            w->used = 0;
        }
    }
    pthread_mutex_unlock(&inotify_mutex);
    int ret = real_inotify_rm_watch(fd, wd);
    if (sock_wd >= 0) {
        int saved = errno;
        real_inotify_rm_watch(fd, sock_wd);
        errno = saved;
    }
    return ret;
}

/* A read() on a shadowed inotify fd: the socket directory's records are
 * rewritten to the /dev/input watch and node names, the rest pass through.
 * Records only shrink or vanish, so the rewrite is in place; a read that
 * carried nothing for the application is read again on a blocking fd and
 * answered EAGAIN on a non-blocking one, never 0, which would read as EOF. */
static ssize_t inotify_read(int fd, void *buf, size_t count) {
    for (;;) {
        ssize_t n = real_read(fd, buf, count);
        if (n <= 0) return n;
        sji_inotify_watch_t watches[SJI_MAX_INOTIFY_WATCHES];
        int nwatch = 0;
        pthread_mutex_lock(&inotify_mutex);
        for (int i = 0; i < SJI_MAX_INOTIFY_WATCHES; i++) {
            if (inotify_watches[i].used && inotify_watches[i].fd == fd) watches[nwatch++] = inotify_watches[i];
        }
        pthread_mutex_unlock(&inotify_mutex);
        unsigned char *p = buf;
        size_t in = 0, out = 0, total = (size_t)n;
        while (in + sizeof(struct inotify_event) <= total) {
            struct inotify_event ev;
            memcpy(&ev, p + in, sizeof(ev));
            size_t rec = sizeof(ev) + ev.len;
            if (in + rec > total) break;
            const sji_inotify_watch_t *w = NULL;
            for (int i = 0; i < nwatch; i++) {
                if (watches[i].sock_wd == ev.wd) { w = &watches[i]; break; }
            }
            if (!w) {
                if (out != in) memmove(p + out, p + in, rec);
                out += rec;
                in += rec;
                continue;
            }
            char name[NAME_MAX + 1];
            const char *node = NULL;
            if (ev.len > 0) {
                size_t nl = strnlen((const char *)(p + in + sizeof(ev)), ev.len);
                if (nl <= NAME_MAX) {
                    memcpy(name, p + in + sizeof(ev), nl);
                    name[nl] = '\0';
                    node = ev_node_for_socket_name(name);
                }
            }
            in += rec;
            if (!node) continue;
            size_t node_len = strlen(node) + 1;
            struct inotify_event outev = {
                .wd = w->app_wd,
                .mask = ev.mask,
                .cookie = ev.cookie,
                .len = (uint32_t)((node_len + sizeof(ev) - 1) / sizeof(ev) * sizeof(ev)),
            };
            memcpy(p + out, &outev, sizeof(outev));
            memset(p + out + sizeof(outev), 0, outev.len);
            memcpy(p + out + sizeof(outev), node, node_len);
            out += sizeof(outev) + outev.len;
        }
        if (in < total) {
            memmove(p + out, p + in, total - in);
            out += total - in;
        }
        if (out > 0) return (ssize_t)out;
        int flags = fcntl(fd, F_GETFL);
        if (flags >= 0 && (flags & O_NONBLOCK)) {
            errno = EAGAIN;
            return -1;
        }
    }
}

int close(int fd) {
    if (!real_close) {
        sji_log_error("CRITICAL: real_close not loaded. Cannot proceed with close call.");
        errno = EFAULT;
        return -1;
    }
    inotify_forget_fd(fd);

    pthread_mutex_lock(&interposers_mutex);
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        js_interposer_t *interposer = &interposers[i];
        for (int h = 0; h < interposer->handle_count; h++) {
            if (interposer->handles[h].fd != fd) {
                continue;
            }
            /* Retire the handle before calling real_close(): on Linux the fd
             * is released by the kernel even when close() reports an error
             * (e.g. EINTR), so keeping the entry would leave a stale mapping
             * that could hijack a later reused fd number. */
            interposer->handles[h] = interposer->handles[interposer->handle_count - 1];
            interposer->handle_count--;
            if (interposer->handle_count == 0) {
                memset(&(interposer->js_config), 0, sizeof(js_config_t));
            }
            int ret = real_close(fd);
            int close_errno = errno;
            /* Log after unlock so a blocked stderr can't stall other hooked calls. */
            const char *dev = interposer->open_dev_name;
            int remaining = interposer->handle_count;
            pthread_mutex_unlock(&interposers_mutex);
            if (ret != 0) {
                sji_log_error("real_close on socket fd %d for %s failed: %s. Handle retired anyway.",
                              fd, dev, strerror(close_errno));
            }
            sji_log_info("Intercepted 'close' for interposed fd %d (device %s); %d handle(s) still open.",
                         fd, dev, remaining);
            errno = close_errno;
            return ret;
        }
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real_close(fd);
}

/**
 * Bounded drain of the remainder of a partially consumed event. The peek and
 * the consuming recv() are not atomic, so a non-blocking consume can come up
 * short; those bytes are already out of the kernel buffer, so the rest is
 * drained here, waiting with poll() for at most `budget_ms` so a peer that
 * stalls mid-event cannot hang the caller. Appends at `*consumed`.
 *
 * @return 1 once the whole event is in `buf`; 0 when only a prefix is (budget
 *         exhausted, EOF or hard error), with `*consumed` counting it.
 */
static int drain_event_remainder(int fd, void *buf, size_t *consumed, size_t event_size, int budget_ms) {
    struct timespec drain_start;
    clock_gettime(CLOCK_MONOTONIC, &drain_start);
    while (*consumed < event_size) {
        ssize_t tail = recv(fd, (char *)buf + *consumed, event_size - *consumed, MSG_DONTWAIT);
        if (tail > 0) {
            *consumed += (size_t)tail;
            continue;
        }
        if (tail == 0) {
            return 0; /* EOF mid-event */
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return 0; /* hard error */
        }
        struct timespec drain_now;
        clock_gettime(CLOCK_MONOTONIC, &drain_now);
        long elapsed_ms = (drain_now.tv_sec - drain_start.tv_sec) * 1000L +
                          (drain_now.tv_nsec - drain_start.tv_nsec) / 1000000L;
        int remaining_ms = budget_ms - (int)elapsed_ms;
        if (remaining_ms <= 0) {
            return 0; /* drain budget exhausted */
        }
        struct pollfd pfd = { .fd = fd, .events = POLLIN, .revents = 0 };
        int prc = poll(&pfd, 1, remaining_ms);
        if (prc <= 0) {
            return 0; /* timeout (0) or poll error/EINTR (<0) */
        }
    }
    return 1;
}

/**
 * Stashes a partial-event prefix on the handle owning `fd` for its next read().
 * Caller holds interposers_mutex. A lookup miss means the handle was closed
 * concurrently; the bytes are dropped, but that fd is already dead.
 */
static void stash_partial_event_locked(int fd, const void *buf, size_t len) {
    if (len == 0 || len > SJI_MAX_EVENT_SIZE) {
        return;
    }
    int handle_idx = -1;
    js_interposer_t *slot = find_interposer_for_fd_locked(fd, NULL, &handle_idx);
    if (slot != NULL && handle_idx >= 0) {
        memcpy(slot->handles[handle_idx].partial, buf, len);
        slot->handles[handle_idx].partial_len = len;
    }
}

/**
 * Blocking read of the rest of one event from `*consumed`. recv(MSG_WAITALL)
 * alone returns a short count when a signal lands mid-transfer, which handed
 * to the application would desync the SOCK_STREAM, so a short return counts as
 * progress and EINTR restarts; EINTR surfaces only while nothing of the event
 * is held yet. `*consumed` always reflects the bytes in `buf`, so on EOF or a
 * hard error the caller can stash the prefix.
 *
 * @return 1 once the event is complete; 0 on EOF mid-event; -1 with errno set.
 */
static int recv_event_rest_blocking(int fd, void *buf, size_t *consumed, size_t event_size) {
    while (*consumed < event_size) {
        ssize_t tail = recv(fd, (char *)buf + *consumed, event_size - *consumed, MSG_WAITALL);
        if (tail > 0) {
            *consumed += (size_t)tail; /* short == interrupted mid-event: keep going */
            continue;
        }
        if (tail == 0) {
            return 0; /* EOF */
        }
        if (errno == EINTR) {
            if (*consumed == 0) {
                return -1; /* nothing consumed: let the app see EINTR */
            }
            continue;
        }
        return -1;
    }
    return 1;
}

/**
 * Interposed read(): one whole event (js_event or input_event by device type)
 * per call, EINVAL for a buffer smaller than that. The event stream must never
 * be consumed short, or every later read is misaligned: a non-blocking handle
 * peeks before consuming, and a partially drained event (non-blocking budget
 * exhausted, or EOF/error on a blocking handle) is stashed on the handle and
 * completed by its next read(). Blocking mode follows the socket's actual
 * O_NONBLOCK flag, the handle's open() flags being the fallback.
 */
ssize_t read(int fd, void *buf, size_t count) {
    if (!real_read) {
        sji_log_error("CRITICAL: real_read not loaded. Cannot proceed with read call.");
        errno = EFAULT;
        return -1;
    }
    if (inotify_fd_tracked(fd)) {
        return inotify_read(fd, buf, count);
    }

    js_interposer_t *interposer = NULL;
    int handle_open_flags = 0;
    /* Taken under the lookup's lock so a concurrent close() can't tear the
     * handle out mid-copy. */
    unsigned char stashed[SJI_MAX_EVENT_SIZE];
    size_t stashed_len = 0;
    int handle_idx = -1;
    pthread_mutex_lock(&interposers_mutex);
    interposer = find_interposer_for_fd_locked(fd, &handle_open_flags, &handle_idx);
    if (interposer != NULL && handle_idx >= 0 && interposer->handles[handle_idx].partial_len > 0) {
        stashed_len = interposer->handles[handle_idx].partial_len;
        memcpy(stashed, interposer->handles[handle_idx].partial, stashed_len);
        interposer->handles[handle_idx].partial_len = 0;
    }
    pthread_mutex_unlock(&interposers_mutex);

    if (interposer == NULL) {
        return real_read(fd, buf, count);
    }

    size_t event_size;
    if (interposer->type == DEV_TYPE_JS) {
        event_size = sizeof(struct js_event);
    } else if (interposer->type == DEV_TYPE_EV) {
        event_size = sizeof(struct input_event);
    } else {
        sji_log_error("read: Unknown interposer type %d for fd %d (%s)", interposer->type, fd, interposer->open_dev_name);
        errno = EBADF;
        return -1;
    }

    if (count == 0) return 0;

    if (count < event_size) {
        sji_log_warn("read for %s (fd %d): app buffer too small (%zu bytes) for one event (%zu bytes).",
                     interposer->open_dev_name, fd, count, event_size);
        errno = EINVAL;
        return -1;
    }

    /* Unlocked from here: a concurrent close() retiring the handle leaves the
     * caller's fd with kernel read() semantics (EBADF at worst). */
    int socket_actual_flags = fcntl(fd, F_GETFL, 0);
    int socket_is_actually_nonblocking = (socket_actual_flags != -1 && (socket_actual_flags & O_NONBLOCK));

    if (socket_actual_flags == -1) {
        sji_log_warn("read: fcntl(F_GETFL) failed for sockfd %d (%s): %s. Proceeding, assuming blocking status based on this handle's open() flags.",
                     fd, interposer->open_dev_name, strerror(errno));
        socket_is_actually_nonblocking = (handle_open_flags & O_NONBLOCK);
    }

    const int drain_budget_ms = 10;

    if (stashed_len > 0) {
        memcpy(buf, stashed, stashed_len);
        size_t event_consumed = stashed_len;
        if (socket_is_actually_nonblocking) {
            if (!drain_event_remainder(fd, buf, &event_consumed, event_size, drain_budget_ms)) {
                /* Still short: re-stash and have the caller retry; no bytes lost. */
                pthread_mutex_lock(&interposers_mutex);
                stash_partial_event_locked(fd, buf, event_consumed);
                pthread_mutex_unlock(&interposers_mutex);
                errno = EAGAIN;
                return -1;
            }
        } else {
            int rest = recv_event_rest_blocking(fd, buf, &event_consumed, event_size);
            if (rest != 1) {
                /* Re-stash the prefix, then surface the EOF/error. */
                if (rest == 0) {
                    sji_log_info("SOCKET_READ_EOF: fd %d (%s) closed mid-stashed-event.",
                                 fd, interposer->open_dev_name);
                } else {
                    sji_log_error("SOCKET_READ_ERR: fd %d (%s) failed completing stashed event: %s",
                                  fd, interposer->open_dev_name, strerror(errno));
                }
                int saved_errno = errno;
                pthread_mutex_lock(&interposers_mutex);
                stash_partial_event_locked(fd, buf, event_consumed);
                pthread_mutex_unlock(&interposers_mutex);
                errno = saved_errno;
                return rest; /* 0 (EOF) or -1 (error) */
            }
        }
        sji_log_debug("SOCKET_READ_OK: completed stashed event (%zu bytes) on fd %d (%s)",
                      event_consumed, fd, interposer->open_dev_name);
        return (ssize_t)event_consumed;
    }

    ssize_t bytes_read;
    if (socket_is_actually_nonblocking) {
        /* Peek first; only dequeue once a whole event is buffered. */
        ssize_t peeked = recv(fd, buf, event_size, MSG_PEEK | MSG_DONTWAIT);
        if (peeked > 0 && (size_t)peeked < event_size) {
            sji_log_debug("read: sockfd %d (%s) has a partial event buffered (%zd/%zu bytes); leaving it queued.",
                          fd, interposer->open_dev_name, peeked, event_size);
            errno = EAGAIN;
            return -1;
        }
        if (peeked <= 0) {
            bytes_read = peeked; /* error (e.g. EAGAIN) or EOF; handled below */
        } else {
            /* Peek and consume aren't atomic: a short consume must be drained. */
            bytes_read = recv(fd, buf, event_size, MSG_DONTWAIT);
            if (bytes_read > 0 && (size_t)bytes_read < event_size) {
                size_t event_consumed = (size_t)bytes_read;
                if (!drain_event_remainder(fd, buf, &event_consumed, event_size, drain_budget_ms)) {
                    /* Stash the prefix and report EAGAIN; the next read completes it. */
                    pthread_mutex_lock(&interposers_mutex);
                    stash_partial_event_locked(fd, buf, event_consumed);
                    pthread_mutex_unlock(&interposers_mutex);
                    sji_log_debug("read: sockfd %d (%s) drained only %zu/%zu bytes; stashed and returning EAGAIN.",
                                  fd, interposer->open_dev_name, event_consumed, event_size);
                    errno = EAGAIN;
                    return -1;
                }
                bytes_read = (ssize_t)event_consumed;
            }
        }
    } else {
        size_t event_consumed = 0;
        int rest = recv_event_rest_blocking(fd, buf, &event_consumed, event_size);
        if (rest == 1) {
            bytes_read = (ssize_t)event_consumed;
        } else {
            if (event_consumed > 0) {
                /* Keep the prefix for the next read rather than return it short. */
                int saved_errno = errno;
                pthread_mutex_lock(&interposers_mutex);
                stash_partial_event_locked(fd, buf, event_consumed);
                pthread_mutex_unlock(&interposers_mutex);
                errno = saved_errno;
            }
            bytes_read = rest; /* 0 (EOF) or -1 (error) */
        }
    }

    if (bytes_read == -1) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (socket_is_actually_nonblocking) {
                 sji_log_debug("read: sockfd %d (%s) non-blocking, no data (EAGAIN/EWOULDBLOCK)", fd, interposer->open_dev_name);
            } else {
                 sji_log_warn("read: sockfd %d (%s) reported as blocking, but got EAGAIN/EWOULDBLOCK. This might indicate an issue or a race condition.", fd, interposer->open_dev_name);
            }
        } else {
            sji_log_error("SOCKET_READ_ERR: read from socket_fd %d (%s) failed: %s (errno %d)",
                          fd, interposer->open_dev_name, strerror(errno), errno);
        }
        return -1;
    } else if (bytes_read == 0) {
        sji_log_info("SOCKET_READ_EOF: read from socket_fd %d (%s) returned 0 (EOF - server closed connection?)",
                     fd, interposer->open_dev_name);
        return 0;
    } else {
        sji_log_debug("SOCKET_READ_OK: read %zd bytes from socket_fd %d (%s)",
                     bytes_read, fd, interposer->open_dev_name);
        if (bytes_read > 0 && (size_t)bytes_read < event_size) {
            sji_log_warn("SOCKET_READ_PARTIAL: read %zd bytes from socket_fd %d (%s), but expected %zu. This might cause issues.",
                         bytes_read, fd, interposer->open_dev_name, event_size);
        }
    }
    return bytes_read;
}

/* An interposed handle added to or modified in an epoll set is switched to
 * O_NONBLOCK first, as epoll consumers expect; only that handle's connection
 * is affected. */
int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event) {
    if (!real_epoll_ctl) {
        sji_log_error("CRITICAL: real_epoll_ctl not loaded. Cannot proceed with epoll_ctl call.");
        errno = EFAULT;
        return -1;
    }

    if (op == EPOLL_CTL_ADD || op == EPOLL_CTL_MOD) {
        pthread_mutex_lock(&interposers_mutex);
        js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL, NULL);
        const char *dev = NULL;
        int nb_ret = 0;
        if (interposer != NULL) {
            /* Log after unlock so a blocked stderr can't stall other hooked calls. */
            dev = interposer->open_dev_name;
            nb_ret = make_socket_nonblocking(fd);
        }
        pthread_mutex_unlock(&interposers_mutex);
        if (dev != NULL) {
            sji_log_info("epoll_ctl %s for interposed socket fd %d (%s). Ensuring O_NONBLOCK.",
                         (op == EPOLL_CTL_ADD ? "ADD" : "MOD"), fd, dev);
            if (nb_ret == -1) {
                sji_log_warn("epoll_ctl: Failed to ensure O_NONBLOCK for socket fd %d (%s). Epoll behavior might be affected.",
                             fd, dev);
            }
        }
    }
    return real_epoll_ctl(epfd, op, fd, event);
}

/**
 * joydev (JSIOC*) ioctls for a js node, answered from the server config and
 * the hard-coded identity; the map setters are refused with EPERM (the maps
 * come from the server) and anything else is ENOTTY. `interposer` is the
 * dispatcher's snapshot; JSIOCSCORR writes its corr, which ioctl() persists.
 *
 * @return 0, the string length for JSIOCGNAME, or -1 with errno set.
 */
int intercept_js_ioctl(js_interposer_t *interposer, int fd, ioctl_request_t request, void *arg) {
    int len;
    uint8_t *u8_ptr;
    uint16_t *u16_ptr;
    int ret_val = 0;
    (void)fd; /* signature symmetry with the EV handler */
    errno = 0;

    if (_IOC_TYPE(request) != 'j') {
        sji_log_warn("IOCTL_JS(%s): Received non-joystick ioctl 0x%lx (Type '%c', NR 0x%02x) on JS device. Setting ENOTTY.",
                       interposer->open_dev_name, (unsigned long)request, _IOC_TYPE(request), _IOC_NR(request));
        errno = ENOTTY;
        ret_val = -1;
        goto exit_js_ioctl;
    }

    switch (_IOC_NR(request)) {
    case 0x01: /* JSIOCGVERSION */
        if (!arg) { errno = EFAULT; ret_val = -1; break; }
        *((uint32_t *)arg) = JS_VERSION;
        sji_log_info("IOCTL_JS(%s): JSIOCGVERSION -> 0x%08x", interposer->open_dev_name, JS_VERSION);
        break;
    case 0x11: /* JSIOCGAXES */
        if (!arg) { errno = EFAULT; ret_val = -1; break; }
        *((uint8_t *)arg) = interposer->js_config.num_axes;
        sji_log_info("IOCTL_JS(%s): JSIOCGAXES -> %u (from server config)", interposer->open_dev_name, interposer->js_config.num_axes);
        break;
    case 0x12: /* JSIOCGBUTTONS */
        if (!arg) { errno = EFAULT; ret_val = -1; break; }
        *((uint8_t *)arg) = interposer->js_config.num_btns;
        sji_log_info("IOCTL_JS(%s): JSIOCGBUTTONS -> %u (from server config)", interposer->open_dev_name, interposer->js_config.num_btns);
        break;
    case 0x13: /* JSIOCGNAME(len) */
        len = _IOC_SIZE(request);
        if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; break; }
        strncpy((char *)arg, FAKE_UDEV_DEVICE_NAME, len -1 );
        ((char *)arg)[len - 1] = '\0';
        sji_log_info("IOCTL_JS(%s): JSIOCGNAME(%d) -> '%s' (Hardcoded for fake_udev sync)",
                     interposer->open_dev_name, len, FAKE_UDEV_DEVICE_NAME);
        ret_val = strlen((char*)arg);
        break;
    case 0x21: /* JSIOCSCORR */
        if (!arg || _IOC_SIZE(request) != sizeof(js_corr_t)) { errno = EINVAL; ret_val = -1; break; }
        memcpy(&interposer->corr, arg, sizeof(js_corr_t));
        sji_log_info("IOCTL_JS(%s): JSIOCSCORR (noop, correction data stored)", interposer->open_dev_name);
        break;
    case 0x22: /* JSIOCGCORR */
        if (!arg || _IOC_SIZE(request) != sizeof(js_corr_t)) { errno = EINVAL; ret_val = -1; break; }
        memcpy(arg, &interposer->corr, sizeof(js_corr_t));
        sji_log_info("IOCTL_JS(%s): JSIOCGCORR (returned stored data)", interposer->open_dev_name);
        break;
    case 0x31: /* JSIOCSAXMAP */
        sji_log_warn("IOCTL_JS(%s): JSIOCSAXMAP (not supported, config from socket). Setting EPERM.", interposer->open_dev_name);
        errno = EPERM; ret_val = -1; break;
    case 0x32: /* JSIOCGAXMAP */
        if (!arg) { errno = EFAULT; ret_val = -1; break; }
        u8_ptr = (uint8_t *)arg;
        if (_IOC_SIZE(request) < interposer->js_config.num_axes * sizeof(uint8_t) ||
            interposer->js_config.num_axes > INTERPOSER_MAX_AXES) {
            sji_log_error("IOCTL_JS(%s): JSIOCGAXMAP invalid size/count. ReqSize: %u, CfgAxes: %u. Setting EINVAL.",
                          interposer->open_dev_name, _IOC_SIZE(request), interposer->js_config.num_axes);
            errno = EINVAL; ret_val = -1; break;
        }
        memcpy(u8_ptr, interposer->js_config.axes_map, interposer->js_config.num_axes * sizeof(uint8_t));
        sji_log_info("IOCTL_JS(%s): JSIOCGAXMAP (%u axes from server config)", interposer->open_dev_name, interposer->js_config.num_axes);
        break;
    case 0x33: /* JSIOCSBTNMAP */
        sji_log_warn("IOCTL_JS(%s): JSIOCSBTNMAP (not supported, config from socket). Setting EPERM.", interposer->open_dev_name);
        errno = EPERM; ret_val = -1; break;
    case 0x34: /* JSIOCGBTNMAP */
        if (!arg) { errno = EFAULT; ret_val = -1; break; }
        u16_ptr = (uint16_t *)arg;
        if (_IOC_SIZE(request) < interposer->js_config.num_btns * sizeof(uint16_t) ||
            interposer->js_config.num_btns > INTERPOSER_MAX_BTNS) {
            sji_log_error("IOCTL_JS(%s): JSIOCGBTNMAP invalid size/count. ReqSize: %u, CfgBtns: %u. Setting EINVAL.",
                          interposer->open_dev_name, _IOC_SIZE(request), interposer->js_config.num_btns);
            errno = EINVAL; ret_val = -1; break;
        }
        memcpy(u16_ptr, interposer->js_config.btn_map, interposer->js_config.num_btns * sizeof(uint16_t));
        sji_log_info("IOCTL_JS(%s): JSIOCGBTNMAP (%u buttons from server config)", interposer->open_dev_name, interposer->js_config.num_btns);
        break;
    default:
        sji_log_warn("IOCTL_JS(%s): Unhandled joystick ioctl request 0x%lx (NR=0x%02x). Setting ENOTTY.",
                     interposer->open_dev_name, (unsigned long)request, _IOC_NR(request));
        errno = ENOTTY;
        ret_val = -1;
        break;
    }

exit_js_ioctl:
    if (ret_val < 0 && errno == 0) {
        errno = ENOTTY;
    } else if (ret_val >= 0) {
        errno = 0;
    }
    sji_log_debug("IOCTL_JS_RETURN(%s): req=0x%lx, ret_val=%d, errno=%d (%s)",
                 interposer->open_dev_name, (unsigned long)request, ret_val, errno, (errno != 0 ? strerror(errno) : "Success"));
    return ret_val;
}

/**
 * evdev (EVIOC*) ioctls for an event node: identity from the FAKE_UDEV_*
 * values, key and abs capability bits from the server config, fixed absinfo
 * ranges, no input properties, and force feedback accepted as a no-op.
 * `array_idx` is the slot's index in interposers[], from which the pad number
 * for EVIOCGPHYS/EVIOCGUNIQ derives. Anything else, including joydev ioctls,
 * is ENOTTY.
 *
 * @return 0, a string length, the buffer length or an effect id, or -1 with
 *         errno set.
 */
int intercept_ev_ioctl(js_interposer_t *interposer, ptrdiff_t array_idx, int fd, ioctl_request_t request, void *arg) {
    struct input_absinfo *absinfo_ptr;
    struct input_id *id_ptr;
    struct ff_effect *effect_s_ptr;
    int effect_id_val;
    int ev_version = 0x010001;
    int len;
    unsigned int i;
    int ret_val = 0;
    errno = 0;
    (void)fd; /* signature symmetry with the JS handler */

    char ioctl_type = _IOC_TYPE(request);
    unsigned int ioctl_nr = _IOC_NR(request);
    unsigned int ioctl_size = _IOC_SIZE(request);

    if (ioctl_type == 'E') {

        if (ioctl_nr >= _IOC_NR(EVIOCGABS(0)) && ioctl_nr < (_IOC_NR(EVIOCGABS(0)) + ABS_CNT)) {
            uint8_t abs_code = ioctl_nr - _IOC_NR(EVIOCGABS(0));
            if (!arg || ioctl_size < sizeof(struct input_absinfo)) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            absinfo_ptr = (struct input_absinfo *)arg;
            memset(absinfo_ptr, 0, sizeof(struct input_absinfo));

            absinfo_ptr->value = 0;
            absinfo_ptr->minimum = ABS_AXIS_MIN_DEFAULT;
            absinfo_ptr->maximum = ABS_AXIS_MAX_DEFAULT;
            absinfo_ptr->fuzz = 16;
            absinfo_ptr->flat = 128;
            absinfo_ptr->resolution = 1;

            if (abs_code == ABS_X || abs_code == ABS_Y || abs_code == ABS_RX || abs_code == ABS_RY || abs_code == ABS_Z || abs_code == ABS_RZ) {
                absinfo_ptr->minimum = ABS_AXIS_MIN_DEFAULT; 
                absinfo_ptr->maximum = ABS_AXIS_MAX_DEFAULT; 
                absinfo_ptr->fuzz = 16;     
                absinfo_ptr->flat = 128;    
                absinfo_ptr->resolution = 1;
                sji_log_debug("IOCTL_EV(%s): EVIOCGABS(0x%02x) - Main analog stick. min=%d, max=%d, res=%d",
                             interposer->open_dev_name, abs_code, absinfo_ptr->minimum, absinfo_ptr->maximum, absinfo_ptr->resolution);
            } else if (abs_code == ABS_HAT0X || abs_code == ABS_HAT0Y) {
                absinfo_ptr->minimum = ABS_HAT_MIN_DEFAULT;
                absinfo_ptr->maximum = ABS_HAT_MAX_DEFAULT;
                absinfo_ptr->fuzz = 0;
                absinfo_ptr->flat = 0;
                absinfo_ptr->resolution = 0;
                sji_log_debug("IOCTL_EV(%s): EVIOCGABS(0x%02x) - HAT/D-pad axis. min=%d, max=%d, res=%d",
                             interposer->open_dev_name, abs_code, absinfo_ptr->minimum, absinfo_ptr->maximum, absinfo_ptr->resolution);
            } else {
                 sji_log_debug("IOCTL_EV(%s): EVIOCGABS(0x%02x) - Other axis. Using general defaults. min=%d, max=%d, res=%d",
                             interposer->open_dev_name, abs_code, absinfo_ptr->minimum, absinfo_ptr->maximum, absinfo_ptr->resolution);
            }
         
            sji_log_info("IOCTL_EV(%s): EVIOCGABS(0x%02x) -> value=%d, min=%d, max=%d, fuzz=%d, flat=%d, res=%d",
                         interposer->open_dev_name, abs_code,
                         absinfo_ptr->value, absinfo_ptr->minimum, absinfo_ptr->maximum,
                         absinfo_ptr->fuzz, absinfo_ptr->flat, absinfo_ptr->resolution); 
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGNAME(0))) {
            len = ioctl_size;
            if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            strncpy((char *)arg, FAKE_UDEV_DEVICE_NAME, len - 1);
            ((char *)arg)[len - 1] = '\0';
            sji_log_info("IOCTL_EV(%s): EVIOCGNAME(%d) -> '%s' (Hardcoded for fake_udev sync)",
                         interposer->open_dev_name, len, (char *)arg);
            ret_val = strlen((char *)arg);
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGPHYS(0))) {
            len = ioctl_size; 
            if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }

            ptrdiff_t interposer_array_idx = array_idx;
            int gamepad_idx = -1;

            if (interposer_array_idx >= 0 && (size_t)interposer_array_idx < NUM_INTERPOSERS() && interposer->type == DEV_TYPE_EV) {
                gamepad_idx = interposer_array_idx - NUM_JS_INTERPOSERS;
            }
            
            if (gamepad_idx < 0) { 
                sji_log_error("IOCTL_EV(%s): EVIOCGPHYS - Could not determine valid gamepad index (%td, type %d). Setting EINVAL.", 
                              interposer->open_dev_name, interposer_array_idx, interposer->type);
                errno = EINVAL; ret_val = -1; goto exit_ev_ioctl;
            }
            
            snprintf((char *)arg, len, "virtual/input/selkies_ev%d/phys", gamepad_idx);
            ret_val = strlen((char *)arg); 
            
            sji_log_info("IOCTL_EV(%s): EVIOCGPHYS(%d) -> '%s'",
                         interposer->open_dev_name, len, (char *)arg);
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGUNIQ(0))) {
            len = ioctl_size;
            if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }

            ptrdiff_t interposer_array_idx = array_idx;
            int gamepad_idx = -1;

            if (interposer_array_idx >= NUM_JS_INTERPOSERS && (size_t)interposer_array_idx < NUM_INTERPOSERS() && interposer->type == DEV_TYPE_EV) {
                gamepad_idx = interposer_array_idx - NUM_JS_INTERPOSERS;
            }

            if (gamepad_idx != -1) {
                /* Must match the "uniq" sysattr fake-udev publishes for the pad. */
                snprintf((char *)arg, len, "SGVP%04d", gamepad_idx);
            } else {
                sji_log_warn("IOCTL_EV(%s): EVIOCGUNIQ - Could not determine valid gamepad index for unique ID. Using fallback.", interposer->open_dev_name);
                strncpy((char *)arg, "SGVP-UNKNOWN", len -1);
            }
            ((char *)arg)[len - 1] = '\0'; 
            ret_val = strlen((char *)arg); 

            sji_log_info("IOCTL_EV(%s): EVIOCGUNIQ(%d) -> '%s'",
                         interposer->open_dev_name, len, (char *)arg);
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGPROP(0))) {
            len = ioctl_size;
            if (!arg || len <=0 ) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            /* No properties, like a real Xbox 360 pad: any bit here (e.g.
             * INPUT_PROP_POINTING_STICK) makes udev/libinput input_id classify
             * the device as a pointer and SDL2-evdev apps (Xemu) lose the pad. */
            memset(arg, 0, len);
            ret_val = (int)len;
            sji_log_info("IOCTL_EV(%s): EVIOCGPROP(%d) -> no properties (gamepad)", interposer->open_dev_name, len);
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGKEY(0))) {
            len = ioctl_size;
            if (!arg || len <=0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            memset(arg, 0, len);
            sji_log_info("IOCTL_EV(%s): EVIOCGKEY(%d) (all keys reported up)", interposer->open_dev_name, len);
            ret_val = len;
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGLED(0))) {
            len = ioctl_size;
            if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            
            memset(arg, 0, len); 
            
            sji_log_info("IOCTL_EV(%s): EVIOCGLED(%d) (all LEDs reported off)",
                         interposer->open_dev_name, len);
            ret_val = len;
            goto exit_ev_ioctl;
        }

        if (ioctl_nr == _IOC_NR(EVIOCGSW(0))) {
            len = ioctl_size;
            if (!arg || len <= 0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }

            memset(arg, 0, len);

            sji_log_info("IOCTL_EV(%s): EVIOCGSW(%d) (all switches reported off)",
                         interposer->open_dev_name, len);
            ret_val = len;
            goto exit_ev_ioctl;
        }

        if (ioctl_nr >= _IOC_NR(EVIOCGBIT(0,0)) && ioctl_nr < _IOC_NR(EVIOCGBIT(EV_MAX,0))) {
            unsigned char ev_type_query = ioctl_nr - _IOC_NR(EVIOCGBIT(0,0));
            len = ioctl_size;
            if (!arg || len <=0) { errno = EFAULT; ret_val = -1; goto exit_ev_ioctl; }
            memset(arg, 0, len);

            if (ev_type_query == 0) {
                if (EV_SYN / 8 < len) ((unsigned char *)arg)[EV_SYN / 8] |= (1 << (EV_SYN % 8));
                if (EV_KEY / 8 < len) ((unsigned char *)arg)[EV_KEY / 8] |= (1 << (EV_KEY % 8));
                if (EV_ABS / 8 < len) ((unsigned char *)arg)[EV_ABS / 8] |= (1 << (EV_ABS % 8));
                if (EV_FF  / 8 < len) ((unsigned char *)arg)[EV_FF  / 8] |= (1 << (EV_FF  % 8));
                sji_log_info("IOCTL_EV(%s): EVIOCGBIT(type 0x00 - General Caps, len %d) -> EV_SYN, EV_KEY, EV_ABS, EV_FF",
                             interposer->open_dev_name, len);
            } else if (ev_type_query == EV_KEY) {
                sji_log_info("IOCTL_EV(%s): EVIOCGBIT(type 0x%02x - EV_KEY, len %d, num_btns_cfg %u from server) - Argument buffer at %p",
                             interposer->open_dev_name, ev_type_query, len, interposer->js_config.num_btns, arg);
                for (i = 0; i < interposer->js_config.num_btns; ++i) {
                    int key_code = interposer->js_config.btn_map[i]; 
                    if (key_code >= 0 && key_code < KEY_MAX && (key_code / 8 < len)) {
                        ((unsigned char *)arg)[key_code / 8] |= (1 << (key_code % 8));
                        sji_log_debug("IOCTL_EV(%s): EVIOCGBIT(EV_KEY) - Setting bit for key_code 0x%03x (Byte %d, Bit %d)", 
                                     interposer->open_dev_name, key_code, key_code / 8, key_code % 8);
                    } else {
                         sji_log_warn("IOCTL_EV(%s): EVIOCGBIT(EV_KEY) - Skipped invalid/OOB key_code 0x%03x from server config (idx %u).", 
                                      interposer->open_dev_name, key_code, i);
                    }
                }
                if (len > 0 && arg) {
                    char bitmask_preview[128] = {0};
                    int preview_len = (len < 16) ? len : 16;
                    for (int k=0; k < preview_len; ++k) {
                        snprintf(bitmask_preview + strlen(bitmask_preview), sizeof(bitmask_preview) - strlen(bitmask_preview), "%02x ", ((unsigned char*)arg)[k]);
                    }
                    sji_log_debug("IOCTL_EV(%s): EVIOCGBIT(EV_KEY) - Returning bitmask (first %d bytes): %s", 
                                 interposer->open_dev_name, preview_len, bitmask_preview);
                }
                ret_val = len; 
                goto exit_ev_ioctl;

            } else if (ev_type_query == EV_ABS) {
                 sji_log_info("IOCTL_EV(%s): EVIOCGBIT(type 0x%02x - EV_ABS, len %d, num_axes_cfg %u from server) - Argument buffer at %p",
                             interposer->open_dev_name, ev_type_query, len, interposer->js_config.num_axes, arg);
                for (i = 0; i < interposer->js_config.num_axes; ++i) {
                    int abs_code = interposer->js_config.axes_map[i]; 
                     if (abs_code >= 0 && abs_code < ABS_MAX && (abs_code / 8 < len)) {
                        ((unsigned char *)arg)[abs_code / 8] |= (1 << (abs_code % 8));
                        sji_log_debug("IOCTL_EV(%s): EVIOCGBIT(EV_ABS) - Setting bit for abs_code 0x%02x (Byte %d, Bit %d)", 
                                     interposer->open_dev_name, abs_code, abs_code / 8, abs_code % 8);
                     } else {
                        sji_log_warn("IOCTL_EV(%s): EVIOCGBIT(EV_ABS) - Skipped invalid/OOB abs_code 0x%02x from server config (idx %u).", 
                                     interposer->open_dev_name, abs_code, i);
                     }
                }
                if (len > 0 && arg) {
                    char bitmask_preview[128] = {0};
                    int preview_len = (len < 16) ? len : 16;
                    for (int k=0; k < preview_len; ++k) {
                        snprintf(bitmask_preview + strlen(bitmask_preview), sizeof(bitmask_preview) - strlen(bitmask_preview), "%02x ", ((unsigned char*)arg)[k]);
                    }
                    sji_log_debug("IOCTL_EV(%s): EVIOCGBIT(EV_ABS) - Returning bitmask (first %d bytes): %s", 
                                 interposer->open_dev_name, preview_len, bitmask_preview);
                }
                ret_val = len;
                goto exit_ev_ioctl;
            } else if (ev_type_query == EV_FF) {
                sji_log_info("IOCTL_EV(%s): EVIOCGBIT(type 0x%02x - EV_FF, len %d) -> Reporting NO FF capabilities",
                interposer->open_dev_name, ev_type_query, len);
                ret_val = len;
                goto exit_ev_ioctl;
            } else {
                sji_log_info("IOCTL_EV(%s): EVIOCGBIT(type 0x%02x - Other, len %d) -> No bits set",
                             interposer->open_dev_name, ev_type_query, len);
            }
            ret_val = len;
            goto exit_ev_ioctl;
        }

        switch (request) {
            case EVIOCGVERSION:
                if (!arg || ioctl_size < sizeof(int)) { errno = EFAULT; ret_val = -1; break; }
                *((int *)arg) = ev_version;
                sji_log_info("IOCTL_EV(%s): EVIOCGVERSION -> 0x%08x", interposer->open_dev_name, ev_version);
                break;
            case EVIOCGID: 
                if (!arg || ioctl_size < sizeof(struct input_id)) { errno = EFAULT; ret_val = -1; break; }
                id_ptr = (struct input_id *)arg;
                memset(id_ptr, 0, sizeof(struct input_id));
                id_ptr->bustype = FAKE_UDEV_BUS_TYPE;
                id_ptr->vendor  = FAKE_UDEV_VENDOR_ID;
                id_ptr->product = FAKE_UDEV_PRODUCT_ID;
                id_ptr->version = FAKE_UDEV_VERSION_ID;
                sji_log_info("IOCTL_EV(%s): EVIOCGID -> bus:0x%04x, ven:0x%04x, prod:0x%04x, ver:0x%04x (Hardcoded for fake_udev sync)",
                               interposer->open_dev_name, id_ptr->bustype, id_ptr->vendor, id_ptr->product, id_ptr->version);
                break;
            case EVIOCGRAB:
                sji_log_info("IOCTL_EV(%s): EVIOCGRAB (noop, success reported)", interposer->open_dev_name);
                break;
            case EVIOCSFF:
                if (!arg || ioctl_size < sizeof(struct ff_effect)) { errno = EFAULT; ret_val = -1; break; }
                effect_s_ptr = (struct ff_effect *)arg;
                sji_log_info("IOCTL_EV(%s): EVIOCSFF (type: 0x%x, id_in: %d) (noop, returns id)",
                               interposer->open_dev_name, effect_s_ptr->type, effect_s_ptr->id);
                effect_s_ptr->id = (effect_s_ptr->id == -1) ? 1 : effect_s_ptr->id;
                ret_val = effect_s_ptr->id;
                break;
            case EVIOCRMFF:
                effect_id_val = (int)(intptr_t)arg;
                sji_log_info("IOCTL_EV(%s): EVIOCRMFF (id: %d) (noop, success reported)", interposer->open_dev_name, effect_id_val);
                break;
            case EVIOCGEFFECTS:
                if (!arg || ioctl_size < sizeof(int)) { errno = EFAULT; ret_val = -1; break; }
                *(int *)arg = 0;
                sji_log_info("IOCTL_EV(%s): EVIOCGEFFECTS -> %d (Reporting NO FF)", interposer->open_dev_name, *(int *)arg);
                break;
            default:
                sji_log_warn("IOCTL_EV(%s): Unhandled EVDEV ioctl request 0x%lx (Type 'E', NR 0x%02x, Size %u). Setting ENOTTY.",
                               interposer->open_dev_name, (unsigned long)request, ioctl_nr, ioctl_size);
                errno = ENOTTY;
                ret_val = -1;
                break;
        }
    } else if (ioctl_type == 'j') {
        /* A kernel evdev node rejects joydev ioctls with ENOTTY, and SDL relies
         * on that to tell it apart from /dev/input/jsX and pick the VID/PID GUID. */
        sji_log_info("IOCTL_EV(%s): Joystick ioctl 0x%lx (Type 'j', NR 0x%02x) on EVDEV device. Reporting ENOTTY (kernel-faithful).",
                       interposer->open_dev_name, (unsigned long)request, ioctl_nr);
        errno = ENOTTY;
        ret_val = -1;
    } else {
        sji_log_warn("IOCTL_EV(%s): Received ioctl with unexpected type '%c' (request 0x%lx, NR 0x%02x). Setting ENOTTY.",
                       interposer->open_dev_name, ioctl_type, (unsigned long)request, ioctl_nr);
        errno = ENOTTY;
        ret_val = -1;
    }

exit_ev_ioctl:
    if (ret_val < 0 && errno == 0) {
        errno = ENOTTY;
    } else if (ret_val >= 0) {
        errno = 0;
    }
    sji_log_debug("IOCTL_EV_RETURN(%s): req=0x%lx, ret_val=%d, errno=%d (%s)",
                 interposer->open_dev_name, (unsigned long)request, ret_val, errno, (errno != 0 ? strerror(errno) : "Success"));
    return ret_val;
}

/**
 * Interposed ioctl(): routes an interposed fd to the js or ev handler on a
 * snapshot of its slot taken under the lock, so the handler (and its logging)
 * runs unlocked. The one handler write, JSIOCSCORR's corr, is persisted back
 * under the lock only if the fd still owns the same slot: between unlock and
 * re-lock the fd may have been closed and reused for another device's handle,
 * and a same-slot match is still correct because corr is device-global.
 */
int ioctl(int fd, ioctl_request_t request, ...) {
    if (!real_ioctl) {
        sji_log_error("CRITICAL: real_ioctl not loaded. Cannot proceed with ioctl call.");
        errno = EFAULT;
        return -1;
    }

    va_list args_list;
    va_start(args_list, request);
    void *arg_ptr = va_arg(args_list, void *);
    va_end(args_list);

    js_interposer_t *interposer = NULL;
    pthread_mutex_lock(&interposers_mutex);
    interposer = find_interposer_for_fd_locked(fd, NULL, NULL);

    if (interposer == NULL) {
        pthread_mutex_unlock(&interposers_mutex);
        return real_ioctl(fd, request, arg_ptr);
    }

    js_interposer_t snapshot;
    memset(&snapshot, 0, sizeof(snapshot));
    snapshot.type = interposer->type;
    memcpy(snapshot.open_dev_name, interposer->open_dev_name, sizeof(snapshot.open_dev_name));
    snapshot.corr = interposer->corr;
    snapshot.js_config = interposer->js_config;
    ptrdiff_t array_idx = interposer - interposers;
    pthread_mutex_unlock(&interposers_mutex);

    int ioctl_ret;
    errno = 0;
    if (snapshot.type == DEV_TYPE_JS) {
        ioctl_ret = intercept_js_ioctl(&snapshot, fd, request, arg_ptr);
    } else if (snapshot.type == DEV_TYPE_EV) {
        ioctl_ret = intercept_ev_ioctl(&snapshot, array_idx, fd, request, arg_ptr);
    } else {
        sji_log_error("IOCTL(%s): Interposer has unknown type %d for fd %d. This should not happen. Setting EINVAL.",
                       snapshot.open_dev_name, snapshot.type, fd);
        errno = EINVAL;
        return -1;
    }

    if (ioctl_ret >= 0 && _IOC_TYPE(request) == 'j' && _IOC_NR(request) == 0x21) {
        int saved_errno = errno; /* the lookup must not perturb the handler's errno */
        pthread_mutex_lock(&interposers_mutex);
        js_interposer_t *live = find_interposer_for_fd_locked(fd, NULL, NULL);
        if (live != NULL && (live - interposers) == array_idx) {
            live->corr = snapshot.corr;
        } else {
            sji_log_warn("IOCTL(%s): skipping JSIOCSCORR persist-back; fd %d no longer owns the original slot (reuse race).",
                         snapshot.open_dev_name, fd);
        }
        pthread_mutex_unlock(&interposers_mutex);
        errno = saved_errno;
    }
    return ioctl_ret;
}
