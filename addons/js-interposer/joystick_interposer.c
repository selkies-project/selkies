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
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <linux/joystick.h>
#include <linux/input.h>
#include <linux/input-event-codes.h>

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
    int current_pos = 0;
    ssize_t written_bytes_count;
    int printed_len;

    printed_len = snprintf(buffer + current_pos, sizeof(buffer) - current_pos, "[%lu]", (unsigned long)time(NULL));
    if (printed_len > 0) {
        current_pos += (printed_len < (sizeof(buffer) - current_pos)) ? printed_len : (sizeof(buffer) - current_pos -1);
    }

    if (current_pos < sizeof(buffer) -1) {
        printed_len = snprintf(buffer + current_pos, sizeof(buffer) - current_pos,
                                "[SJI]%s[%s:%d] ", level, func_name, line_num);
        if (printed_len > 0) {
            current_pos += (printed_len < (sizeof(buffer) - current_pos)) ? printed_len : (sizeof(buffer) - current_pos -1);
        }
    }

    if (current_pos < sizeof(buffer) -1) {
        va_list argp;
        va_start(argp, format);
        printed_len = vsnprintf(buffer + current_pos, sizeof(buffer) - current_pos, format, argp);
        va_end(argp);
        if (printed_len > 0) {
            current_pos += (printed_len < (sizeof(buffer) - current_pos)) ? printed_len : (sizeof(buffer) - current_pos -1);
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
 * @brief State for each interposed device.
 *
 * This structure maintains the state associated with each device path
 * (e.g., "/dev/input/js0") that the interposer handles.
 *
 * Members:
 *  - type: Indicates if the device is a joystick (DEV_TYPE_JS) or event (DEV_TYPE_EV) device.
 *  - open_dev_name: The original device path (e.g., "/dev/input/js0").
 *  - socket_path: Path to the Unix domain socket for this device.
 *  - sockfd: File descriptor for the connected Unix domain socket; -1 if not connected.
 *  - open_flags: Flags used by the application when opening the device via `open()`.
 *  - corr: Stores joystick correction data (for JSIOCSCORR/GCORR ioctls).
 *  - js_config: Device configuration received from the socket server.
 */
typedef struct {
    uint8_t type;
    char open_dev_name[255];
    char socket_path[255];
    int sockfd;
    int open_flags;
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
#define ABS_TRIGGER_MIN_DEFAULT 0
#define ABS_TRIGGER_MAX_DEFAULT 255
#define ABS_HAT_MIN_DEFAULT -1
#define ABS_HAT_MAX_DEFAULT 1

/**
 * @brief Array holding the state for all configured interposers.
 * This array is initialized with predefined device paths and socket paths
 * for both joystick (`jsX`) and event (`event*`) devices.
 */
static js_interposer_t interposers[NUM_INTERPOSERS()] = {
    { DEV_TYPE_JS, JS0_DEVICE_PATH, JS0_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_JS, JS1_DEVICE_PATH, JS1_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_JS, JS2_DEVICE_PATH, JS2_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_JS, JS3_DEVICE_PATH, JS3_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_EV, EV0_DEVICE_PATH, EV0_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_EV, EV1_DEVICE_PATH, EV1_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_EV, EV2_DEVICE_PATH, EV2_SOCKET_PATH, -1, 0, {0}, {0} },
    { DEV_TYPE_EV, EV3_DEVICE_PATH, EV3_SOCKET_PATH, -1, 0, {0}, {0} },
};

/**
 * @brief Library constructor function, called when the library is loaded.
 *
 * This function performs essential one-time initialization:
 * 1. Initializes the logging system via `sji_logging_init()`.
 * 2. Loads pointers to the real libc functions that will be intercepted
 *    (e.g., `open`, `ioctl`, `read`, `close`, `epoll_ctl`, `write`, `access`).
 *    It also attempts to load `open64` if available.
 * Critical failures during function loading are logged.
 */
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

    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (interposers[i].sockfd != -1 && interposers[i].sockfd == fd) {
            memset(buf, 0, sizeof(struct stat));
            fill_fake_stat(interposers[i].open_dev_name, buf);
            
            sji_log_debug("Intercepted fstat for fd %d (%s), returning fake rdev %d:%d", 
                fd, interposers[i].open_dev_name, major(buf->st_rdev), minor(buf->st_rdev));
            return 0;
        }
    }
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
                sji_log_warn("read_socket_config: real_read on sockfd %d returned EAGAIN/EWOULDBLOCK. Retrying after short delay.", sockfd);
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

    sji_log_info("Successfully read joystick config from sockfd %d: Name='%s', Vnd=0x%04x, Prd=0x%04x, Ver=0x%04x, Btns=%u, Axes=%u",
                 sockfd, config_dest->name, config_dest->vendor, config_dest->product, config_dest->version,
                 config_dest->num_btns, config_dest->num_axes);

    if (strnlen(config_dest->name, CONTROLLER_NAME_MAX_LEN) == CONTROLLER_NAME_MAX_LEN) {
        config_dest->name[CONTROLLER_NAME_MAX_LEN-1] = '\0';
        sji_log_warn("Config name from server was not null-terminated within max length; forced termination.");
    }

config_read_cleanup:
    if (socket_was_nonblocking && original_socket_flags != -1) {
        sji_log_debug("read_socket_config: Restoring O_NONBLOCK to sockfd %d.", sockfd);
        if (fcntl(sockfd, F_SETFL, original_socket_flags) == -1) {
            sji_log_warn("read_socket_config: Failed to restore O_NONBLOCK to sockfd %d: %s", sockfd, strerror(errno));
        }
    }
    return (bytes_read_total == bytes_to_read) ? 0 : -1;
}

/**
 * @brief Connects an interposer to its corresponding Unix domain socket.
 *
 * This function creates a new socket, attempts to connect to the Unix domain
 * socket specified in `interposer->socket_path` with a timeout. Upon successful
 * connection, it reads the device configuration using `read_socket_config()`
 * and sends a 1-byte architecture specifier (sizeof(long)) to the server.
 *
 * @param interposer Pointer to the `js_interposer_t` state for the device.
 *                   The `sockfd` and `js_config` members will be updated.
 * @return 0 on successful connection and configuration, -1 on failure.
 *         `errno` may be set by underlying system calls.
 */
static int connect_interposer_socket(js_interposer_t *interposer) {
    interposer->sockfd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (interposer->sockfd == -1) {
        sji_log_error("Failed to create socket for %s: %s", interposer->socket_path, strerror(errno));
        return -1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(struct sockaddr_un));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, interposer->socket_path, sizeof(addr.sun_path) - 1);

    int attempt = 0;
    long total_slept_us = 0;
    long timeout_us = SOCKET_CONNECT_TIMEOUT_MS * 1000;
    long sleep_interval_us = 10000;

    sji_log_info("Attempting to connect to %s (fd %d)...", interposer->socket_path, interposer->sockfd);
    while (connect(interposer->sockfd, (struct sockaddr *)&addr, sizeof(struct sockaddr_un)) == -1) {
        if (errno == ENOENT || errno == ECONNREFUSED) {
            if (total_slept_us >= timeout_us) {
                sji_log_error("Timed out connecting to socket %s after %dms.", interposer->socket_path, SOCKET_CONNECT_TIMEOUT_MS);
                goto connect_fail;
            }
            if (attempt == 0 || (attempt % 10 == 0)) {
                 sji_log_warn("Connection to %s refused/not found, retrying (attempt %d, elapsed %ldms)...",
                              interposer->socket_path, attempt + 1, total_slept_us / 1000);
            }
            usleep(sleep_interval_us);
            total_slept_us += sleep_interval_us;
            attempt++;
            continue;
        }
        sji_log_error("Failed to connect to socket %s: %s", interposer->socket_path, strerror(errno));
        goto connect_fail;
    }
    sji_log_info("Connected to socket %s (fd %d).", interposer->socket_path, interposer->sockfd);

    if (read_socket_config(interposer->sockfd, &(interposer->js_config)) != 0) {
        sji_log_error("Failed to read config from socket %s.", interposer->socket_path);
        goto connect_fail;
    }

    unsigned char arch_byte[1] = { (unsigned char)sizeof(long) };
    sji_log_info("Sending architecture specifier (%u bytes, value: %u) to %s.", (unsigned int)sizeof(arch_byte), arch_byte[0], interposer->socket_path);
    if (real_write(interposer->sockfd, arch_byte, sizeof(arch_byte)) != sizeof(arch_byte)) {
        sji_log_error("Failed to send architecture specifier to %s: %s", interposer->socket_path, strerror(errno));
        goto connect_fail;
    }
    return 0;

connect_fail:
    if (interposer->sockfd != -1) {
        real_close(interposer->sockfd);
        interposer->sockfd = -1;
    }
    return -1;
}

/**
 * @brief Common logic for handling intercepted `open()` and `open64()` calls.
 *
 * This function checks if the `pathname` matches one of the device paths
 * configured for interposition. If a match is found:
 *  - If the device is already "opened" (i.e., its socket is connected),
 *    the existing socket fd is returned.
 *  - If it's a new open, `connect_interposer_socket()` is called to establish
 *    the connection and retrieve configuration.
 *  - If `O_NONBLOCK` was specified in `flags`, `make_socket_nonblocking()` is
 *    called for the socket.
 * The `found_interposer_ptr` is updated to point to the matched interposer.
 *
 * @param pathname The file path being opened.
 * @param flags The flags passed to `open()` or `open64()`.
 * @param found_interposer_ptr Output parameter; on successful interposition,
 *                             this will point to the `js_interposer_t` structure
 *                             for the opened device.
 * @return The socket file descriptor if the device is successfully interposed.
 *         -1 if an error occurs during interposition (e.g., socket connection failed).
 *            `errno` will be set (e.g., to `EIO`).
 *         -2 if the `pathname` is not recognized as an interposable device.
 *            The caller should then proceed to call the real `open()`/`open64()`.
 */
static int common_open_logic(const char *pathname, int flags, js_interposer_t **found_interposer_ptr) {
    *found_interposer_ptr = NULL;
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (strcmp(pathname, interposers[i].open_dev_name) == 0) {
            *found_interposer_ptr = &interposers[i];

            if (interposers[i].sockfd != -1) {
                sji_log_info("Device %s already open via interposer (socket_fd %d, app_flags_orig=0x%x, new_req_flags=0x%x). Reusing.",
                             pathname, interposers[i].sockfd, interposers[i].open_flags, flags);
                return interposers[i].sockfd;
            }

            interposers[i].open_flags = flags;

            if (connect_interposer_socket(&interposers[i]) == -1) {
                sji_log_error("Failed to establish socket connection for %s.", pathname);
                interposers[i].open_flags = 0;
                errno = EIO;
                return -1;
            }

            if (interposers[i].open_flags & O_NONBLOCK) {
                sji_log_info("Application opened %s with O_NONBLOCK. Setting socket fd %d to non-blocking.",
                             pathname, interposers[i].sockfd);
                if (make_socket_nonblocking(interposers[i].sockfd) == -1) {
                    sji_log_warn("Failed to make socket fd %d non-blocking for %s as requested by app. Socket may remain blocking.",
                                  interposers[i].sockfd, pathname);
                }
            }
            sji_log_info("Successfully interposed 'open' for %s (app_flags=0x%x), socket_fd: %d. Socket flags: 0x%x",
                         pathname, interposers[i].open_flags, interposers[i].sockfd, fcntl(interposers[i].sockfd, F_GETFL, 0));
            return interposers[i].sockfd;
        }
    }
    return -2;
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
        sji_log_error("CRITICAL: real_open not loaded. Cannot proceed with open call.");
        errno = EFAULT;
        return -1;
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(pathname, flags, &interposer);

    if (result_fd == -2) {
        mode_t mode = 0;
        if (flags & O_CREAT) {
            va_list args;
            va_start(args, flags);
            mode = va_arg(args, mode_t);
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
        sji_log_error("CRITICAL: Neither real_open64 nor real_open loaded. Cannot proceed with open64 call.");
        errno = EFAULT;
        return -1;
    }

    js_interposer_t *interposer = NULL;
    int result_fd = common_open_logic(pathname, flags, &interposer);

    if (result_fd == -2) {
        mode_t mode = 0;
        if (flags & O_CREAT) {
            va_list args;
            va_start(args, flags);
            mode = va_arg(args, mode_t);
            va_end(args);
        }

        if (real_open64) {
            result_fd = (flags & O_CREAT) ? real_open64(pathname, flags, mode) : real_open64(pathname, flags);
        } else {
            sji_log_info("real_open64 not available, falling back to real_open for: %s", pathname);
            result_fd = (flags & O_CREAT) ? real_open(pathname, flags, mode) : real_open(pathname, flags);
        }
    }
    return result_fd;
}

/**
 * @brief Intercepted `close()` system call.
 *
 * If `real_close` is not loaded, returns -1 with `errno` set to `EFAULT`.
 * Checks if the given file descriptor `fd` corresponds to one of the
 * interposer's active socket file descriptors.
 * If it is, `real_close()` is called on the socket fd, and the interposer's
 * state for that device is reset (sockfd set to -1, config cleared).
 * If `fd` is not an interposed socket, the call is passed to `real_close()`.
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

    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (fd >= 0 && fd == interposers[i].sockfd) {
            sji_log_info("Intercepted 'close' for interposed fd %d (device %s). Closing socket.",
                         fd, interposers[i].open_dev_name);
            int ret = real_close(fd);
            if (ret == 0) {
                interposers[i].sockfd = -1;
                interposers[i].open_flags = 0;
                memset(&(interposers[i].js_config), 0, sizeof(js_config_t));
                sji_log_info("Socket for %s (fd %d) closed and interposer state reset.", interposers[i].open_dev_name, fd);
            } else {
                sji_log_error("real_close on socket fd %d for %s failed: %s.",
                              fd, interposers[i].open_dev_name, strerror(errno));
            }
            return ret;
        }
    }
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
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (fd == interposers[i].sockfd && interposers[i].sockfd != -1) {
            interposer = &interposers[i];
            break;
        }
    }

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

    int socket_actual_flags = fcntl(interposer->sockfd, F_GETFL, 0);
    int socket_is_actually_nonblocking = (socket_actual_flags != -1 && (socket_actual_flags & O_NONBLOCK));

    if (socket_actual_flags == -1) {
        sji_log_warn("read: fcntl(F_GETFL) failed for sockfd %d (%s): %s. Proceeding, assuming blocking status based on open_flags.",
                     interposer->sockfd, interposer->open_dev_name, strerror(errno));
        socket_is_actually_nonblocking = (interposer->open_flags & O_NONBLOCK);
    }
    
    ssize_t bytes_read = recv(interposer->sockfd, buf, event_size, 0);

    if (bytes_read == -1) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (socket_is_actually_nonblocking) {
                 sji_log_debug("read: sockfd %d (%s) non-blocking, no data (EAGAIN/EWOULDBLOCK)", interposer->sockfd, interposer->open_dev_name);
            } else {
                 sji_log_warn("read: sockfd %d (%s) reported as blocking, but got EAGAIN/EWOULDBLOCK. This might indicate an issue or a race condition.", interposer->sockfd, interposer->open_dev_name);
            }
        } else {
            sji_log_error("SOCKET_READ_ERR: read from socket_fd %d (%s) failed: %s (errno %d)",
                          interposer->sockfd, interposer->open_dev_name, strerror(errno), errno);
        }
        return -1;
    } else if (bytes_read == 0) {
        sji_log_info("SOCKET_READ_EOF: read from socket_fd %d (%s) returned 0 (EOF - server closed connection?)",
                     interposer->sockfd, interposer->open_dev_name);
        return 0;
    } else {
        sji_log_debug("SOCKET_READ_OK: read %zd bytes from socket_fd %d (%s)",
                     bytes_read, interposer->sockfd, interposer->open_dev_name);
        if (bytes_read < event_size && bytes_read > 0) {
            sji_log_warn("SOCKET_READ_PARTIAL: read %zd bytes from socket_fd %d (%s), but expected %zu. This might cause issues.",
                         bytes_read, interposer->sockfd, interposer->open_dev_name, event_size);
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
        for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
            if (fd == interposers[i].sockfd && interposers[i].sockfd != -1) {
                sji_log_info("epoll_ctl %s for interposed socket fd %d (%s). Ensuring O_NONBLOCK.",
                             (op == EPOLL_CTL_ADD ? "ADD" : "MOD"), fd, interposers[i].open_dev_name);
                if (make_socket_nonblocking(fd) == -1) {
                    sji_log_warn("epoll_ctl: Failed to ensure O_NONBLOCK for socket fd %d (%s). Epoll behavior might be affected.",
                                 fd, interposers[i].open_dev_name);
                }
                break;
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
int intercept_ev_ioctl(js_interposer_t *interposer, int fd, ioctl_request_t request, void *arg) {
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

            if (abs_code == ABS_X || abs_code == ABS_Y || abs_code == ABS_RX || abs_code == ABS_RY) {
                absinfo_ptr->minimum = ABS_AXIS_MIN_DEFAULT; 
                absinfo_ptr->maximum = ABS_AXIS_MAX_DEFAULT; 
                absinfo_ptr->fuzz = 16;     
                absinfo_ptr->flat = 128;    
                absinfo_ptr->resolution = 1;
                sji_log_debug("IOCTL_EV(%s): EVIOCGABS(0x%02x) - Main analog stick. min=%d, max=%d, res=%d",
                             interposer->open_dev_name, abs_code, absinfo_ptr->minimum, absinfo_ptr->maximum, absinfo_ptr->resolution);
            } else if (abs_code == ABS_Z || abs_code == ABS_RZ) {
                absinfo_ptr->minimum = ABS_TRIGGER_MIN_DEFAULT;
                absinfo_ptr->maximum = ABS_TRIGGER_MAX_DEFAULT;
                absinfo_ptr->fuzz = 0;
                absinfo_ptr->flat = 0;
                absinfo_ptr->resolution = 1;
                sji_log_debug("IOCTL_EV(%s): EVIOCGABS(0x%02x) - Trigger. min=%d, max=%d, res=%d",
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

            ptrdiff_t interposer_array_idx = interposer - interposers;
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

            ptrdiff_t interposer_array_idx = interposer - interposers;
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
    for (size_t i = 0; i < NUM_INTERPOSERS(); i++) {
        if (fd == interposers[i].sockfd && interposers[i].sockfd != -1) {
            interposer = &interposers[i];
            break;
        }
    }

    if (interposer == NULL) {
        return real_ioctl(fd, request, arg_ptr);
    }

    if (interposer->type == DEV_TYPE_JS) {
        return intercept_js_ioctl(interposer, fd, request, arg_ptr);
    } else if (interposer->type == DEV_TYPE_EV) {
        return intercept_ev_ioctl(interposer, fd, request, arg_ptr);
    } else {
        sji_log_error("IOCTL(%s): Interposer has unknown type %d for fd %d. This should not happen. Setting EINVAL.",
                       interposer->open_dev_name, interposer->type, fd);
        errno = EINVAL;
        return -1;
    }
}
