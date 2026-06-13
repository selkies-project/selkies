/*
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.
*/

/*
    Selkies Joystick Interposer

    An LD_PRELOAD library to redirect /dev/input/jsX and /dev/input/event*
    device access to corresponding Unix domain sockets. This allows joystick
    input to be piped from another source (e.g., a remote session).
*/

#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE 1
#include <dlfcn.h>
#include <stdio.h>
#include <stdarg.h>
#include <fcntl.h>
#include <string.h>
#include <stdint.h>
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
#include <linux/joystick.h>
#include <linux/input.h>
#include <linux/input-event-codes.h>
#include <pthread.h>

/* We interpose libc entry points whose `pathname` is __nonnull, but a real caller
 * can pass NULL and our `if (pathname)` guards forward it for the real EFAULT.
 * The guards are intentional, so silence the false-positive -Wnonnull-compare. */
#pragma GCC diagnostic ignored "-Wnonnull-compare"

/**
 * @brief Definitions for O_TMPFILE and mode requirement checking.
 *
 * O_TMPFILE allows creating unnamed temporary files, which requires a third
 * 'mode' argument just like O_CREAT. The NEEDS_MODE macro safely identifies
 * if the flags passed to open/openat require extracting this mode argument
 * from the variadic list to prevent creating files with 000 permissions.
 */
#ifndef O_TMPFILE
#define __O_TMPFILE     020000000
#define O_TMPFILE       (__O_TMPFILE | O_DIRECTORY)
#endif
#define NEEDS_MODE(flags) (((flags) & O_CREAT) || (((flags) & O_TMPFILE) == O_TMPFILE))

/**
 * @brief Defines the data type for ioctl request codes.
 *
 * This type is defined as `unsigned long` if `__GLIBC__` is defined,
 * and `int` otherwise, to maintain portability across different C libraries
 * where the underlying type of ioctl requests might vary.
 */
#ifdef __GLIBC__
typedef unsigned long ioctl_request_t;
#else
typedef int ioctl_request_t;
#endif

/**
 * @brief Timeout for socket connection attempts in milliseconds.
 */
#define SOCKET_CONNECT_TIMEOUT_MS 250

/**
 * @brief Maximum time to wait for the full device configuration to arrive on a
 * freshly connected socket, in milliseconds. Prevents a connected-but-silent
 * (stalled or hostile) peer from hanging the application thread that opened the
 * device indefinitely inside the intercepted open()/openat().
 */
#define SOCKET_CONFIG_READ_TIMEOUT_MS 5000

/**
 * @brief Device paths for /dev/input/jsX joystick devices to be interposed.
 */
#define JS0_DEVICE_PATH "/dev/input/js0"
/**
 * @brief Socket paths corresponding to /dev/input/jsX devices.
 */
#define JS0_SOCKET_PATH "/tmp/selkies_js0.sock"
#define JS1_DEVICE_PATH "/dev/input/js1"
#define JS1_SOCKET_PATH "/tmp/selkies_js1.sock"
#define JS2_DEVICE_PATH "/dev/input/js2"
#define JS2_SOCKET_PATH "/tmp/selkies_js2.sock"
#define JS3_DEVICE_PATH "/dev/input/js3"
#define JS3_SOCKET_PATH "/tmp/selkies_js3.sock"
/**
 * @brief Number of /dev/input/jsX devices to interpose.
 */
#define NUM_JS_INTERPOSERS 4

/**
 * @brief Device paths for /dev/input/event* devices to be interposed.
 * High event numbers (e.g., event1000) are used to avoid conflict with real devices.
 */
#define EV0_DEVICE_PATH "/dev/input/event1000"
/**
 * @brief Socket paths corresponding to /dev/input/event* devices.
 */
#define EV0_SOCKET_PATH "/tmp/selkies_event1000.sock"
#define EV1_DEVICE_PATH "/dev/input/event1001"
#define EV1_SOCKET_PATH "/tmp/selkies_event1001.sock"
#define EV2_DEVICE_PATH "/dev/input/event1002"
#define EV2_SOCKET_PATH "/tmp/selkies_event1002.sock"
#define EV3_DEVICE_PATH "/dev/input/event1003"
#define EV3_SOCKET_PATH "/tmp/selkies_event1003.sock"
/**
 * @brief Number of /dev/input/event* devices to interpose.
 */
#define NUM_EV_INTERPOSERS 4

/**
 * @brief Calculates the total number of interposers (js + ev).
 * @return The sum of NUM_JS_INTERPOSERS and NUM_EV_INTERPOSERS.
 */
#define NUM_INTERPOSERS() (NUM_JS_INTERPOSERS + NUM_EV_INTERPOSERS)

/* --- Hardcoded Identity to match fake_udev.c --- */
/**
 * @brief These values are used to respond to ioctl queries for device identity,
 * ensuring consistency with a potential fake udev setup.
 */
#define FAKE_UDEV_DEVICE_NAME "Microsoft X-Box 360 pad"
#define FAKE_UDEV_VENDOR_ID   0x045e
#define FAKE_UDEV_PRODUCT_ID  0x028e
#define FAKE_UDEV_VERSION_ID  0x0114
#define FAKE_UDEV_BUS_TYPE    BUS_USB

/* --- Logging --- */
/**
 * @brief Global flag to control logging.
 * Initialized by sji_logging_init() based on the JS_LOG environment variable.
 * 1 if logging is enabled, 0 otherwise.
 */
static int g_sji_log_enabled = 0;

/**
 * @brief Log level constants for interposer_log.
 */
#define SJI_LOG_LEVEL_DEBUG "[DEBUG]"
#define SJI_LOG_LEVEL_INFO  "[INFO]"
#define SJI_LOG_LEVEL_WARN  "[WARN]"
#define SJI_LOG_LEVEL_ERROR "[ERROR]"

/* --- Real Function Pointers & Loading --- */
/**
 * @brief Pointers to the real libc functions that this library intercepts.
 * These are loaded using dlsym(RTLD_NEXT, ...) during initialization.
 */
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

/**
 * @brief Initializes the logging system.
 *
 * Checks the `JS_LOG` environment variable. If it is set, logging is enabled
 * by setting `g_sji_log_enabled` to 1. This function should be called once
 * at the very start of the library's initialization.
 */
static void sji_logging_init() {
    if (getenv("JS_LOG") != NULL) {
        g_sji_log_enabled = 1;
    }
}

/**
 * @brief Central logging function for the interposer library.
 *
 * If `g_sji_log_enabled` is true and `real_write` has been loaded, this function
 * formats and prints log messages to `STDERR_FILENO`. Messages include a timestamp,
 * log level, source function name, line number, and the provided message.
 *
 * @param level The log level string (e.g., SJI_LOG_LEVEL_DEBUG).
 * @param func_name The name of the function calling the logger (typically `__func__`).
 * @param line_num The line number where the log call occurs (typically `__LINE__`).
 * @param format A printf-style format string for the log message.
 * @param ... Variadic arguments corresponding to the format string.
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

/**
 * @brief Convenience macros for logging at different levels.
 * These macros automatically provide the function name and line number
 * to the `interposer_log` function.
 */
/**
 * @brief Macro for logging debug messages.
 * @param ... Variadic arguments forming the log message, passed to interposer_log.
 */
#define sji_log_debug(...) interposer_log(SJI_LOG_LEVEL_DEBUG, __func__, __LINE__, __VA_ARGS__)
/**
 * @brief Macro for logging informational messages.
 * @param ... Variadic arguments forming the log message, passed to interposer_log.
 */
#define sji_log_info(...)  interposer_log(SJI_LOG_LEVEL_INFO,  __func__, __LINE__, __VA_ARGS__)
/**
 * @brief Macro for logging warning messages.
 * @param ... Variadic arguments forming the log message, passed to interposer_log.
 */
#define sji_log_warn(...)  interposer_log(SJI_LOG_LEVEL_WARN,  __func__, __LINE__, __VA_ARGS__)
/**
 * @brief Macro for logging error messages.
 * @param ... Variadic arguments forming the log message, passed to interposer_log.
 */
#define sji_log_error(...) interposer_log(SJI_LOG_LEVEL_ERROR, __func__, __LINE__, __VA_ARGS__)

/**
 * @brief Loads a real function pointer using `dlsym(RTLD_NEXT, name)`.
 *
 * If the target function pointer is already loaded, the function returns immediately.
 * Otherwise, it attempts to load the function specified by `name`.
 * Errors during `dlsym` are logged.
 *
 * @param target_func_ptr Address of the function pointer variable where the
 *                        address of the loaded function will be stored.
 * @param name The name of the function to load (e.g., "open").
 * @return 0 on success (or if already loaded), -1 if `dlsym` fails.
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

/* --- Data Structures --- */
/**
 * @brief Typedef for joystick correction data.
 * The actual structure `struct js_corr` is defined in `<linux/joystick.h>`
 * and is treated as opaque by this interposer. This typedef is for storing
 * data related to `JSIOCSCORR` and `JSIOCGCORR` ioctls.
 */
typedef struct js_corr js_corr_t;

/**
 * @brief Maximum length for controller name string in `js_config_t`.
 */
#define CONTROLLER_NAME_MAX_LEN 255
/**
 * @brief Maximum number of buttons supported in `js_config_t`.
 */
#define INTERPOSER_MAX_BTNS 512
/**
 * @brief Maximum number of axes supported in `js_config_t`.
 */
#define INTERPOSER_MAX_AXES 64

/**
 * @brief Configuration for a joystick/controller, received from the socket server.
 *
 * This structure holds the configuration details for a joystick or game controller,
 * which is typically sent by a server application over a Unix domain socket.
 * The layout and size of this structure must be identical between the client (this
 * interposer library) and the server to ensure correct data interpretation.
 *
 * Members:
 *  - name: Null-terminated string for the controller's name.
 *  - vendor: USB Vendor ID of the controller.
 *  - product: USB Product ID of the controller.
 *  - version: Device version number.
 *  - num_btns: Number of buttons the controller has.
 *  - num_axes: Number of axes the controller has.
 *  - btn_map: Array mapping logical button indices to evdev key codes.
 *  - axes_map: Array mapping logical axis indices to evdev abs codes.
 *  - final_alignment_padding: Padding to ensure consistent struct size.
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

/**
 * @brief Maximum number of concurrently open application handles per device.
 *
 * Every open() of a device gets its own socket connection, so this bounds the
 * connections per device. Real applications hold one handle (two briefly when
 * an enumeration pass overlaps active use); opens beyond the bound fail with
 * EMFILE.
 */
#define SJI_MAX_HANDLES_PER_DEVICE 16

/**
 * @brief One application open() handle: a dedicated socket connection.
 *
 * Members:
 *  - fd: Connected socket file descriptor returned to the application.
 *  - open_flags: Flags the application passed to open() for this handle.
 */
typedef struct {
    int fd;
    int open_flags;
} sji_handle_t;

/**
 * @brief State for each interposed device.
 *
 * This structure maintains the state associated with each device path
 * (e.g., "/dev/input/js0") that the interposer handles.
 *
 * Each open() handle owns a dedicated socket connection, so every open()
 * returns a unique file descriptor (as POSIX requires), O_NONBLOCK applies
 * per handle, and every handle receives every device event (the server
 * broadcasts events to all connections for a device). close() of one handle
 * never disturbs the others.
 *
 * Members:
 *  - type: Indicates if the device is a joystick (DEV_TYPE_JS) or event (DEV_TYPE_EV) device.
 *  - open_dev_name: The original device path (e.g., "/dev/input/js0").
 *  - socket_path: Path to the Unix domain socket for this device.
 *  - handles: One entry per outstanding open() handle of this device.
 *  - handle_count: Number of valid entries in `handles`. Statically zero, so
 *    fd lookups match nothing before the first open() (even if an intercepted
 *    call runs before the library constructor).
 *  - corr: Stores joystick correction data (for JSIOCSCORR/GCORR ioctls);
 *    device-global, matching the kernel joystick driver's correction state.
 *  - js_config: Device configuration received from the socket server. The
 *    server sends identical content on every connection for a device; each
 *    successful open() refreshes this copy.
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

/**
 * @brief Device type identifiers used in `js_interposer_t`.
 */
#define DEV_TYPE_JS 0 /**< Identifier for joystick devices (/dev/input/jsX). */
#define DEV_TYPE_EV 1 /**< Identifier for event devices (/dev/input/event*). */

/**
 * @brief Default values for `struct input_absinfo` fields in EVIOCGABS ioctl responses.
 * These are used to provide sensible defaults for various axis types.
 */
#define ABS_AXIS_MIN_DEFAULT -32767
#define ABS_AXIS_MAX_DEFAULT 32767
#define ABS_HAT_MIN_DEFAULT -1
#define ABS_HAT_MAX_DEFAULT 1

/**
 * @brief Array holding the state for all configured interposers.
 * This array is initialized with predefined device paths and socket paths
 * for both joystick (`jsX`) and event (`event*`) devices.
 */
static js_interposer_t interposers[NUM_INTERPOSERS()] = {
    /* Remaining members are zero-initialized; handle_count 0 means no open
     * handles, so the handle tables start empty. */
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
 * @brief Mutex protecting concurrent access to the global interposers[] array.
 *
 * LD_PRELOAD libraries run inside multithreaded hosts (e.g. SDL runs joystick
 * handling on its own thread), so the open/close mutation paths and the fd
 * lookups must be serialized to avoid torn js_config and use of a handle
 * another thread is tearing down. The lock guards only the brief array
 * lookups and state transitions; it is intentionally NOT held across blocking
 * socket I/O — neither the recv() on the event path (read()) nor the
 * connect/config-read on the open path. Each open() builds its connection on
 * a private fd without the lock and only publishes it into the handle table
 * under the lock once fully configured, so lookups never observe a
 * half-initialized handle.
 */
static pthread_mutex_t interposers_mutex = PTHREAD_MUTEX_INITIALIZER;

/**
 * @brief Finds the interposer slot owning an application file descriptor.
 *
 * Must be called with `interposers_mutex` held. Every fd handed to the
 * application by an interposed open() is registered in exactly one slot's
 * handle table until the matching close().
 *
 * @param fd The application file descriptor to look up.
 * @param open_flags_out Optional output; receives the open() flags recorded
 *                       for the matching handle.
 * @return Pointer to the owning slot, or NULL if `fd` is not interposed.
 */
static js_interposer_t *find_interposer_for_fd_locked(int fd, int *open_flags_out) {
    if (fd < 0) {
        return NULL;
    }
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        for (int h = 0; h < interposers[i].handle_count; h++) {
            if (interposers[i].handles[h].fd == fd) {
                if (open_flags_out != NULL) {
                    *open_flags_out = interposers[i].handles[h].open_flags;
                }
                return &interposers[i];
            }
        }
    }
    return NULL;
}

/* Library constructor: init logging and load pointers to the real libc functions we intercept. */
__attribute__((constructor)) void init_interposer() {
    sji_logging_init();

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
    sji_log_info("Selkies Joystick Interposer initialized. Logging is %s.", g_sji_log_enabled ? "ENABLED" : "DISABLED");
}

/**
 * @brief Sets a socket file descriptor to non-blocking mode.
 *
 * Retrieves the current flags of the socket, and if `O_NONBLOCK` is not set,
 * attempts to add it using `fcntl`.
 *
 * @param sockfd The socket file descriptor to make non-blocking.
 * @return 0 on success or if already non-blocking, -1 on failure (e.g., `fcntl` error).
 */
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
 * @brief Intercepted `access()` system call.
 *
 * If the `pathname` matches one of the device paths configured for interposition
 * (e.g., "/dev/input/js0"), this function will always return 0 (success),
 * effectively making these virtual devices appear accessible.
 * For any other `pathname`, the call is passed through to the real `access()` function.
 *
 * @param pathname The path to the file whose accessibility is to be checked.
 * @param mode The accessibility checks to be performed (e.g., `R_OK`, `W_OK`).
 * @return 0 if `pathname` is an interposed device path or if the real `access()`
 *         call succeeds for other paths. -1 on error (errno is set by the real
 *         `access()` or if `real_access` is not loaded).
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

/**
 * @brief Helper to populate a stat structure with fake device IDs.
 *
 * SDL uses the st_rdev field (device ID) to check for duplicates.
 * Since our sockets are just unix sockets, they usually return 0 or a generic ID.
 * We must forge unique IDs (Major 13 for Input) matching the virtual path indices.
 */
static void fill_fake_stat(const char* path, struct stat *buf) {
    buf->st_mode = S_IFCHR | 0666;
    
    int dev_num = -1;
    
    if (sscanf(path, "/dev/input/event%d", &dev_num) == 1) {
        buf->st_rdev = makedev(13, dev_num);
    } else if (sscanf(path, "/dev/input/js%d", &dev_num) == 1) {
        buf->st_rdev = makedev(13, dev_num);
    } else {
        buf->st_rdev = makedev(13, 9999); 
    }
    
    buf->st_uid = 0;
    buf->st_gid = 0;
    buf->st_size = 0;
    buf->st_blksize = 4096;
    buf->st_blocks = 0;
    buf->st_nlink = 1;
}

/**
 * @brief Intercepted `fstat()` system call.
 */
int fstat(int fd, struct stat *buf) {
    if (!real_fstat) {
         if (load_real_func((void *)&real_fstat, "fstat") < 0) {
             errno = EFAULT;
             return -1;
         }
    }

    pthread_mutex_lock(&interposers_mutex);
    js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL);
    if (interposer != NULL) {
        memset(buf, 0, sizeof(struct stat));
        fill_fake_stat(interposer->open_dev_name, buf);
        /* Snapshot the device name (static string), then log after unlock so a blocked stderr can't stall other hooked calls. */
        const char *dev = interposer->open_dev_name;
        pthread_mutex_unlock(&interposers_mutex);
        sji_log_debug("Intercepted fstat for fd %d (%s), returning fake rdev %d:%d",
            fd, dev, major(buf->st_rdev), minor(buf->st_rdev));
        return 0;
    }
    pthread_mutex_unlock(&interposers_mutex);
    return real_fstat(fd, buf);
}

/**
 * @brief Intercepted `stat()` system call.
 */
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

/**
 * @brief Intercepted `lstat()` system call.
 */
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

/**
 * @brief Reads the joystick configuration (`js_config_t`) from a connected socket.
 *
 * This function attempts to read `sizeof(js_config_t)` bytes from the provided
 * socket file descriptor into the `config_dest` buffer. If the socket is
 * non-blocking, it is temporarily set to blocking for this read operation and
 * restored afterwards.
 *
 * @param sockfd The file descriptor of the connected socket from which to read.
 * @param config_dest Pointer to a `js_config_t` structure to store the read configuration.
 * @return 0 on successful read of the complete configuration, -1 on failure
 *         (e.g., read error, EOF, timeout). `errno` may be set by underlying calls.
 */
static int read_socket_config(int sockfd, js_config_t *config_dest) {
    ssize_t bytes_to_read = sizeof(js_config_t);
    ssize_t bytes_read_total = 0;
    char *buffer_ptr = (char *)config_dest;
    int original_socket_flags = fcntl(sockfd, F_GETFL, 0);
    int socket_was_nonblocking = 0;

    /* Bound the total time spent waiting for the config so a connected-but-silent
     * peer cannot hang the calling application thread forever. SO_RCVTIMEO makes
     * an otherwise-blocking real_read return EAGAIN periodically; the monotonic
     * deadline below caps the cumulative wait across retries. */
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

    /* Clamp the button/axis counts to the static array bounds. These values come
     * straight from the socket peer and are otherwise trusted verbatim; without
     * this, an oversized num_btns/num_axes drives out-of-bounds reads of
     * btn_map/axes_map in the EVIOCGBIT handlers (and any other count-keyed loop). */
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
 * @brief Connects to the Unix domain socket backing an interposed device.
 *
 * This function creates a new socket, attempts to connect to the Unix domain
 * socket at `socket_path` with a timeout. Upon successful connection, it reads
 * the device configuration into `config_dest` using `read_socket_config()` and
 * sends a 1-byte architecture specifier (sizeof(long)) to the server.
 *
 * It deliberately operates on locals/out-params only — never on the shared
 * interposers[] slot — so it can run without `interposers_mutex` held while
 * other threads scan the array; the caller publishes the returned fd and
 * config into the slot under the lock once fully configured.
 *
 * @param socket_path Path of the Unix domain socket to connect to.
 * @param config_dest Receives the device configuration on success.
 * @return The connected socket fd on success, -1 on failure.
 *         `errno` may be set by underlying system calls.
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

/* Shared open()/open64() interposition. Each open of a matched device gets its OWN
 * socket connection (unique fd per POSIX, per-handle O_NONBLOCK, every handle gets
 * every event). connect_interposer_socket() runs WITHOUT interposers_mutex (it can
 * block on timeouts and would stall every other interposed call); the fd stays
 * private to this thread until published under the lock.
 * Returns the socket fd; -1 on error (errno EIO on connect fail, EMFILE at
 * SJI_MAX_HANDLES_PER_DEVICE); -2 if not an interposable path (caller uses real open). */
static int common_open_logic(const char *pathname, int flags, js_interposer_t **found_interposer_ptr) {
    *found_interposer_ptr = NULL;

    if (pathname == NULL) {
        return -2;  /* let the real open*() set errno=EFAULT for a NULL path */
    }

    /* Match the slot by device name without the lock: the name fields are set
     * once at static initialization and never mutated. */
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

    /* Blocking connect + config read on a private fd, deliberately WITHOUT
     * the global lock. */
    js_config_t pending_config;
    memset(&pending_config, 0, sizeof(pending_config));
    int new_fd = connect_interposer_socket(interposer->socket_path, &pending_config);
    if (new_fd == -1) {
        sji_log_error("Failed to establish socket connection for %s.", pathname);
        errno = EIO;
        return -1;
    }

    if (flags & O_NONBLOCK) {
        /* The fd is still private to this thread; set it up before publishing. */
        sji_log_info("Application opened %s with O_NONBLOCK. Setting socket fd %d to non-blocking.",
                     pathname, new_fd);
        if (make_socket_nonblocking(new_fd) == -1) {
            sji_log_warn("Failed to make socket fd %d non-blocking for %s as requested by app. Socket may remain blocking.",
                          new_fd, pathname);
        }
    }

    /* Publish the fully configured connection (atomic from the perspective of
     * every lock-holding scanner). */
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
    /* The server sends the same per-device config on every connection;
     * last-write-wins keeps the slot's cached copy current. */
    interposer->js_config = pending_config;
    int open_handles = interposer->handle_count;
    pthread_mutex_unlock(&interposers_mutex);

    /* Gate the fcntl so the success path performs no extra syscall and leaves errno untouched when logging is off. */
    int sock_flags = g_sji_log_enabled ? fcntl(new_fd, F_GETFL, 0) : 0;
    sji_log_info("Successfully interposed 'open' for %s (app_flags=0x%x), socket_fd: %d (%d handle(s) open). Socket flags: 0x%x",
                 pathname, flags, new_fd, open_handles, sock_flags);
    return new_fd;
}

/**
 * @brief Intercepted `open()` system call.
 *
 * If `real_open` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * Otherwise, it calls `common_open_logic()` to determine if the `pathname`
 * corresponds to a device that should be interposed.
 * If `common_open_logic()` returns:
 *  - A non-negative fd: This fd (representing the socket) is returned to the application.
 *  - -1: An error occurred during interposition; -1 is returned and `errno` is already set.
 *  - -2: The path is not an interposable device; the call is passed to `real_open()`.
 *
 * @param pathname The path to the file to open.
 * @param flags Flags for opening the file (e.g., `O_RDONLY`, `O_NONBLOCK`).
 * @param ... Optional `mode_t mode` argument if `O_CREAT` is in `flags`.
 * @return A file descriptor on success, or -1 on error (`errno` is set).
 */
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

/**
 * @brief Intercepted `open64()` system call.
 *
 * Similar to the intercepted `open()`, this function uses `common_open_logic()`
 * to handle interposition for target device paths. If the path is not
 * interposable, the call is passed to `real_open64()` if available, or
 * falls back to `real_open()` otherwise.
 * If neither `real_open64` nor `real_open` are loaded, returns -1 with `errno`
 * set to `EFAULT`.
 *
 * @param pathname The path to the file to open.
 * @param flags Flags for opening the file.
 * @param ... Optional `mode_t mode` argument if `O_CREAT` is in `flags`.
 * @return A file descriptor on success, or -1 on error (`errno` is set).
 */
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

/**
 * @brief Intercepted `openat()` system call.
 *
 * Resolves the full path if a relative path and directory fd are provided.
 * Uses `common_open_logic()` to handle interposition for target device paths.
 * Safely extracts and passes the `mode` argument if file creation flags
 * (O_CREAT or O_TMPFILE) are present to prevent permission bugs.
 *
 * @param dirfd The directory file descriptor.
 * @param pathname The path to the file to open.
 * @param flags Flags for opening the file.
 * @param ... Optional `mode_t mode` argument if file creation flags are set.
 * @return A file descriptor on success, or -1 on error (`errno` is set).
 */
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

/**
 * @brief Intercepted `openat64()` system call.
 *
 * 64-bit variant of the intercepted `openat()` system call. Resolves relative
 * paths, applies interposer logic, and safely handles variadic `mode` arguments.
 * Falls back to `real_openat()` if `real_openat64` is not available.
 *
 * @param dirfd The directory file descriptor.
 * @param pathname The path to the file to open.
 * @param flags Flags for opening the file.
 * @param ... Optional `mode_t mode` argument if file creation flags are set.
 * @return A file descriptor on success, or -1 on error (`errno` is set).
 */
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

/**
 * @brief Intercepted `close()` system call.
 *
 * If `real_close` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * Checks if the given file descriptor `fd` is a handle created by an
 * interposed open(). If it is, the handle is removed from its device's table
 * and its dedicated socket connection is closed via `real_close()`; other
 * handles for the same device own their own connections and are unaffected.
 * When the last handle of a device closes, the cached device config is
 * cleared.
 * If `fd` is not an interposed handle, the call is passed to `real_close()`.
 *
 * @param fd The file descriptor to close.
 * @return 0 on success, -1 on error (`errno` is set by `real_close()`).
 */
int close(int fd) {
    if (!real_close) {
        sji_log_error("CRITICAL: real_close not loaded. Cannot proceed with close call.");
        errno = EFAULT;
        return -1;
    }

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
                /* Last handle for this device is gone; drop the cached config. */
                memset(&(interposer->js_config), 0, sizeof(js_config_t));
            }
            int ret = real_close(fd);
            int close_errno = errno;
            /* Snapshot under the lock, then log outside it so a blocked stderr can't stall other hooked calls. */
            const char *dev = interposer->open_dev_name;  /* static string constant, safe after unlock */
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
 * @brief Intercepted `read()` system call.
 *
 * If `real_read` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * Checks if `fd` is an interposed socket. If not, passes to `real_read()`.
 * If it is an interposed socket:
 *  - Determines the expected event size (`struct js_event` or `struct input_event`).
 *  - If `count` is 0, returns 0.
 *  - If `count` is less than one event size, returns -1 with `errno` set to `EINVAL`.
 *  - Attempts to `recv()` one event from the socket.
 *  - Handles non-blocking behavior (`EAGAIN`/`EWOULDBLOCK`).
 *
 * @param fd The file descriptor to read from.
 * @param buf Buffer to store the read data.
 * @param count Maximum number of bytes to read.
 * @return Number of bytes read on success. 0 on EOF. -1 on error (`errno` is set).
 */
ssize_t read(int fd, void *buf, size_t count) {
    if (!real_read) {
        sji_log_error("CRITICAL: real_read not loaded. Cannot proceed with read call.");
        errno = EFAULT;
        return -1;
    }

    js_interposer_t *interposer = NULL;
    int handle_open_flags = 0;
    pthread_mutex_lock(&interposers_mutex);
    interposer = find_interposer_for_fd_locked(fd, &handle_open_flags);
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

    /* recv() on the caller's fd: each handle owns its own connection, so this
     * reads exactly this handle's event stream. After the unlocked lookup a
     * concurrent close() can retire the handle; the caller's fd keeps kernel
     * read() semantics (EBADF at worst). */
    int socket_actual_flags = fcntl(fd, F_GETFL, 0);
    int socket_is_actually_nonblocking = (socket_actual_flags != -1 && (socket_actual_flags & O_NONBLOCK));

    if (socket_actual_flags == -1) {
        sji_log_warn("read: fcntl(F_GETFL) failed for sockfd %d (%s): %s. Proceeding, assuming blocking status based on this handle's open() flags.",
                     fd, interposer->open_dev_name, strerror(errno));
        socket_is_actually_nonblocking = (handle_open_flags & O_NONBLOCK);
    }

    ssize_t bytes_read;
    if (socket_is_actually_nonblocking) {
        /* Non-blocking: never consume a partial event. Peek first and only
         * dequeue once a whole event is buffered; consuming a partial event
         * would permanently desync the SOCK_STREAM for all later reads. */
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
            /* Peek and consuming recv() aren't atomic; a partial consume must
             * finish draining the event or the stream desyncs. Bounded so a
             * stalled peer cannot hang the caller. */
            bytes_read = recv(fd, buf, event_size, MSG_DONTWAIT);
            if (bytes_read > 0 && (size_t)bytes_read < event_size) {
                size_t event_consumed = (size_t)bytes_read;
                /* Drain budget for the partial event: bounded poll() (not a spin)
                 * caps the stall at ~10ms if the peer stalls mid-event. */
                const int drain_budget_ms = 10;
                struct timespec drain_start;
                clock_gettime(CLOCK_MONOTONIC, &drain_start);
                while (event_consumed < event_size) {
                    ssize_t tail = recv(fd, (char *)buf + event_consumed,
                                        event_size - event_consumed, MSG_DONTWAIT);
                    if (tail > 0) {
                        event_consumed += (size_t)tail;
                        continue;
                    }
                    if (tail == 0) {
                        break; /* EOF mid-event; surface the short count below. */
                    }
                    if (errno != EAGAIN && errno != EWOULDBLOCK) {
                        break; /* hard error */
                    }
                    /* Remainder not buffered yet; wait (efficiently) for the
                     * rest, but only for the time left in the budget. */
                    struct timespec drain_now;
                    clock_gettime(CLOCK_MONOTONIC, &drain_now);
                    long elapsed_ms = (drain_now.tv_sec - drain_start.tv_sec) * 1000L +
                                      (drain_now.tv_nsec - drain_start.tv_nsec) / 1000000L;
                    int remaining_ms = drain_budget_ms - (int)elapsed_ms;
                    if (remaining_ms <= 0) {
                        break; /* drain budget exhausted */
                    }
                    struct pollfd pfd = { .fd = fd, .events = POLLIN, .revents = 0 };
                    int prc = poll(&pfd, 1, remaining_ms);
                    if (prc <= 0) {
                        /* timeout (0) or poll error/EINTR (<0): stop draining
                         * and surface the short count below. */
                        break;
                    }
                    /* Readable (or POLLHUP/POLLERR): loop and let recv() report
                     * the new bytes, EOF, or the hard error. */
                }
                bytes_read = (ssize_t)event_consumed;
            }
        }
    } else {
        /* Blocking: wait for a whole event so a short read cannot desync the
         * stream. MSG_WAITALL only returns short on EOF or an interrupting signal. */
        bytes_read = recv(fd, buf, event_size, MSG_WAITALL);
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

/**
 * @brief Intercepted `epoll_ctl()` system call.
 *
 * If `real_epoll_ctl` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * If the operation is `EPOLL_CTL_ADD` or `EPOLL_CTL_MOD` and `fd` is one
 * of the interposed socket file descriptors, this function ensures that the
 * underlying socket is set to non-blocking mode using `make_socket_nonblocking()`.
 * This is important because `epoll` is typically used with non-blocking FDs.
 * After this potential modification, the call is passed to `real_epoll_ctl()`.
 *
 * @param epfd The epoll instance file descriptor.
 * @param op The operation to perform (e.g., `EPOLL_CTL_ADD`, `EPOLL_CTL_MOD`, `EPOLL_CTL_DEL`).
 * @param fd The file descriptor to add/modify/remove from the epoll instance.
 * @param event Pointer to an `epoll_event` structure describing the event.
 * @return 0 on success, -1 on error (`errno` is set by `real_epoll_ctl()`).
 */
int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event) {
    if (!real_epoll_ctl) {
        sji_log_error("CRITICAL: real_epoll_ctl not loaded. Cannot proceed with epoll_ctl call.");
        errno = EFAULT;
        return -1;
    }

    if (op == EPOLL_CTL_ADD || op == EPOLL_CTL_MOD) {
        pthread_mutex_lock(&interposers_mutex);
        js_interposer_t *interposer = find_interposer_for_fd_locked(fd, NULL);
        const char *dev = NULL;
        int nb_ret = 0;
        if (interposer != NULL) {
            /* Snapshot the device name (static string) and flip O_NONBLOCK under the lock;
             * defer all logging until after unlock so a blocked stderr can't stall other hooked calls. */
            dev = interposer->open_dev_name;
            /* Each handle owns its own connection, so this flips only the
             * caller's handle to non-blocking, not other handles of the device. */
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

/* --- IOCTL Handling --- */

/**
 * @brief Handles ioctl calls for interposed joystick devices (DEV_TYPE_JS).
 *
 * This function processes ioctl requests specific to joystick devices
 * (`/dev/input/jsX`). It emulates the behavior of a standard joystick driver
 * for supported ioctl commands, using configuration data received from the
 * socket server where appropriate (e.g., for number of axes/buttons, mappings).
 * Unsupported ioctls typically result in `ENOTTY` or `EPERM`.
 *
 * @param interposer Pointer to the `js_interposer_t` state for the device.
 * @param fd The application's file descriptor, which is our socket fd.
 * @param request The ioctl request code.
 * @param arg Pointer to the argument for the ioctl request.
 * @return 0 on success, or a positive value if the ioctl returns data (e.g., string length).
 *         -1 on error (`errno` is set appropriately).
 */
int intercept_js_ioctl(js_interposer_t *interposer, int fd, ioctl_request_t request, void *arg) {
    int len;
    uint8_t *u8_ptr;
    uint16_t *u16_ptr;
    int ret_val = 0;
    (void)fd; /* fd is part of the handler signature for symmetry with the EV
               * handler; this handler operates on *interposer, not the fd. */
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
 * @brief Handles ioctl calls for interposed event devices (DEV_TYPE_EV).
 *
 * This function processes ioctl requests specific to evdev input devices
 * (`/dev/input/event*`). It emulates responses for common evdev ioctls like
 * `EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGBIT` (for capabilities),
 * `EVIOCGABS` (for absolute axis info), and basic force feedback ioctls.
 * Device identity (name, IDs) is hardcoded to match `FAKE_UDEV_*` defines.
 * Capabilities (buttons, axes) are derived from `interposer->js_config`.
 * Unsupported ioctls typically result in `ENOTTY`.
 *
 * @param interposer Pointer to the `js_interposer_t` state for the device.
 * @param fd The application's file descriptor, which is our socket fd.
 * @param request The ioctl request code.
 * @param arg Pointer to the argument for the ioctl request.
 * @return 0 on success, or a positive value if the ioctl returns data (e.g., string length or effect ID).
 *         -1 on error (`errno` is set appropriately).
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
                snprintf((char *)arg, len, "SJI-EV%d", gamepad_idx);
            } else {
                sji_log_warn("IOCTL_EV(%s): EVIOCGUNIQ - Could not determine valid gamepad index for unique ID. Using fallback.", interposer->open_dev_name);
                strncpy((char *)arg, "SJI-EV-UNKNOWN", len -1);
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
            memset(arg, 0, len);

            if (INPUT_PROP_POINTING_STICK / 8 < (unsigned int)len) {
                ((unsigned char *)arg)[INPUT_PROP_POINTING_STICK / 8] |= (1 << (INPUT_PROP_POINTING_STICK % 8));
                sji_log_info("IOCTL_EV(%s): EVIOCGPROP(%d) - Added INPUT_PROP_POINTING_STICK", interposer->open_dev_name, len);
            } else {
                sji_log_warn("IOCTL_EV(%s): EVIOCGPROP(%d) - Buffer too small for INPUT_PROP_POINTING_STICK", interposer->open_dev_name, len);
            }
            ret_val = 0;
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
        sji_log_info("IOCTL_EV_COMPAT(%s): Joystick ioctl 0x%lx (Type 'j', NR 0x%02x) on EVDEV device. Delegating to JS handler.",
                       interposer->open_dev_name, (unsigned long)request, ioctl_nr);
        return intercept_js_ioctl(interposer, fd, request, arg);
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
 * @brief Intercepted `ioctl()` system call.
 *
 * If `real_ioctl` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * Checks if the file descriptor `fd` corresponds to an interposed device.
 * If it is not an interposed fd, the call is passed to `real_ioctl()`.
 * If it is an interposed fd, the call is routed to either `intercept_js_ioctl()`
 * or `intercept_ev_ioctl()` based on the `interposer->type`.
 *
 * @param fd The file descriptor on which the ioctl operation is to be performed.
 * @param request The device-dependent ioctl request code.
 * @param ... A third argument, typically a pointer (`void *arg`), whose type
 *            depends on the specific ioctl request.
 * @return On success, the return value depends on the specific ioctl command.
 *         On error, -1 is returned, and `errno` is set appropriately by the
 *         specific ioctl handler or by `real_ioctl()`.
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
    interposer = find_interposer_for_fd_locked(fd, NULL);

    if (interposer == NULL) {
        pthread_mutex_unlock(&interposers_mutex);
        return real_ioctl(fd, request, arg_ptr);
    }

    /* Snapshot the fields the handlers read under the lock, then run the handler
     * unlocked so blocking logging can't stall other hooked calls. interposers[]
     * is static, so the live slot stays valid; JSIOCSCORR is persisted back below. */
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
        /* The EV handler delegates 'j'-type ioctls (incl. JSIOCSCORR) to the
         * JS handler, which writes snapshot.corr just like the JS path. */
        ioctl_ret = intercept_ev_ioctl(&snapshot, array_idx, fd, request, arg_ptr);
    } else {
        sji_log_error("IOCTL(%s): Interposer has unknown type %d for fd %d. This should not happen. Setting EINVAL.",
                       snapshot.open_dev_name, snapshot.type, fd);
        errno = EINVAL;
        return -1;
    }

    /* JSIOCSCORR is the only handler write: persist snapshot.corr back to the live
     * slot (re-acquire the lock, re-validate the fd still owns it). Save/restore
     * errno so the lock/lookup can't perturb the handler's reported errno. */
    if (ioctl_ret >= 0 && _IOC_TYPE(request) == 'j' && _IOC_NR(request) == 0x21) {
        int saved_errno = errno;
        pthread_mutex_lock(&interposers_mutex);
        js_interposer_t *live = find_interposer_for_fd_locked(fd, NULL);
        if (live != NULL) {
            live->corr = snapshot.corr;
        }
        pthread_mutex_unlock(&interposers_mutex);
        errno = saved_errno;
    }
    return ioctl_ret;
}
