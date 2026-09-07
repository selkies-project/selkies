/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * Selkies fake libudev
 *
 * A libudev replacement (LD_PRELOAD or library substitution) that adds
 * NUM_VIRTUAL_GAMEPADS Xbox 360 pads, whose /dev/input/jsN and
 * /dev/input/eventN nodes are served by the sibling joystick interposer, to
 * everything the real libudev reports. The pads are built from the static
 * definitions in initialize_virtual_gamepads_data_if_needed(); every other
 * device, subsystem and hotplug event passes through to the real library.
 *
 * Passthrough. This library carries the `libudev.so.1` SONAME, so the loader
 * never maps the real file for the application; it is dlopen()ed by path at
 * the first udev_new() instead. `SELKIES_REAL_LIBUDEV` names it (`none`
 * disables passthrough); otherwise `LD_LIBRARY_PATH` and the platform library
 * directories are searched for a `libudev.so.1*` that is neither this file
 * nor a copy of it, told by the exported `selkies_fake_udev_identity`.
 * Without a real library only the pads are visible. The real library is
 * loaded without RTLD_DEEPBIND, so an allocator the application interposes
 * stays interposed inside it; its internal calls to its own exports therefore
 * land here too. Every object this library allocates opens with a type magic,
 * and a pointer without one is the real library's, forwarded untouched.
 * Consumers only ever hold wrappers: a wrapper around a real object owns one
 * reference to it and copies the real object's lists into its own entries,
 * freed with the wrapper, so an opaque list pointer is always ours.
 *
 * Policy. The pads live in the "input" subsystem. A real node with one of
 * their names (js0..js3, event1000..event1003) is hidden from enumeration,
 * lookups and the monitor, since that /dev/input path is the interposer's;
 * every other real input device stays visible. An enumeration adds the pads
 * when it has no subsystem filter or matches "input" without excluding it; a
 * monitor delivers them when it has no subsystem filter or one for "input"
 * without a devtype. Filters the pads cannot honour (sysattr, tag,
 * is_initialized) are forwarded and do not restrict them.
 *
 * Each pad is a four-node tree: a usb_device parent (idVendor/idProduct,
 * serial), an input parent under it (the `id` attributes, name, phys, uniq,
 * capabilities), and the js and event children carrying the devnode, the
 * input-major devnum and the ID_INPUT_* properties. The identity values
 * (0x045e:0x028e, "Microsoft X-Box 360 pad", the uniq "SGVP%04d") must agree
 * with the ones the joystick interposer reports through its ioctls. A generic
 * scan of "input" yields the js and event nodes whose interposer socket is
 * bound; a sysname pattern may also select the input parent; property filters
 * apply to each.
 *
 * A udev_monitor reports pad hotplug by watching the interposer's socket
 * directory with inotify: creation or deletion of "selkies_<sysname>.sock"
 * becomes an "add" or "remove" for that node. The directory is
 * SELKIES_JS_SOCKET_PATH (default /tmp) and must match the interposer's. The
 * fd handed to consumers is an epoll set over the inotify fd, the real
 * monitor's fd and an eventfd that stays armed while buffered inotify records
 * remain undispensed, so it polls readable exactly while
 * udev_monitor_receive_device() has something to yield.
 *
 * JS_LOG in the environment enables stderr diagnostics.
 */

#define _GNU_SOURCE
#include "libudev.h"
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fnmatch.h>
#include <limits.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <unistd.h>

static bool g_fake_udev_log_enabled = false;

static void fake_udev_logging_init_if_needed(void) {
    static bool initialized = false;
    if (initialized) {
        return;
    }
    initialized = true;
    g_fake_udev_log_enabled = getenv("JS_LOG") != NULL;
}

#define FAKE_UDEV_LOG(level, fmt, ...) do { if (g_fake_udev_log_enabled) fprintf(stderr, "[fake_udev_" level ":%s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); } while (0)
#define FAKE_UDEV_LOG_DEBUG(...) FAKE_UDEV_LOG("dbg", __VA_ARGS__)
#define FAKE_UDEV_LOG_INFO(...)  FAKE_UDEV_LOG("info", __VA_ARGS__)
#define FAKE_UDEV_LOG_WARN(...)  FAKE_UDEV_LOG("warn", __VA_ARGS__)
#define FAKE_UDEV_LOG_ERROR(...) FAKE_UDEV_LOG("err", __VA_ARGS__)

#define NUM_VIRTUAL_GAMEPADS 4
#define VIRTUAL_EVENT_ID_BASE 1000
#define INPUT_MAJOR 13
#define EVDEV_MINOR_BASE 64

/* The inotify read buffer must hold at least one max-length record or read()
 * fails with EINVAL. */
#define FAKE_UDEV_SOCKET_DIR_DEFAULT "/tmp"
#define FAKE_UDEV_INOTIFY_EVBUF_SIZE 4096

/* Type magics opening every object this library allocates; see the module
 * comment for why an object without one is forwarded to the real library. */
#define FAKE_MAGIC_UDEV      ((uintptr_t)0x53454C01u)
#define FAKE_MAGIC_DEVICE    ((uintptr_t)0x53454C02u)
#define FAKE_MAGIC_ENUMERATE ((uintptr_t)0x53454C03u)
#define FAKE_MAGIC_MONITOR   ((uintptr_t)0x53454C04u)
#define FAKE_MAGIC_LIST      ((uintptr_t)0x53454C05u)
#define FAKE_MAGIC_HWDB      ((uintptr_t)0x53454C06u)
#define FAKE_MAGIC_QUEUE     ((uintptr_t)0x53454C07u)

/* True for a non-NULL object the real library allocated. */
static inline bool is_raw(const void *obj, uintptr_t magic) {
    return obj != NULL && *(const uintptr_t *)obj != magic;
}

/* ------------------------------------------------------------------------ */
/* The real library                                                          */
/* ------------------------------------------------------------------------ */

/* Every entry point forwarded to the real library. The deprecated logging
 * setters are not among them; they are inert there as well. */
#define FAKE_UDEV_REAL_SYMBOLS(X) \
    X(udev_new) X(udev_ref) X(udev_unref) X(udev_get_userdata) X(udev_set_userdata) \
    X(udev_list_entry_get_next) X(udev_list_entry_get_by_name) \
    X(udev_list_entry_get_name) X(udev_list_entry_get_value) \
    X(udev_device_ref) X(udev_device_unref) X(udev_device_get_udev) \
    X(udev_device_new_from_syspath) X(udev_device_new_from_devnum) \
    X(udev_device_new_from_subsystem_sysname) X(udev_device_new_from_device_id) \
    X(udev_device_new_from_environment) X(udev_device_get_parent) \
    X(udev_device_get_parent_with_subsystem_devtype) X(udev_device_get_devpath) \
    X(udev_device_get_subsystem) X(udev_device_get_devtype) X(udev_device_get_syspath) \
    X(udev_device_get_sysname) X(udev_device_get_sysnum) X(udev_device_get_devnode) \
    X(udev_device_get_is_initialized) X(udev_device_get_devlinks_list_entry) \
    X(udev_device_get_properties_list_entry) X(udev_device_get_tags_list_entry) \
    X(udev_device_get_current_tags_list_entry) X(udev_device_get_sysattr_list_entry) \
    X(udev_device_get_property_value) X(udev_device_get_driver) X(udev_device_get_devnum) \
    X(udev_device_get_action) X(udev_device_get_seqnum) \
    X(udev_device_get_usec_since_initialized) X(udev_device_get_sysattr_value) \
    X(udev_device_set_sysattr_value) X(udev_device_has_tag) X(udev_device_has_current_tag) \
    X(udev_monitor_ref) X(udev_monitor_unref) X(udev_monitor_get_udev) \
    X(udev_monitor_new_from_netlink) X(udev_monitor_enable_receiving) \
    X(udev_monitor_set_receive_buffer_size) X(udev_monitor_get_fd) \
    X(udev_monitor_receive_device) X(udev_monitor_filter_add_match_subsystem_devtype) \
    X(udev_monitor_filter_add_match_tag) X(udev_monitor_filter_update) \
    X(udev_monitor_filter_remove) \
    X(udev_enumerate_ref) X(udev_enumerate_unref) X(udev_enumerate_get_udev) \
    X(udev_enumerate_new) X(udev_enumerate_add_match_subsystem) \
    X(udev_enumerate_add_nomatch_subsystem) X(udev_enumerate_add_match_sysattr) \
    X(udev_enumerate_add_nomatch_sysattr) X(udev_enumerate_add_match_property) \
    X(udev_enumerate_add_match_sysname) X(udev_enumerate_add_match_tag) \
    X(udev_enumerate_add_match_parent) X(udev_enumerate_add_match_is_initialized) \
    X(udev_enumerate_add_syspath) X(udev_enumerate_scan_devices) \
    X(udev_enumerate_scan_subsystems) X(udev_enumerate_get_list_entry) \
    X(udev_queue_ref) X(udev_queue_unref) X(udev_queue_get_udev) X(udev_queue_new) \
    X(udev_queue_get_kernel_seqnum) X(udev_queue_get_udev_seqnum) \
    X(udev_queue_get_udev_is_active) X(udev_queue_get_queue_is_empty) \
    X(udev_queue_get_seqnum_is_finished) X(udev_queue_get_seqnum_sequence_is_finished) \
    X(udev_queue_get_fd) X(udev_queue_flush) X(udev_queue_get_queued_list_entry) \
    X(udev_hwdb_new) X(udev_hwdb_ref) X(udev_hwdb_unref) X(udev_hwdb_get_properties_list_entry) \
    X(udev_util_encode_string)

/* Entry points of the real library, NULL where it lacks one (an older
 * library predating a symbol); `loaded` is false when none was found. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"
static struct {
    void *handle;
    bool loaded;
#define FAKE_UDEV_DECLARE_REAL(name) __typeof__(name) *name;
    FAKE_UDEV_REAL_SYMBOLS(FAKE_UDEV_DECLARE_REAL)
#undef FAKE_UDEV_DECLARE_REAL
} R;
#pragma GCC diagnostic pop

static pthread_once_t g_real_once = PTHREAD_ONCE_INIT;

/* Marks this library for the search below: a candidate exporting it is a
 * copy of the fake, not the real library. */
int selkies_fake_udev_identity = 1;

static const char *const fake_udev_lib_dirs[] = {
#if defined(__x86_64__)
    "/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu", "/usr/lib64", "/lib64",
#elif defined(__i386__)
    "/usr/lib/i386-linux-gnu", "/lib/i386-linux-gnu", "/usr/lib32", "/lib32",
#elif defined(__aarch64__)
    "/usr/lib/aarch64-linux-gnu", "/lib/aarch64-linux-gnu", "/usr/lib64", "/lib64",
#elif defined(__arm__)
    "/usr/lib/arm-linux-gnueabihf", "/lib/arm-linux-gnueabihf",
#elif defined(__riscv)
    "/usr/lib/riscv64-linux-gnu", "/lib/riscv64-linux-gnu", "/usr/lib64", "/lib64",
#elif defined(__powerpc64__)
    "/usr/lib/powerpc64le-linux-gnu", "/lib/powerpc64le-linux-gnu", "/usr/lib64", "/lib64",
#elif defined(__s390x__)
    "/usr/lib/s390x-linux-gnu", "/lib/s390x-linux-gnu", "/usr/lib64", "/lib64",
#endif
    "/usr/lib", "/lib", "/usr/local/lib", "/usr/local/lib64",
    "/usr/lib64", "/lib64", "/usr/lib32", "/lib32",
};

/* Whether `path` is the file this library was loaded from, compared by
 * inode so a symlink to it is recognised. */
static bool fake_udev_is_self(const char *path) {
    Dl_info info;
    struct stat self_st, cand_st;
    if (!dladdr((void *)&udev_new, &info) || !info.dli_fname) {
        return false;
    }
    if (stat(info.dli_fname, &self_st) != 0 || stat(path, &cand_st) != 0) {
        return false;
    }
    return self_st.st_dev == cand_st.st_dev && self_st.st_ino == cand_st.st_ino;
}

/* Binds R to the library at `path` when it is a real libudev: one that
 * defines udev_new and does not carry this library's identity marker. */
static bool fake_udev_try_real(const char *path) {
    void *handle = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!handle) {
        FAKE_UDEV_LOG_DEBUG("dlopen(%s) failed: %s", path, dlerror());
        return false;
    }
    void *real_new = dlsym(handle, "udev_new");
    if (!real_new || real_new == (void *)&udev_new || dlsym(handle, "selkies_fake_udev_identity")) {
        FAKE_UDEV_LOG_DEBUG("%s is not the real libudev", path);
        dlclose(handle);
        return false;
    }
#define FAKE_UDEV_BIND_REAL(name) R.name = (__typeof__(R.name))dlsym(handle, #name);
    FAKE_UDEV_REAL_SYMBOLS(FAKE_UDEV_BIND_REAL)
#undef FAKE_UDEV_BIND_REAL
    R.handle = handle;
    R.loaded = true;
    FAKE_UDEV_LOG_INFO("passing through to real libudev %s", path);
    return true;
}

/* Tries every libudev.so.1* in `dir` other than this library or a copy of it. */
static bool fake_udev_search_dir(const char *dir) {
    DIR *d = opendir(dir);
    if (!d) {
        return false;
    }
    bool found = false;
    struct dirent *entry;
    while (!found && (entry = readdir(d)) != NULL) {
        if (strncmp(entry->d_name, "libudev.so.1", strlen("libudev.so.1")) != 0 || strstr(entry->d_name, "fake")) {
            continue;
        }
        char path[PATH_MAX];
        snprintf(path, sizeof(path), "%s/%s", dir, entry->d_name);
        if (fake_udev_is_self(path)) {
            continue;
        }
        found = fake_udev_try_real(path);
    }
    closedir(d);
    return found;
}

/* Locates and binds the real library once, as the module comment describes. */
static void fake_udev_load_real(void) {
    fake_udev_logging_init_if_needed();
    const char *explicit_path = getenv("SELKIES_REAL_LIBUDEV");
    if (explicit_path) {
        if (explicit_path[0] == '\0' || strcmp(explicit_path, "none") == 0) {
            FAKE_UDEV_LOG_INFO("passthrough disabled by SELKIES_REAL_LIBUDEV");
            return;
        }
        if (fake_udev_try_real(explicit_path)) {
            return;
        }
        FAKE_UDEV_LOG_WARN("SELKIES_REAL_LIBUDEV=%s is unusable, searching the library directories", explicit_path);
    }
    const char *ld_path = getenv("LD_LIBRARY_PATH");
    if (ld_path && ld_path[0]) {
        char *copy = strdup(ld_path);
        if (copy) {
            char *save = NULL;
            for (char *dir = strtok_r(copy, ":", &save); dir; dir = strtok_r(NULL, ":", &save)) {
                if (dir[0] && fake_udev_search_dir(dir)) {
                    free(copy);
                    return;
                }
            }
            free(copy);
        }
    }
    for (size_t i = 0; i < sizeof(fake_udev_lib_dirs) / sizeof(fake_udev_lib_dirs[0]); ++i) {
        if (fake_udev_search_dir(fake_udev_lib_dirs[i])) {
            return;
        }
    }
    FAKE_UDEV_LOG_WARN("no real libudev found; only the virtual gamepads are visible");
}

/* ------------------------------------------------------------------------ */
/* Virtual gamepad definitions                                               */
/* ------------------------------------------------------------------------ */

static const char *fake_udev_socket_dir(void) {
    const char *d = getenv("SELKIES_JS_SOCKET_PATH");
    return (d && d[0]) ? d : FAKE_UDEV_SOCKET_DIR_DEFAULT;
}

/* Whether the interposer socket behind the node `sysname` is bound. Only a
 * served node is enumerated, matching the interposer's own directory listing:
 * a scanner that opens an unserved node pays the full connect timeout before
 * it fails, and the monitor announces the node once its socket appears. */
static bool sysname_socket_live(const char *sysname) {
    char sock[PATH_MAX];
    snprintf(sock, sizeof(sock), "%s/selkies_%s.sock", fake_udev_socket_dir(), sysname);
    return access(sock, F_OK) == 0;
}

typedef enum {
    VIRTUAL_TYPE_NONE = -1,
    VIRTUAL_TYPE_JS,
    VIRTUAL_TYPE_EVENT,
    VIRTUAL_TYPE_INPUT_PARENT,
    VIRTUAL_TYPE_USB_PARENT
} virtual_device_node_type_t;

typedef struct {
    const char *name;
    const char *value;
} key_value_pair_t;

typedef struct {
    int id; // 0 to NUM_VIRTUAL_GAMEPADS-1

    char js_syspath[256];
    char js_devnode[64];
    char js_sysname[64];
    key_value_pair_t js_properties[4]; // DEVNAME, ID_INPUT_JOYSTICK, ID_INPUT, NULL

    char event_syspath[256];
    char event_devnode[64];
    char event_sysname[64];
    key_value_pair_t event_properties[6]; // DEVNAME, ID_INPUT_EVENT_JOYSTICK, ID_INPUT_JOYSTICK, ID_INPUT_GAMEPAD, ID_INPUT, NULL

    char input_parent_syspath[256];
    char input_parent_sysname[64];
    key_value_pair_t input_parent_sysattrs[12]; // vendor, product, version, name, phys, uniq, caps, etc. +NULL
    key_value_pair_t input_parent_properties[3]; // ID_INPUT, ID_INPUT_JOYSTICK, NULL

    char usb_parent_syspath[256];
    char usb_parent_sysname[64];
    key_value_pair_t usb_parent_sysattrs[7]; // idVendor, idProduct, manufacturer, product, bcdDevice, serial (+NULL)
} virtual_gamepad_definition_t;

static virtual_gamepad_definition_t virtual_gamepads[NUM_VIRTUAL_GAMEPADS];
static bool virtual_gamepads_initialized = false;

/* Backing storage for the formatted sysattr/property values the definitions point at. */
static char input_phys[NUM_VIRTUAL_GAMEPADS][64];
static char input_uniq[NUM_VIRTUAL_GAMEPADS][64];
static char usb_serials[NUM_VIRTUAL_GAMEPADS][64];

static void initialize_virtual_gamepads_data_if_needed(void) {
    if (virtual_gamepads_initialized) {
        return;
    }
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        def->id = i;

        snprintf(def->input_parent_sysname, sizeof(def->input_parent_sysname), "input%d", i + 10);
        snprintf(def->input_parent_syspath, sizeof(def->input_parent_syspath),
                 "/sys/devices/virtual/selkies_pad%d/input/%s", i, def->input_parent_sysname);
        def->input_parent_sysattrs[0] = (key_value_pair_t){"id/vendor", "0x045e"};
        def->input_parent_sysattrs[1] = (key_value_pair_t){"id/product", "0x028e"};
        def->input_parent_sysattrs[2] = (key_value_pair_t){"id/version", "0x0114"};
        def->input_parent_sysattrs[3] = (key_value_pair_t){"name", "Microsoft X-Box 360 pad"};
        snprintf(input_phys[i], sizeof(input_phys[i]), "selkies/virtpad%d/input0", i);
        def->input_parent_sysattrs[4] = (key_value_pair_t){"phys", input_phys[i]};
        snprintf(input_uniq[i], sizeof(input_uniq[i]), "SGVP%04d", i);
        def->input_parent_sysattrs[5] = (key_value_pair_t){"uniq", input_uniq[i]};
        def->input_parent_sysattrs[6] = (key_value_pair_t){"capabilities/ev", "1b"};
        def->input_parent_sysattrs[7] = (key_value_pair_t){"capabilities/key", "ffff000000000000 0 0 0 0 0 7fdb000000000000 0 0 0 0"};
        def->input_parent_sysattrs[8] = (key_value_pair_t){"capabilities/abs", "3003f"};
        def->input_parent_sysattrs[9] = (key_value_pair_t){"id/bustype", "0003"}; // BUS_USB
        def->input_parent_sysattrs[10] = (key_value_pair_t){"event_count", "123"};
        def->input_parent_sysattrs[11] = (key_value_pair_t){NULL, NULL};
        def->input_parent_properties[0] = (key_value_pair_t){"ID_INPUT", "1"};
        def->input_parent_properties[1] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"};
        def->input_parent_properties[2] = (key_value_pair_t){NULL, NULL};

        snprintf(def->js_sysname, sizeof(def->js_sysname), "js%d", i);
        snprintf(def->js_syspath, sizeof(def->js_syspath), "%s/%s", def->input_parent_syspath, def->js_sysname);
        snprintf(def->js_devnode, sizeof(def->js_devnode), "/dev/input/js%d", i);
        def->js_properties[0] = (key_value_pair_t){"DEVNAME", def->js_devnode};
        def->js_properties[1] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"};
        def->js_properties[2] = (key_value_pair_t){"ID_INPUT", "1"};
        def->js_properties[3] = (key_value_pair_t){NULL, NULL};

        snprintf(def->event_sysname, sizeof(def->event_sysname), "event%d", VIRTUAL_EVENT_ID_BASE + i);
        snprintf(def->event_syspath, sizeof(def->event_syspath), "%s/%s", def->input_parent_syspath, def->event_sysname);
        snprintf(def->event_devnode, sizeof(def->event_devnode), "/dev/input/event%d", VIRTUAL_EVENT_ID_BASE + i);
        def->event_properties[0] = (key_value_pair_t){"DEVNAME", def->event_devnode};
        def->event_properties[1] = (key_value_pair_t){"ID_INPUT_EVENT_JOYSTICK", "1"};
        def->event_properties[2] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"};
        def->event_properties[3] = (key_value_pair_t){"ID_INPUT_GAMEPAD", "1"};
        def->event_properties[4] = (key_value_pair_t){"ID_INPUT", "1"};
        def->event_properties[5] = (key_value_pair_t){NULL, NULL};

        snprintf(def->usb_parent_sysname, sizeof(def->usb_parent_sysname), "selkies_usb_ctrl%d_dev", i);
        snprintf(def->usb_parent_syspath, sizeof(def->usb_parent_syspath), "/sys/devices/virtual/usb/%s", def->usb_parent_sysname);
        def->usb_parent_sysattrs[0] = (key_value_pair_t){"idVendor", "0x045e"};
        def->usb_parent_sysattrs[1] = (key_value_pair_t){"idProduct", "0x028e"};
        def->usb_parent_sysattrs[2] = (key_value_pair_t){"manufacturer", "©Microsoft Corporation"};
        def->usb_parent_sysattrs[3] = (key_value_pair_t){"product", "Controller"};
        def->usb_parent_sysattrs[4] = (key_value_pair_t){"bcdDevice", "0x0114"};
        snprintf(usb_serials[i], sizeof(usb_serials[i]), "SELKIESUSB%04d", i);
        def->usb_parent_sysattrs[5] = (key_value_pair_t){"serial", usb_serials[i]};
        def->usb_parent_sysattrs[6] = (key_value_pair_t){NULL, NULL};
    }
    virtual_gamepads_initialized = true;
    FAKE_UDEV_LOG_INFO("initialized %d virtual gamepads, event nodes %d..%d",
                       NUM_VIRTUAL_GAMEPADS, VIRTUAL_EVENT_ID_BASE, VIRTUAL_EVENT_ID_BASE + NUM_VIRTUAL_GAMEPADS - 1);
}

static const virtual_gamepad_definition_t *find_virtual_def_by_syspath(const char *syspath, virtual_device_node_type_t *node_type_out) {
    initialize_virtual_gamepads_data_if_needed();
    *node_type_out = VIRTUAL_TYPE_NONE;
    if (!syspath) {
        return NULL;
    }
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        if (strcmp(syspath, def->js_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_JS; return def; }
        if (strcmp(syspath, def->event_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_EVENT; return def; }
        if (strcmp(syspath, def->input_parent_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_INPUT_PARENT; return def; }
        if (strcmp(syspath, def->usb_parent_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_USB_PARENT; return def; }
    }
    return NULL;
}

/* The virtual js or event node named `sysname`, if any. */
static const virtual_gamepad_definition_t *find_virtual_node_by_sysname(const char *sysname, virtual_device_node_type_t *node_type_out) {
    initialize_virtual_gamepads_data_if_needed();
    *node_type_out = VIRTUAL_TYPE_NONE;
    if (!sysname) {
        return NULL;
    }
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        if (strcmp(sysname, def->js_sysname) == 0) { *node_type_out = VIRTUAL_TYPE_JS; return def; }
        if (strcmp(sysname, def->event_sysname) == 0) { *node_type_out = VIRTUAL_TYPE_EVENT; return def; }
    }
    return NULL;
}

static const char *virtual_syspath(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    switch (type) {
        case VIRTUAL_TYPE_JS: return def->js_syspath;
        case VIRTUAL_TYPE_EVENT: return def->event_syspath;
        case VIRTUAL_TYPE_INPUT_PARENT: return def->input_parent_syspath;
        case VIRTUAL_TYPE_USB_PARENT: return def->usb_parent_syspath;
        default: return NULL;
    }
}

static const char *virtual_sysname(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    switch (type) {
        case VIRTUAL_TYPE_JS: return def->js_sysname;
        case VIRTUAL_TYPE_EVENT: return def->event_sysname;
        case VIRTUAL_TYPE_INPUT_PARENT: return def->input_parent_sysname;
        case VIRTUAL_TYPE_USB_PARENT: return def->usb_parent_sysname;
        default: return NULL;
    }
}

static const char *virtual_subsystem(virtual_device_node_type_t type) {
    return type == VIRTUAL_TYPE_USB_PARENT ? "usb" : "input";
}

static const key_value_pair_t *virtual_property_table(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    switch (type) {
        case VIRTUAL_TYPE_JS: return def->js_properties;
        case VIRTUAL_TYPE_EVENT: return def->event_properties;
        case VIRTUAL_TYPE_INPUT_PARENT: return def->input_parent_properties;
        default: return NULL;
    }
}

static const key_value_pair_t *virtual_sysattr_table(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    switch (type) {
        case VIRTUAL_TYPE_INPUT_PARENT: return def->input_parent_sysattrs;
        case VIRTUAL_TYPE_USB_PARENT: return def->usb_parent_sysattrs;
        default: return NULL;
    }
}

/* A virtual node's property: its table first, then the SUBSYSTEM and DEVPATH
 * every real device carries. */
static const char *virtual_property(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type, const char *key) {
    const key_value_pair_t *table = virtual_property_table(def, type);
    for (int i = 0; table && table[i].name != NULL; ++i) {
        if (strcmp(table[i].name, key) == 0) {
            return table[i].value;
        }
    }
    if (strcmp(key, "SUBSYSTEM") == 0) {
        return virtual_subsystem(type);
    }
    if (strcmp(key, "DEVPATH") == 0) {
        return virtual_syspath(def, type) + strlen("/sys");
    }
    return NULL;
}

static dev_t virtual_devnum(const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    switch (type) {
        case VIRTUAL_TYPE_JS: return makedev(INPUT_MAJOR, def->id);
        case VIRTUAL_TYPE_EVENT: return makedev(INPUT_MAJOR, EVDEV_MINOR_BASE + VIRTUAL_EVENT_ID_BASE + def->id);
        default: return 0;
    }
}

/* Whether a real input node named `sysname` collides with a virtual node and
 * is therefore hidden; see the module comment. */
static bool real_input_sysname_hidden(const char *sysname) {
    virtual_device_node_type_t type;
    return find_virtual_node_by_sysname(sysname, &type) != NULL;
}

/* The same test on a syspath, for enumeration results not yet instantiated. */
static bool real_syspath_hidden(const char *syspath) {
    if (!syspath || !strstr(syspath, "/input/")) {
        return false;
    }
    const char *base = strrchr(syspath, '/');
    return base && real_input_sysname_hidden(base + 1);
}

/* ------------------------------------------------------------------------ */
/* Objects                                                                   */
/* ------------------------------------------------------------------------ */

struct udev {
    uintptr_t magic;
    int n_ref;
    struct udev *real;
    void *userdata;
};

struct udev_list_entry {
    uintptr_t magic;
    struct udev_list_entry *next;
    char *name;
    char *value;
};

/* A parent handed out earlier, kept for the child's lifetime as libudev
 * specifies (the caller does not own the returned parent). */
struct parent_cache {
    char *key;
    struct udev_device *dev;
    struct parent_cache *next;
};

struct udev_device {
    uintptr_t magic;
    struct udev *udev_ctx;
    int n_ref;
    struct udev_device *real; // owned reference; NULL for a virtual node
    const virtual_gamepad_definition_t *gamepad_def;
    virtual_device_node_type_t node_type;
    struct udev_list_entry *properties;
    struct udev_list_entry *sysattrs;
    struct udev_list_entry *devlinks;
    struct udev_list_entry *tags;
    struct udev_list_entry *current_tags;
    bool properties_built, sysattrs_built, devlinks_built, tags_built, current_tags_built;
    struct parent_cache *parents;
    const char *action; // hotplug action from a monitor ("add"/"remove"); NULL otherwise
};

struct udev_enumerate {
    uintptr_t magic;
    struct udev *udev_ctx;
    int n_ref;
    struct udev_enumerate *real;
    bool real_muted;                // a filter the real library cannot see (a virtual parent) empties its results
    struct udev_list_entry *results;
    bool subsystem_filtered;        // any add_match_subsystem
    bool input_matched;             // ... one of them "input"
    bool input_excluded;            // add_nomatch_subsystem("input")
    bool sysname_filtered;
    char sysname_pattern[64];
    struct udev_list_entry *property_filters;
    const virtual_gamepad_definition_t *parent_def; // add_match_parent with a virtual parent
    virtual_device_node_type_t parent_type;
    struct udev_list_entry *extra_syspaths; // virtual syspaths from add_syspath
};

struct udev_monitor {
    uintptr_t magic;
    struct udev *udev_ctx;
    int n_ref;
    struct udev_monitor *real;
    int real_fd;               // the real monitor's fd, in the epoll set; -1 without one
    int fd;                    // epoll set over inotify_fd, real_fd and evbuf_efd; -1 => hand out inotify_fd
    int inotify_fd;            // -1 if unavailable
    int evbuf_efd;             // eventfd armed while evbuf holds an undispensed matching record
    bool evbuf_efd_armed;      // current arm state of evbuf_efd
    int watch_wd;              // watch descriptor for the socket directory, -1 if none
    bool subsystem_filtered;   // any filter_add_match_subsystem_devtype
    bool input_matched;        // ... one of them "input" without a devtype
    char evbuf[FAKE_UDEV_INOTIFY_EVBUF_SIZE]; // undispensed inotify records
    size_t evbuf_len;          // valid bytes in evbuf
    size_t evbuf_off;          // offset of the next record to dispense
};

struct udev_hwdb {
    uintptr_t magic;
    int n_ref;
    struct udev_hwdb *real;
    struct udev_list_entry *properties; // the last lookup's copy, valid until the next
};

struct udev_queue {
    uintptr_t magic;
    struct udev *udev_ctx;
    int n_ref;
    struct udev_queue *real;
    struct udev_list_entry *queued;
};

/* ---- lists ---- */

static void free_udev_list(struct udev_list_entry *head) {
    while (head) {
        struct udev_list_entry *next = head->next;
        free(head->name);
        free(head->value);
        free(head);
        head = next;
    }
}

/* Appends a copy of name/value; a NULL value stays NULL. Returns false on
 * allocation failure, leaving the list as it was. */
static bool list_append(struct udev_list_entry **head, struct udev_list_entry **tail, const char *name, const char *value) {
    struct udev_list_entry *entry = (struct udev_list_entry *)calloc(1, sizeof(*entry));
    if (!entry) {
        return false;
    }
    entry->magic = FAKE_MAGIC_LIST;
    entry->name = name ? strdup(name) : NULL;
    entry->value = value ? strdup(value) : NULL;
    if ((name && !entry->name) || (value && !entry->value)) {
        free(entry->name);
        free(entry->value);
        free(entry);
        return false;
    }
    if (*head) {
        (*tail)->next = entry;
    } else {
        *head = entry;
    }
    *tail = entry;
    return true;
}

/* A copy of a real library list as our own entries. */
static struct udev_list_entry *list_copy_real(struct udev_list_entry *real_head) {
    struct udev_list_entry *head = NULL, *tail = NULL;
    for (struct udev_list_entry *it = real_head; it; it = R.udev_list_entry_get_next(it)) {
        list_append(&head, &tail, R.udev_list_entry_get_name(it), R.udev_list_entry_get_value(it));
    }
    return head;
}

struct udev_list_entry *udev_list_entry_get_next(struct udev_list_entry *list_entry) {
    if (is_raw(list_entry, FAKE_MAGIC_LIST)) {
        return R.udev_list_entry_get_next ? R.udev_list_entry_get_next(list_entry) : NULL;
    }
    return list_entry ? list_entry->next : NULL;
}

const char *udev_list_entry_get_name(struct udev_list_entry *list_entry) {
    if (is_raw(list_entry, FAKE_MAGIC_LIST)) {
        return R.udev_list_entry_get_name ? R.udev_list_entry_get_name(list_entry) : NULL;
    }
    return list_entry ? list_entry->name : NULL;
}

const char *udev_list_entry_get_value(struct udev_list_entry *list_entry) {
    if (is_raw(list_entry, FAKE_MAGIC_LIST)) {
        return R.udev_list_entry_get_value ? R.udev_list_entry_get_value(list_entry) : NULL;
    }
    return list_entry ? list_entry->value : NULL;
}

struct udev_list_entry *udev_list_entry_get_by_name(struct udev_list_entry *list_entry, const char *name) {
    if (is_raw(list_entry, FAKE_MAGIC_LIST)) {
        return R.udev_list_entry_get_by_name ? R.udev_list_entry_get_by_name(list_entry, name) : NULL;
    }
    for (struct udev_list_entry *it = list_entry; it && name; it = it->next) {
        if (it->name && strcmp(it->name, name) == 0) {
            return it;
        }
    }
    return NULL;
}

/* ---- context ---- */

struct udev *udev_new(void) {
    fake_udev_logging_init_if_needed();
    pthread_once(&g_real_once, fake_udev_load_real);
    initialize_virtual_gamepads_data_if_needed();
    struct udev *udev = (struct udev *)calloc(1, sizeof(*udev));
    if (!udev) {
        return NULL;
    }
    udev->magic = FAKE_MAGIC_UDEV;
    udev->n_ref = 1;
    if (R.loaded) {
        udev->real = R.udev_new();
        if (!udev->real) {
            FAKE_UDEV_LOG_WARN("real udev_new failed; this context sees only the virtual gamepads");
        }
    }
    return udev;
}

struct udev *udev_ref(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_ref ? R.udev_ref(udev) : udev;
    }
    if (!udev) {
        return NULL;
    }
    udev->n_ref++;
    return udev;
}

struct udev *udev_unref(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_unref ? R.udev_unref(udev) : NULL;
    }
    if (!udev) {
        return NULL;
    }
    if (--udev->n_ref > 0) {
        return udev;
    }
    if (udev->real) {
        R.udev_unref(udev->real);
    }
    free(udev);
    return NULL;
}

void *udev_get_userdata(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_get_userdata ? R.udev_get_userdata(udev) : NULL;
    }
    return udev ? udev->userdata : NULL;
}

void udev_set_userdata(struct udev *udev, void *userdata) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        if (R.udev_set_userdata) R.udev_set_userdata(udev, userdata);
        return;
    }
    if (udev) {
        udev->userdata = userdata;
    }
}

void udev_set_log_fn(struct udev *udev,
                     void (*log_fn)(struct udev *udev,
                                    int priority, const char *file, int line, const char *fn,
                                    const char *format, va_list args)) {
    (void)udev; (void)log_fn;
}

int udev_get_log_priority(struct udev *udev) {
    (void)udev;
    return 0;
}

void udev_set_log_priority(struct udev *udev, int priority) {
    (void)udev; (void)priority;
}

/* ---- devices ---- */

static struct udev_device *device_alloc(struct udev *udev) {
    struct udev_device *dev = (struct udev_device *)calloc(1, sizeof(*dev));
    if (!dev) {
        return NULL;
    }
    dev->magic = FAKE_MAGIC_DEVICE;
    dev->udev_ctx = udev_ref(udev);
    dev->n_ref = 1;
    dev->node_type = VIRTUAL_TYPE_NONE;
    return dev;
}

static struct udev_device *virtual_device_new(struct udev *udev, const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    struct udev_device *dev = device_alloc(udev);
    if (!dev) {
        return NULL;
    }
    dev->gamepad_def = def;
    dev->node_type = type;
    FAKE_UDEV_LOG_DEBUG("virtual device %p for %s", (void *)dev, virtual_syspath(def, type));
    return dev;
}

/* Wraps a real device, taking over the caller's reference to it. */
static struct udev_device *real_device_wrap(struct udev *udev, struct udev_device *real) {
    if (!real) {
        return NULL;
    }
    struct udev_device *dev = device_alloc(udev);
    if (!dev) {
        R.udev_device_unref(real);
        return NULL;
    }
    dev->real = real;
    return dev;
}

/* Wraps a real device unless it is a hidden input node, in which case the
 * reference is dropped and NULL returned. */
static struct udev_device *real_device_wrap_visible(struct udev *udev, struct udev_device *real) {
    if (!real) {
        return NULL;
    }
    const char *subsystem = R.udev_device_get_subsystem(real);
    if (subsystem && strcmp(subsystem, "input") == 0 && real_input_sysname_hidden(R.udev_device_get_sysname(real))) {
        FAKE_UDEV_LOG_INFO("hiding real input node %s behind a virtual gamepad", R.udev_device_get_syspath(real));
        R.udev_device_unref(real);
        return NULL;
    }
    return real_device_wrap(udev, real);
}

static void device_free(struct udev_device *dev) {
    for (struct parent_cache *p = dev->parents; p;) {
        struct parent_cache *next = p->next;
        udev_device_unref(p->dev);
        free(p->key);
        free(p);
        p = next;
    }
    free_udev_list(dev->properties);
    free_udev_list(dev->sysattrs);
    free_udev_list(dev->devlinks);
    free_udev_list(dev->tags);
    free_udev_list(dev->current_tags);
    if (dev->real) {
        R.udev_device_unref(dev->real);
    }
    udev_unref(dev->udev_ctx);
    free(dev);
}

struct udev_device *udev_device_ref(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_ref ? R.udev_device_ref(udev_device) : udev_device;
    }
    if (!udev_device) {
        return NULL;
    }
    udev_device->n_ref++;
    return udev_device;
}

struct udev_device *udev_device_unref(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_unref ? R.udev_device_unref(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (--udev_device->n_ref > 0) {
        return udev_device;
    }
    device_free(udev_device);
    return NULL;
}

struct udev *udev_device_get_udev(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_udev ? R.udev_device_get_udev(udev_device) : NULL;
    }
    return udev_device ? udev_device->udev_ctx : NULL;
}

struct udev_device *udev_device_new_from_syspath(struct udev *udev, const char *syspath) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_device_new_from_syspath ? R.udev_device_new_from_syspath(udev, syspath) : NULL;
    }
    if (!udev || !syspath) {
        return NULL;
    }
    virtual_device_node_type_t type;
    const virtual_gamepad_definition_t *def = find_virtual_def_by_syspath(syspath, &type);
    if (def) {
        return virtual_device_new(udev, def, type);
    }
    if (!udev->real) {
        FAKE_UDEV_LOG_DEBUG("no device for syspath %s", syspath);
        return NULL;
    }
    return real_device_wrap_visible(udev, R.udev_device_new_from_syspath(udev->real, syspath));
}

struct udev_device *udev_device_new_from_devnum(struct udev *udev, char type, dev_t devnum) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_device_new_from_devnum ? R.udev_device_new_from_devnum(udev, type, devnum) : NULL;
    }
    if (!udev) {
        return NULL;
    }
    if (type == 'c' && major(devnum) == INPUT_MAJOR) {
        initialize_virtual_gamepads_data_if_needed();
        for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
            const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
            if (devnum == virtual_devnum(def, VIRTUAL_TYPE_JS)) {
                return virtual_device_new(udev, def, VIRTUAL_TYPE_JS);
            }
            if (devnum == virtual_devnum(def, VIRTUAL_TYPE_EVENT)) {
                return virtual_device_new(udev, def, VIRTUAL_TYPE_EVENT);
            }
        }
    }
    if (!udev->real) {
        return NULL;
    }
    return real_device_wrap_visible(udev, R.udev_device_new_from_devnum(udev->real, type, devnum));
}

struct udev_device *udev_device_new_from_subsystem_sysname(struct udev *udev, const char *subsystem, const char *sysname) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_device_new_from_subsystem_sysname ? R.udev_device_new_from_subsystem_sysname(udev, subsystem, sysname) : NULL;
    }
    if (!udev || !subsystem || !sysname) {
        return NULL;
    }
    initialize_virtual_gamepads_data_if_needed();
    static const virtual_device_node_type_t types[] = {VIRTUAL_TYPE_JS, VIRTUAL_TYPE_EVENT, VIRTUAL_TYPE_INPUT_PARENT, VIRTUAL_TYPE_USB_PARENT};
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        for (size_t t = 0; t < sizeof(types) / sizeof(types[0]); ++t) {
            if (strcmp(subsystem, virtual_subsystem(types[t])) == 0 && strcmp(sysname, virtual_sysname(def, types[t])) == 0) {
                return virtual_device_new(udev, def, types[t]);
            }
        }
    }
    if (!udev->real) {
        return NULL;
    }
    return real_device_wrap_visible(udev, R.udev_device_new_from_subsystem_sysname(udev->real, subsystem, sysname));
}

struct udev_device *udev_device_new_from_device_id(struct udev *udev, const char *id) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_device_new_from_device_id ? R.udev_device_new_from_device_id(udev, id) : NULL;
    }
    if (!udev || !id) {
        return NULL;
    }
    unsigned int maj, min;
    if ((id[0] == 'c' || id[0] == 'b') && sscanf(id + 1, "%u:%u", &maj, &min) == 2) {
        return udev_device_new_from_devnum(udev, id[0], makedev(maj, min));
    }
    if (!udev->real || !R.udev_device_new_from_device_id) {
        return NULL;
    }
    return real_device_wrap_visible(udev, R.udev_device_new_from_device_id(udev->real, id));
}

struct udev_device *udev_device_new_from_environment(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_device_new_from_environment ? R.udev_device_new_from_environment(udev) : NULL;
    }
    if (!udev || !udev->real) {
        return NULL;
    }
    return real_device_wrap_visible(udev, R.udev_device_new_from_environment(udev->real));
}

/* Forwards a device accessor for a real pointer or a wrapper around one;
 * falls through for a virtual node. */
#define DEVICE_PASSTHROUGH(dev, fn, dflt, ...) \
    do { \
        if (is_raw((dev), FAKE_MAGIC_DEVICE)) { \
            return R.fn ? R.fn((dev), ##__VA_ARGS__) : (dflt); \
        } \
        if ((dev) && (dev)->real) { \
            return R.fn ? R.fn((dev)->real, ##__VA_ARGS__) : (dflt); \
        } \
    } while (0)

const char *udev_device_get_syspath(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_syspath, NULL);
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    return virtual_syspath(udev_device->gamepad_def, udev_device->node_type);
}

const char *udev_device_get_sysname(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_sysname, NULL);
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    return virtual_sysname(udev_device->gamepad_def, udev_device->node_type);
}

/* The trailing digits of the sysname, as libudev defines the sysnum. */
const char *udev_device_get_sysnum(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_sysnum, NULL);
    const char *sysname = udev_device_get_sysname(udev_device);
    if (!sysname) {
        return NULL;
    }
    const char *end = sysname + strlen(sysname);
    const char *digits = end;
    while (digits > sysname && digits[-1] >= '0' && digits[-1] <= '9') {
        digits--;
    }
    return digits == end ? NULL : digits;
}

const char *udev_device_get_devpath(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_devpath, NULL);
    const char *syspath = udev_device_get_syspath(udev_device);
    return syspath ? syspath + strlen("/sys") : NULL;
}

const char *udev_device_get_subsystem(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_subsystem, NULL);
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    return virtual_subsystem(udev_device->node_type);
}

const char *udev_device_get_devtype(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_devtype, NULL);
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    return udev_device->node_type == VIRTUAL_TYPE_USB_PARENT ? "usb_device" : NULL;
}

const char *udev_device_get_devnode(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_devnode, NULL);
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: return udev_device->gamepad_def->js_devnode;
        case VIRTUAL_TYPE_EVENT: return udev_device->gamepad_def->event_devnode;
        default: return NULL;
    }
}

dev_t udev_device_get_devnum(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_devnum, 0);
    if (!udev_device || !udev_device->gamepad_def) {
        return 0;
    }
    return virtual_devnum(udev_device->gamepad_def, udev_device->node_type);
}

const char *udev_device_get_property_value(struct udev_device *udev_device, const char *key) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_property_value, NULL, key);
    if (!udev_device || !udev_device->gamepad_def || !key) {
        return NULL;
    }
    return virtual_property(udev_device->gamepad_def, udev_device->node_type, key);
}

const char *udev_device_get_sysattr_value(struct udev_device *udev_device, const char *sysattr) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_sysattr_value, NULL, sysattr);
    if (!udev_device || !udev_device->gamepad_def || !sysattr) {
        return NULL;
    }
    const key_value_pair_t *table = virtual_sysattr_table(udev_device->gamepad_def, udev_device->node_type);
    for (int i = 0; table && table[i].name != NULL; ++i) {
        if (strcmp(table[i].name, sysattr) == 0) {
            return table[i].value;
        }
    }
    return NULL;
}

int udev_device_set_sysattr_value(struct udev_device *udev_device, const char *sysattr, const char *value) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_set_sysattr_value, -ENOSYS, sysattr, value);
    return udev_device ? 0 : -EINVAL;
}

const char *udev_device_get_driver(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_driver, NULL);
    /* Firefox keys its X/Y (BTN_WEST/BTN_NORTH) swap correction on the "xpad"
     * driver a real wired Xbox 360 pad binds; without it on the USB parent the
     * pad shows X/Y swapped. Button codes are unchanged for js/evdev consumers. */
    if (udev_device && udev_device->node_type == VIRTUAL_TYPE_USB_PARENT) {
        return "xpad";
    }
    return NULL;
}

const char *udev_device_get_action(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_action, NULL);
    if (!udev_device) {
        return NULL;
    }
    // Monitor-delivered nodes carry their action; enumerated ones report "add".
    return udev_device->action ? udev_device->action : "add";
}

unsigned long long int udev_device_get_seqnum(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_seqnum, 0);
    return 0;
}

unsigned long long int udev_device_get_usec_since_initialized(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_usec_since_initialized, 0);
    return 0;
}

int udev_device_get_is_initialized(struct udev_device *udev_device) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_get_is_initialized, 1);
    return 1;
}

int udev_device_has_tag(struct udev_device *udev_device, const char *tag) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_has_tag, 0, tag);
    return 0;
}

int udev_device_has_current_tag(struct udev_device *udev_device, const char *tag) {
    DEVICE_PASSTHROUGH(udev_device, udev_device_has_current_tag, 0, tag);
    return 0;
}

/* The cached parent under `key`, or NULL. */
static struct udev_device *parent_cache_get(struct udev_device *dev, const char *key) {
    for (struct parent_cache *p = dev->parents; p; p = p->next) {
        if (strcmp(p->key, key) == 0) {
            return p->dev;
        }
    }
    return NULL;
}

/* Stores `parent` (ownership passes to the cache) and returns it. */
static struct udev_device *parent_cache_put(struct udev_device *dev, const char *key, struct udev_device *parent) {
    if (!parent) {
        return NULL;
    }
    struct parent_cache *entry = (struct parent_cache *)calloc(1, sizeof(*entry));
    if (!entry || !(entry->key = strdup(key))) {
        free(entry);
        return parent; // uncached: the reference leaks rather than the lookup failing
    }
    entry->dev = parent;
    entry->next = dev->parents;
    dev->parents = entry;
    return parent;
}

/* The virtual parent of a node for the given criteria: the input parent of a
 * js or event node, the usb_device parent of an input parent. A NULL
 * subsystem means the direct parent whatever it is. */
static struct udev_device *virtual_parent(struct udev_device *dev, const char *subsystem, const char *devtype) {
    const virtual_gamepad_definition_t *def = dev->gamepad_def;
    bool devtype_empty = devtype == NULL || devtype[0] == '\0';
    if (dev->node_type == VIRTUAL_TYPE_JS || dev->node_type == VIRTUAL_TYPE_EVENT) {
        if (!subsystem || (strcmp(subsystem, "input") == 0 && devtype_empty)) {
            return virtual_device_new(dev->udev_ctx, def, VIRTUAL_TYPE_INPUT_PARENT);
        }
        if (strcmp(subsystem, "usb") == 0 && (devtype_empty || strcmp(devtype, "usb_device") == 0)) {
            return virtual_device_new(dev->udev_ctx, def, VIRTUAL_TYPE_USB_PARENT);
        }
    } else if (dev->node_type == VIRTUAL_TYPE_INPUT_PARENT) {
        if (!subsystem || (strcmp(subsystem, "usb") == 0 && (devtype_empty || strcmp(devtype, "usb_device") == 0))) {
            return virtual_device_new(dev->udev_ctx, def, VIRTUAL_TYPE_USB_PARENT);
        }
    }
    return NULL;
}

/* Wraps a parent the real library returned without a reference of its own. */
static struct udev_device *real_parent_wrap(struct udev_device *dev, struct udev_device *real_parent) {
    if (!real_parent) {
        return NULL;
    }
    return real_device_wrap(dev->udev_ctx, R.udev_device_ref(real_parent));
}

struct udev_device *udev_device_get_parent(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_parent ? R.udev_device_get_parent(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    struct udev_device *cached = parent_cache_get(udev_device, "*");
    if (cached) {
        return cached;
    }
    struct udev_device *parent = udev_device->real
        ? real_parent_wrap(udev_device, R.udev_device_get_parent(udev_device->real))
        : virtual_parent(udev_device, NULL, NULL);
    return parent_cache_put(udev_device, "*", parent);
}

struct udev_device *udev_device_get_parent_with_subsystem_devtype(struct udev_device *udev_device, const char *subsystem, const char *devtype) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_parent_with_subsystem_devtype ? R.udev_device_get_parent_with_subsystem_devtype(udev_device, subsystem, devtype) : NULL;
    }
    if (!udev_device || !subsystem) {
        return NULL;
    }
    char key[128];
    snprintf(key, sizeof(key), "%s/%s", subsystem, devtype ? devtype : "");
    struct udev_device *cached = parent_cache_get(udev_device, key);
    if (cached) {
        return cached;
    }
    struct udev_device *parent = udev_device->real
        ? real_parent_wrap(udev_device, R.udev_device_get_parent_with_subsystem_devtype(udev_device->real, subsystem, devtype))
        : virtual_parent(udev_device, subsystem, devtype);
    return parent_cache_put(udev_device, key, parent);
}

struct udev_list_entry *udev_device_get_properties_list_entry(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_properties_list_entry ? R.udev_device_get_properties_list_entry(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (udev_device->properties_built) {
        return udev_device->properties;
    }
    udev_device->properties_built = true;
    if (udev_device->real) {
        udev_device->properties = list_copy_real(R.udev_device_get_properties_list_entry(udev_device->real));
        return udev_device->properties;
    }
    if (!udev_device->gamepad_def) {
        return NULL;
    }
    struct udev_list_entry *tail = NULL;
    const key_value_pair_t *table = virtual_property_table(udev_device->gamepad_def, udev_device->node_type);
    for (int i = 0; table && table[i].name != NULL; ++i) {
        list_append(&udev_device->properties, &tail, table[i].name, table[i].value);
    }
    static const char *const synthesized[] = {"SUBSYSTEM", "DEVPATH"};
    for (size_t i = 0; i < sizeof(synthesized) / sizeof(synthesized[0]); ++i) {
        list_append(&udev_device->properties, &tail, synthesized[i],
                    virtual_property(udev_device->gamepad_def, udev_device->node_type, synthesized[i]));
    }
    return udev_device->properties;
}

struct udev_list_entry *udev_device_get_sysattr_list_entry(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_sysattr_list_entry ? R.udev_device_get_sysattr_list_entry(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (udev_device->sysattrs_built) {
        return udev_device->sysattrs;
    }
    udev_device->sysattrs_built = true;
    if (udev_device->real) {
        udev_device->sysattrs = list_copy_real(R.udev_device_get_sysattr_list_entry(udev_device->real));
        return udev_device->sysattrs;
    }
    if (!udev_device->gamepad_def) {
        return NULL;
    }
    struct udev_list_entry *tail = NULL;
    const key_value_pair_t *table = virtual_sysattr_table(udev_device->gamepad_def, udev_device->node_type);
    for (int i = 0; table && table[i].name != NULL; ++i) {
        list_append(&udev_device->sysattrs, &tail, table[i].name, NULL);
    }
    return udev_device->sysattrs;
}

struct udev_list_entry *udev_device_get_devlinks_list_entry(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_devlinks_list_entry ? R.udev_device_get_devlinks_list_entry(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (udev_device->devlinks_built) {
        return udev_device->devlinks;
    }
    udev_device->devlinks_built = true;
    if (udev_device->real) {
        udev_device->devlinks = list_copy_real(R.udev_device_get_devlinks_list_entry(udev_device->real));
    }
    return udev_device->devlinks;
}

struct udev_list_entry *udev_device_get_tags_list_entry(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_tags_list_entry ? R.udev_device_get_tags_list_entry(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (udev_device->tags_built) {
        return udev_device->tags;
    }
    udev_device->tags_built = true;
    if (udev_device->real) {
        udev_device->tags = list_copy_real(R.udev_device_get_tags_list_entry(udev_device->real));
    }
    return udev_device->tags;
}

struct udev_list_entry *udev_device_get_current_tags_list_entry(struct udev_device *udev_device) {
    if (is_raw(udev_device, FAKE_MAGIC_DEVICE)) {
        return R.udev_device_get_current_tags_list_entry ? R.udev_device_get_current_tags_list_entry(udev_device) : NULL;
    }
    if (!udev_device) {
        return NULL;
    }
    if (udev_device->current_tags_built) {
        return udev_device->current_tags;
    }
    udev_device->current_tags_built = true;
    if (udev_device->real && R.udev_device_get_current_tags_list_entry) {
        udev_device->current_tags = list_copy_real(R.udev_device_get_current_tags_list_entry(udev_device->real));
    }
    return udev_device->current_tags;
}

/* ---- enumerate ---- */

struct udev_enumerate *udev_enumerate_new(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_enumerate_new ? R.udev_enumerate_new(udev) : NULL;
    }
    if (!udev) {
        return NULL;
    }
    struct udev_enumerate *e = (struct udev_enumerate *)calloc(1, sizeof(*e));
    if (!e) {
        return NULL;
    }
    e->magic = FAKE_MAGIC_ENUMERATE;
    e->udev_ctx = udev_ref(udev);
    e->n_ref = 1;
    e->parent_type = VIRTUAL_TYPE_NONE;
    if (udev->real) {
        e->real = R.udev_enumerate_new(udev->real);
    }
    return e;
}

struct udev_enumerate *udev_enumerate_ref(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_ref ? R.udev_enumerate_ref(udev_enumerate) : udev_enumerate;
    }
    if (!udev_enumerate) {
        return NULL;
    }
    udev_enumerate->n_ref++;
    return udev_enumerate;
}

struct udev_enumerate *udev_enumerate_unref(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_unref ? R.udev_enumerate_unref(udev_enumerate) : NULL;
    }
    if (!udev_enumerate) {
        return NULL;
    }
    if (--udev_enumerate->n_ref > 0) {
        return udev_enumerate;
    }
    if (udev_enumerate->real) {
        R.udev_enumerate_unref(udev_enumerate->real);
    }
    free_udev_list(udev_enumerate->results);
    free_udev_list(udev_enumerate->property_filters);
    free_udev_list(udev_enumerate->extra_syspaths);
    udev_unref(udev_enumerate->udev_ctx);
    free(udev_enumerate);
    return NULL;
}

struct udev *udev_enumerate_get_udev(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_get_udev ? R.udev_enumerate_get_udev(udev_enumerate) : NULL;
    }
    return udev_enumerate ? udev_enumerate->udev_ctx : NULL;
}

/* Forwards an enumerate call for a real pointer, then to the wrapper's real
 * counterpart, returning the real result only for the former. */
#define ENUMERATE_PASSTHROUGH(e, fn, ...) \
    do { \
        if (is_raw((e), FAKE_MAGIC_ENUMERATE)) { \
            return R.fn ? R.fn((e), ##__VA_ARGS__) : -EINVAL; \
        } \
        if (!(e)) { \
            return -EINVAL; \
        } \
        if ((e)->real) { \
            R.fn((e)->real, ##__VA_ARGS__); \
        } \
    } while (0)

int udev_enumerate_add_match_subsystem(struct udev_enumerate *udev_enumerate, const char *subsystem) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_subsystem, subsystem);
    if (!subsystem) {
        return -EINVAL;
    }
    udev_enumerate->subsystem_filtered = true;
    if (strcmp(subsystem, "input") == 0) {
        udev_enumerate->input_matched = true;
    }
    return 0;
}

int udev_enumerate_add_nomatch_subsystem(struct udev_enumerate *udev_enumerate, const char *subsystem) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_nomatch_subsystem, subsystem);
    if (subsystem && strcmp(subsystem, "input") == 0) {
        udev_enumerate->input_excluded = true;
    }
    return 0;
}

int udev_enumerate_add_match_sysname(struct udev_enumerate *udev_enumerate, const char *sysname) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_sysname, sysname);
    if (!sysname) {
        return -EINVAL;
    }
    udev_enumerate->sysname_filtered = true;
    strncpy(udev_enumerate->sysname_pattern, sysname, sizeof(udev_enumerate->sysname_pattern) - 1);
    udev_enumerate->sysname_pattern[sizeof(udev_enumerate->sysname_pattern) - 1] = '\0';
    return 0;
}

/* Prepends a property filter. A NULL value matches on the property's presence
 * alone; a NULL property is a no-op returning 0, as in libudev. Results are
 * rebuilt by the next scan_devices. */
int udev_enumerate_add_match_property(struct udev_enumerate *udev_enumerate, const char *property, const char *value) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_property, property, value);
    if (!property) {
        return 0;
    }
    struct udev_list_entry *tail = NULL;
    struct udev_list_entry *head = NULL;
    if (!list_append(&head, &tail, property, value)) {
        return -ENOMEM;
    }
    head->next = udev_enumerate->property_filters;
    udev_enumerate->property_filters = head;
    return 0;
}

int udev_enumerate_add_match_sysattr(struct udev_enumerate *udev_enumerate, const char *sysattr, const char *value) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_sysattr, sysattr, value);
    return 0;
}

int udev_enumerate_add_nomatch_sysattr(struct udev_enumerate *udev_enumerate, const char *sysattr, const char *value) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_nomatch_sysattr, sysattr, value);
    return 0;
}

int udev_enumerate_add_match_tag(struct udev_enumerate *udev_enumerate, const char *tag) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_tag, tag);
    return 0;
}

int udev_enumerate_add_match_is_initialized(struct udev_enumerate *udev_enumerate) {
    ENUMERATE_PASSTHROUGH(udev_enumerate, udev_enumerate_add_match_is_initialized);
    return 0;
}

int udev_enumerate_add_match_parent(struct udev_enumerate *udev_enumerate, struct udev_device *parent) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_add_match_parent ? R.udev_enumerate_add_match_parent(udev_enumerate, parent) : -EINVAL;
    }
    if (!udev_enumerate || !parent) {
        return -EINVAL;
    }
    if (is_raw(parent, FAKE_MAGIC_DEVICE) || parent->real) {
        struct udev_device *real_parent = is_raw(parent, FAKE_MAGIC_DEVICE) ? parent : parent->real;
        if (udev_enumerate->real) {
            R.udev_enumerate_add_match_parent(udev_enumerate->real, real_parent);
        }
        // The virtual nodes descend from no real device.
        udev_enumerate->input_excluded = true;
        return 0;
    }
    // A virtual parent has no real descendants.
    udev_enumerate->real_muted = true;
    udev_enumerate->parent_def = parent->gamepad_def;
    udev_enumerate->parent_type = parent->node_type;
    return 0;
}

int udev_enumerate_add_syspath(struct udev_enumerate *udev_enumerate, const char *syspath) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_add_syspath ? R.udev_enumerate_add_syspath(udev_enumerate, syspath) : -EINVAL;
    }
    if (!udev_enumerate || !syspath) {
        return -EINVAL;
    }
    virtual_device_node_type_t type;
    if (find_virtual_def_by_syspath(syspath, &type)) {
        struct udev_list_entry *tail = udev_enumerate->extra_syspaths;
        while (tail && tail->next) {
            tail = tail->next;
        }
        return list_append(&udev_enumerate->extra_syspaths, &tail, syspath, NULL) ? 0 : -ENOMEM;
    }
    if (udev_enumerate->real) {
        return R.udev_enumerate_add_syspath(udev_enumerate->real, syspath);
    }
    return 0;
}

int udev_enumerate_add_match_sysnum(struct udev_enumerate *udev_enumerate, const char *sysnum) {
    (void)sysnum;
    return udev_enumerate ? 0 : -EINVAL;
}

int udev_enumerate_add_match_devicenode(struct udev_enumerate *udev_enumerate, const char *devnode) {
    (void)devnode;
    return udev_enumerate ? 0 : -EINVAL;
}

/* True when every filter matches a property of the node; a filter with a NULL
 * value matches on the property's presence. */
static bool virtual_matches_property_filters(const virtual_gamepad_definition_t *def,
                                             virtual_device_node_type_t node_type,
                                             struct udev_list_entry *filters) {
    for (struct udev_list_entry *filter = filters; filter != NULL; filter = filter->next) {
        const char *value = virtual_property(def, node_type, filter->name);
        if (!value || (filter->value && strcmp(value, filter->value) != 0)) {
            return false;
        }
    }
    return true;
}

/* Whether a virtual node passes the enumeration's sysname, parent and
 * property filters. */
static bool virtual_matches(const struct udev_enumerate *e, const virtual_gamepad_definition_t *def, virtual_device_node_type_t type) {
    if (e->sysname_filtered && fnmatch(e->sysname_pattern, virtual_sysname(def, type), 0) != 0) {
        return false;
    }
    if (e->parent_def) {
        if (e->parent_def != def) {
            return false;
        }
        bool descendant = e->parent_type == VIRTUAL_TYPE_USB_PARENT
            ? type != VIRTUAL_TYPE_USB_PARENT
            : (e->parent_type == VIRTUAL_TYPE_INPUT_PARENT && (type == VIRTUAL_TYPE_JS || type == VIRTUAL_TYPE_EVENT));
        if (!descendant) {
            return false;
        }
    }
    return virtual_matches_property_filters(def, type, e->property_filters);
}

/* Appends the virtual nodes the enumeration admits: the js and event nodes
 * whose sockets are bound, and an input parent a sysname pattern or a parent
 * match selects. */
static void enumerate_add_virtual(struct udev_enumerate *e, struct udev_list_entry **head, struct udev_list_entry **tail) {
    if (e->input_excluded || (e->subsystem_filtered && !e->input_matched)) {
        return;
    }
    initialize_virtual_gamepads_data_if_needed();
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        bool js_live = sysname_socket_live(def->js_sysname);
        bool event_live = sysname_socket_live(def->event_sysname);
        if (!js_live && !event_live) {
            continue;
        }
        if (js_live && virtual_matches(e, def, VIRTUAL_TYPE_JS)) {
            list_append(head, tail, def->js_syspath, NULL);
        }
        if (event_live && virtual_matches(e, def, VIRTUAL_TYPE_EVENT)) {
            list_append(head, tail, def->event_syspath, NULL);
        }
        if ((e->sysname_filtered || e->parent_def) && virtual_matches(e, def, VIRTUAL_TYPE_INPUT_PARENT)) {
            list_append(head, tail, def->input_parent_syspath, NULL);
        }
    }
}

/* Rebuilds the result list: the real library's results less the hidden input
 * nodes, then the virtual nodes and the virtual syspaths added explicitly. */
int udev_enumerate_scan_devices(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_scan_devices ? R.udev_enumerate_scan_devices(udev_enumerate) : -EINVAL;
    }
    if (!udev_enumerate) {
        return -EINVAL;
    }
    free_udev_list(udev_enumerate->results);
    udev_enumerate->results = NULL;
    struct udev_list_entry *head = NULL, *tail = NULL;
    int real_count = 0;
    if (udev_enumerate->real && !udev_enumerate->real_muted) {
        int rc = R.udev_enumerate_scan_devices(udev_enumerate->real);
        if (rc < 0) {
            FAKE_UDEV_LOG_WARN("real scan_devices failed: %s", strerror(-rc));
        }
        for (struct udev_list_entry *it = R.udev_enumerate_get_list_entry(udev_enumerate->real); it; it = R.udev_list_entry_get_next(it)) {
            const char *syspath = R.udev_list_entry_get_name(it);
            if (real_syspath_hidden(syspath)) {
                continue;
            }
            if (list_append(&head, &tail, syspath, NULL)) {
                real_count++;
            }
        }
    }
    enumerate_add_virtual(udev_enumerate, &head, &tail);
    for (struct udev_list_entry *it = udev_enumerate->extra_syspaths; it; it = it->next) {
        list_append(&head, &tail, it->name, NULL);
    }
    udev_enumerate->results = head;
    int total = 0;
    for (struct udev_list_entry *it = head; it; it = it->next) {
        total++;
    }
    FAKE_UDEV_LOG_INFO("scan: %d real, %d virtual (subsystem_filtered=%d input_matched=%d sysname='%s')",
                       real_count, total - real_count, udev_enumerate->subsystem_filtered,
                       udev_enumerate->input_matched, udev_enumerate->sysname_filtered ? udev_enumerate->sysname_pattern : "");
    return 0;
}

int udev_enumerate_scan_subsystems(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_scan_subsystems ? R.udev_enumerate_scan_subsystems(udev_enumerate) : -EINVAL;
    }
    if (!udev_enumerate) {
        return -EINVAL;
    }
    free_udev_list(udev_enumerate->results);
    udev_enumerate->results = NULL;
    if (!udev_enumerate->real) {
        return 0;
    }
    int rc = R.udev_enumerate_scan_subsystems(udev_enumerate->real);
    udev_enumerate->results = list_copy_real(R.udev_enumerate_get_list_entry(udev_enumerate->real));
    return rc;
}

int udev_enumerate_scan_children(struct udev_enumerate *udev_enumerate, struct udev_device *parent) {
    if (!udev_enumerate || !parent) {
        return -EINVAL;
    }
    free_udev_list(udev_enumerate->results);
    udev_enumerate->results = NULL;
    return 0;
}

struct udev_list_entry *udev_enumerate_get_list_entry(struct udev_enumerate *udev_enumerate) {
    if (is_raw(udev_enumerate, FAKE_MAGIC_ENUMERATE)) {
        return R.udev_enumerate_get_list_entry ? R.udev_enumerate_get_list_entry(udev_enumerate) : NULL;
    }
    return udev_enumerate ? udev_enumerate->results : NULL;
}

/* ---- monitor ---- */

/* Maps an interposer socket basename ("selkies_<sysname>.sock", e.g.
 * "selkies_js0.sock" or "selkies_event1000.sock") back to its js or event node. */
static bool find_node_by_socket_name(const char *name,
                                     const virtual_gamepad_definition_t **def_out,
                                     virtual_device_node_type_t *type_out) {
    if (!name || strncmp(name, "selkies_", strlen("selkies_")) != 0) {
        return false;
    }
    const char *sysname = name + strlen("selkies_");
    const char *suffix = strstr(sysname, ".sock");
    if (!suffix || suffix[strlen(".sock")] != '\0') {
        return false;
    }
    char bare[64];
    size_t len = (size_t)(suffix - sysname);
    if (len == 0 || len >= sizeof(bare)) {
        return false;
    }
    memcpy(bare, sysname, len);
    bare[len] = '\0';
    *def_out = find_virtual_node_by_sysname(bare, type_out);
    return *def_out != NULL;
}

/* Backs the monitor with an inotify watch on the socket directory so the
 * interposer's socket create/delete surface as add/remove hotplug events, and
 * with the real library's monitor for everything else. The consumer-visible
 * fd is an epoll set over the inotify fd, the real monitor's fd and an
 * eventfd kept armed while undispensed matching records sit in evbuf: one
 * inotify read() can drain several coalesced records while receive_device
 * dispenses one per call, so raw inotify readability would understate pending
 * events and a poll()-gated consumer would stop calling receive_device
 * early. */
struct udev_monitor *udev_monitor_new_from_netlink(struct udev *udev, const char *name) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_monitor_new_from_netlink ? R.udev_monitor_new_from_netlink(udev, name) : NULL;
    }
    if (!udev) {
        return NULL;
    }
    struct udev_monitor *mon = (struct udev_monitor *)calloc(1, sizeof(*mon));
    if (!mon) {
        return NULL;
    }
    mon->magic = FAKE_MAGIC_MONITOR;
    mon->udev_ctx = udev_ref(udev);
    mon->n_ref = 1;
    mon->watch_wd = -1;
    mon->real_fd = -1;
    if (udev->real) {
        mon->real = R.udev_monitor_new_from_netlink(udev->real, name);
        if (mon->real) {
            mon->real_fd = R.udev_monitor_get_fd(mon->real);
        } else {
            FAKE_UDEV_LOG_WARN("real monitor unavailable; only gamepad hotplug is reported");
        }
    }
    mon->inotify_fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (mon->inotify_fd >= 0) {
        const char *sock_dir = fake_udev_socket_dir();
        mon->watch_wd = inotify_add_watch(mon->inotify_fd, sock_dir, IN_CREATE | IN_DELETE);
        if (mon->watch_wd < 0) {
            FAKE_UDEV_LOG_WARN("inotify_add_watch(%s) failed: %s", sock_dir, strerror(errno));
        }
    } else {
        FAKE_UDEV_LOG_WARN("inotify_init1 failed: %s; gamepad hotplug disabled", strerror(errno));
    }
    mon->evbuf_efd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    mon->fd = epoll_create1(EPOLL_CLOEXEC);
    if (mon->fd >= 0) {
        int fds[] = {mon->inotify_fd, mon->evbuf_efd, mon->real_fd};
        for (size_t i = 0; i < sizeof(fds) / sizeof(fds[0]); ++i) {
            if (fds[i] < 0) {
                continue;
            }
            struct epoll_event epev;
            memset(&epev, 0, sizeof(epev));
            epev.events = EPOLLIN;
            epev.data.fd = fds[i];
            epoll_ctl(mon->fd, EPOLL_CTL_ADD, fds[i], &epev);
        }
    } else {
        // No epoll set: udev_monitor_get_fd hands out the raw inotify fd.
        FAKE_UDEV_LOG_WARN("epoll_create1 failed: %s", strerror(errno));
    }
    return mon;
}

struct udev_monitor *udev_monitor_ref(struct udev_monitor *udev_monitor) {
    if (is_raw(udev_monitor, FAKE_MAGIC_MONITOR)) {
        return R.udev_monitor_ref ? R.udev_monitor_ref(udev_monitor) : udev_monitor;
    }
    if (!udev_monitor) {
        return NULL;
    }
    udev_monitor->n_ref++;
    return udev_monitor;
}

struct udev_monitor *udev_monitor_unref(struct udev_monitor *udev_monitor) {
    if (is_raw(udev_monitor, FAKE_MAGIC_MONITOR)) {
        return R.udev_monitor_unref ? R.udev_monitor_unref(udev_monitor) : NULL;
    }
    if (!udev_monitor) {
        return NULL;
    }
    if (--udev_monitor->n_ref > 0) {
        return udev_monitor;
    }
    if (udev_monitor->fd >= 0) {
        close(udev_monitor->fd);
    }
    if (udev_monitor->inotify_fd >= 0) {
        close(udev_monitor->inotify_fd);
    }
    if (udev_monitor->evbuf_efd >= 0) {
        close(udev_monitor->evbuf_efd);
    }
    if (udev_monitor->real) {
        R.udev_monitor_unref(udev_monitor->real);
    }
    udev_unref(udev_monitor->udev_ctx);
    free(udev_monitor);
    return NULL;
}

struct udev *udev_monitor_get_udev(struct udev_monitor *udev_monitor) {
    if (is_raw(udev_monitor, FAKE_MAGIC_MONITOR)) {
        return R.udev_monitor_get_udev ? R.udev_monitor_get_udev(udev_monitor) : NULL;
    }
    return udev_monitor ? udev_monitor->udev_ctx : NULL;
}

/* Forwards a monitor call for a real pointer, then to the wrapper's real
 * counterpart, returning the real result only for the former. */
#define MONITOR_PASSTHROUGH(mon, fn, ...) \
    do { \
        if (is_raw((mon), FAKE_MAGIC_MONITOR)) { \
            return R.fn ? R.fn((mon), ##__VA_ARGS__) : -EINVAL; \
        } \
        if (!(mon)) { \
            return -EINVAL; \
        } \
        if ((mon)->real) { \
            R.fn((mon)->real, ##__VA_ARGS__); \
        } \
    } while (0)

int udev_monitor_enable_receiving(struct udev_monitor *udev_monitor) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_enable_receiving);
    return 0;
}

int udev_monitor_set_receive_buffer_size(struct udev_monitor *udev_monitor, int size) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_set_receive_buffer_size, size);
    return 0;
}

int udev_monitor_get_fd(struct udev_monitor *udev_monitor) {
    if (is_raw(udev_monitor, FAKE_MAGIC_MONITOR)) {
        return R.udev_monitor_get_fd ? R.udev_monitor_get_fd(udev_monitor) : -1;
    }
    if (!udev_monitor) {
        return -1;
    }
    if (udev_monitor->fd >= 0) {
        return udev_monitor->fd;
    }
    return udev_monitor->inotify_fd; // degraded: no epoll set available
}

int udev_monitor_filter_add_match_subsystem_devtype(struct udev_monitor *udev_monitor, const char *subsystem, const char *devtype) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_filter_add_match_subsystem_devtype, subsystem, devtype);
    if (subsystem) {
        udev_monitor->subsystem_filtered = true;
        if (strcmp(subsystem, "input") == 0 && (!devtype || devtype[0] == '\0')) {
            udev_monitor->input_matched = true;
        }
    }
    return 0;
}

int udev_monitor_filter_add_match_tag(struct udev_monitor *udev_monitor, const char *tag) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_filter_add_match_tag, tag);
    return 0;
}

int udev_monitor_filter_update(struct udev_monitor *udev_monitor) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_filter_update);
    return 0;
}

int udev_monitor_filter_remove(struct udev_monitor *udev_monitor) {
    MONITOR_PASSTHROUGH(udev_monitor, udev_monitor_filter_remove);
    udev_monitor->subsystem_filtered = false;
    udev_monitor->input_matched = false;
    return 0;
}

static bool monitor_admits_virtual(const struct udev_monitor *mon) {
    return !mon->subsystem_filtered || mon->input_matched;
}

// True if any undispensed record in evbuf would be delivered by receive_device.
// Mirrors the match logic of monitor_dispense_virtual below.
static bool evbuf_has_matching_record(const struct udev_monitor *mon) {
    if (!monitor_admits_virtual(mon)) {
        return false;
    }
    size_t off = mon->evbuf_off;
    while (off + sizeof(struct inotify_event) <= mon->evbuf_len) {
        const struct inotify_event *ev = (const struct inotify_event *)(mon->evbuf + off);
        size_t rec = sizeof(struct inotify_event) + ev->len;
        if (off + rec > mon->evbuf_len) {
            break; // trailing partial record, never dispensed
        }
        off += rec;
        if (!(ev->mask & (IN_CREATE | IN_DELETE)) || ev->len == 0) {
            continue;
        }
        const virtual_gamepad_definition_t *def = NULL;
        virtual_device_node_type_t type = VIRTUAL_TYPE_NONE;
        if (find_node_by_socket_name(ev->name, &def, &type)) {
            return true;
        }
    }
    return false;
}

// Keep the consumer-visible fd's readability equal to "receive_device will
// yield an event": arm the eventfd while evbuf still holds an undispensed
// matching record, drain it once the buffer is exhausted. Fresh inotify data
// and real netlink data surface through the epoll set on their own.
static void monitor_sync_readable(struct udev_monitor *mon) {
    if (mon->evbuf_efd < 0) {
        return;
    }
    bool want_armed = evbuf_has_matching_record(mon);
    if (want_armed && !mon->evbuf_efd_armed) {
        if (eventfd_write(mon->evbuf_efd, 1) == 0) {
            mon->evbuf_efd_armed = true;
        }
    } else if (!want_armed && mon->evbuf_efd_armed) {
        eventfd_t val;
        if (eventfd_read(mon->evbuf_efd, &val) == 0 || errno == EAGAIN) {
            mon->evbuf_efd_armed = false;
        }
    }
}

// Dispense one virtual device per call. Records are buffered so any left over
// after a match survive to the next call; non-matching records are drained
// in-place.
static struct udev_device *monitor_dispense_virtual(struct udev_monitor *mon) {
    if (mon->inotify_fd < 0) {
        return NULL;
    }
    for (;;) {
        if (mon->evbuf_off + sizeof(struct inotify_event) > mon->evbuf_len) {
            ssize_t n = read(mon->inotify_fd, mon->evbuf, sizeof(mon->evbuf));
            if (n <= 0) {
                return NULL; // EAGAIN (nothing pending) or error/EOF
            }
            mon->evbuf_len = (size_t)n;
            mon->evbuf_off = 0;
            if (mon->evbuf_off + sizeof(struct inotify_event) > mon->evbuf_len) {
                return NULL;
            }
        }

        struct inotify_event *ev = (struct inotify_event *)(mon->evbuf + mon->evbuf_off);
        size_t rec = sizeof(struct inotify_event) + ev->len;
        if (mon->evbuf_off + rec > mon->evbuf_len) {
            mon->evbuf_off = mon->evbuf_len; // drop trailing partial, force refill
            continue;
        }
        mon->evbuf_off += rec;

        const char *action = (ev->mask & IN_CREATE) ? "add" : (ev->mask & IN_DELETE) ? "remove" : NULL;
        if (!action || ev->len == 0 || !monitor_admits_virtual(mon)) {
            continue;
        }
        const virtual_gamepad_definition_t *def = NULL;
        virtual_device_node_type_t type = VIRTUAL_TYPE_NONE;
        if (!find_node_by_socket_name(ev->name, &def, &type)) {
            continue;
        }
        struct udev_device *dev = virtual_device_new(mon->udev_ctx, def, type);
        if (!dev) {
            continue;
        }
        dev->action = action;
        FAKE_UDEV_LOG_INFO("hotplug '%s' for socket '%s' -> %s", action, ev->name, virtual_syspath(def, type));
        return dev;
    }
}

struct udev_device *udev_monitor_receive_device(struct udev_monitor *udev_monitor) {
    if (is_raw(udev_monitor, FAKE_MAGIC_MONITOR)) {
        return R.udev_monitor_receive_device ? R.udev_monitor_receive_device(udev_monitor) : NULL;
    }
    if (!udev_monitor) {
        return NULL;
    }
    struct udev_device *dev = monitor_dispense_virtual(udev_monitor);
    monitor_sync_readable(udev_monitor);
    while (!dev && udev_monitor->real) {
        struct udev_device *real = R.udev_monitor_receive_device(udev_monitor->real);
        if (!real) {
            break;
        }
        dev = real_device_wrap_visible(udev_monitor->udev_ctx, real);
    }
    return dev;
}

/* ---- hwdb ---- */

struct udev_hwdb *udev_hwdb_new(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_hwdb_new ? R.udev_hwdb_new(udev) : NULL;
    }
    if (!udev || !udev->real || !R.udev_hwdb_new) {
        return NULL;
    }
    struct udev_hwdb *real = R.udev_hwdb_new(udev->real);
    if (!real) {
        return NULL;
    }
    struct udev_hwdb *hwdb = (struct udev_hwdb *)calloc(1, sizeof(*hwdb));
    if (!hwdb) {
        R.udev_hwdb_unref(real);
        return NULL;
    }
    hwdb->magic = FAKE_MAGIC_HWDB;
    hwdb->n_ref = 1;
    hwdb->real = real;
    return hwdb;
}

struct udev_hwdb *udev_hwdb_ref(struct udev_hwdb *udev_hwdb) {
    if (is_raw(udev_hwdb, FAKE_MAGIC_HWDB)) {
        return R.udev_hwdb_ref ? R.udev_hwdb_ref(udev_hwdb) : udev_hwdb;
    }
    if (udev_hwdb) {
        udev_hwdb->n_ref++;
    }
    return udev_hwdb;
}

struct udev_hwdb *udev_hwdb_unref(struct udev_hwdb *udev_hwdb) {
    if (is_raw(udev_hwdb, FAKE_MAGIC_HWDB)) {
        return R.udev_hwdb_unref ? R.udev_hwdb_unref(udev_hwdb) : NULL;
    }
    if (!udev_hwdb) {
        return NULL;
    }
    if (--udev_hwdb->n_ref > 0) {
        return udev_hwdb;
    }
    free_udev_list(udev_hwdb->properties);
    R.udev_hwdb_unref(udev_hwdb->real);
    free(udev_hwdb);
    return NULL;
}

struct udev_list_entry *udev_hwdb_get_properties_list_entry(struct udev_hwdb *hwdb, const char *modalias, unsigned int flags) {
    if (is_raw(hwdb, FAKE_MAGIC_HWDB)) {
        return R.udev_hwdb_get_properties_list_entry ? R.udev_hwdb_get_properties_list_entry(hwdb, modalias, flags) : NULL;
    }
    if (!hwdb) {
        return NULL;
    }
    free_udev_list(hwdb->properties);
    hwdb->properties = list_copy_real(R.udev_hwdb_get_properties_list_entry(hwdb->real, modalias, flags));
    return hwdb->properties;
}

/* ---- queue ---- */

struct udev_queue *udev_queue_new(struct udev *udev) {
    if (is_raw(udev, FAKE_MAGIC_UDEV)) {
        return R.udev_queue_new ? R.udev_queue_new(udev) : NULL;
    }
    if (!udev) {
        return NULL;
    }
    struct udev_queue *q = (struct udev_queue *)calloc(1, sizeof(*q));
    if (!q) {
        return NULL;
    }
    q->magic = FAKE_MAGIC_QUEUE;
    q->udev_ctx = udev_ref(udev);
    q->n_ref = 1;
    if (udev->real) {
        q->real = R.udev_queue_new(udev->real);
    }
    return q;
}

struct udev_queue *udev_queue_ref(struct udev_queue *udev_queue) {
    if (is_raw(udev_queue, FAKE_MAGIC_QUEUE)) {
        return R.udev_queue_ref ? R.udev_queue_ref(udev_queue) : udev_queue;
    }
    if (udev_queue) {
        udev_queue->n_ref++;
    }
    return udev_queue;
}

struct udev_queue *udev_queue_unref(struct udev_queue *udev_queue) {
    if (is_raw(udev_queue, FAKE_MAGIC_QUEUE)) {
        return R.udev_queue_unref ? R.udev_queue_unref(udev_queue) : NULL;
    }
    if (!udev_queue) {
        return NULL;
    }
    if (--udev_queue->n_ref > 0) {
        return udev_queue;
    }
    if (udev_queue->real) {
        R.udev_queue_unref(udev_queue->real);
    }
    free_udev_list(udev_queue->queued);
    udev_unref(udev_queue->udev_ctx);
    free(udev_queue);
    return NULL;
}

struct udev *udev_queue_get_udev(struct udev_queue *udev_queue) {
    if (is_raw(udev_queue, FAKE_MAGIC_QUEUE)) {
        return R.udev_queue_get_udev ? R.udev_queue_get_udev(udev_queue) : NULL;
    }
    return udev_queue ? udev_queue->udev_ctx : NULL;
}

/* Forwards a queue query for a real pointer or a wrapper around one; the
 * value after the function is the answer of a queue with no real backing. */
#define QUEUE_PASSTHROUGH(q, fn, dflt, ...) \
    do { \
        if (is_raw((q), FAKE_MAGIC_QUEUE)) { \
            return R.fn ? R.fn((q), ##__VA_ARGS__) : (dflt); \
        } \
        if ((q) && (q)->real) { \
            return R.fn ? R.fn((q)->real, ##__VA_ARGS__) : (dflt); \
        } \
        return (dflt); \
    } while (0)

unsigned long long int udev_queue_get_kernel_seqnum(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_kernel_seqnum, 0);
}

unsigned long long int udev_queue_get_udev_seqnum(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_udev_seqnum, 0);
}

int udev_queue_get_udev_is_active(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_udev_is_active, 0);
}

int udev_queue_get_queue_is_empty(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_queue_is_empty, 1);
}

int udev_queue_get_seqnum_is_finished(struct udev_queue *udev_queue, unsigned long long int seqnum) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_seqnum_is_finished, 1, seqnum);
}

int udev_queue_get_seqnum_sequence_is_finished(struct udev_queue *udev_queue,
                                               unsigned long long int start, unsigned long long int end) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_seqnum_sequence_is_finished, 1, start, end);
}

int udev_queue_get_fd(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_get_fd, -1);
}

int udev_queue_flush(struct udev_queue *udev_queue) {
    QUEUE_PASSTHROUGH(udev_queue, udev_queue_flush, 0);
}

struct udev_list_entry *udev_queue_get_queued_list_entry(struct udev_queue *udev_queue) {
    if (is_raw(udev_queue, FAKE_MAGIC_QUEUE)) {
        return R.udev_queue_get_queued_list_entry ? R.udev_queue_get_queued_list_entry(udev_queue) : NULL;
    }
    if (!udev_queue) {
        return NULL;
    }
    free_udev_list(udev_queue->queued);
    udev_queue->queued = NULL;
    if (udev_queue->real && R.udev_queue_get_queued_list_entry) {
        udev_queue->queued = list_copy_real(R.udev_queue_get_queued_list_entry(udev_queue->real));
    }
    return udev_queue->queued;
}

/* ---- util ---- */

/* The real library's escaping where it is loaded; otherwise a verbatim copy,
 * NUL-terminated within len, returning the bytes copied. */
int udev_util_encode_string(const char *str, char *str_enc, size_t len) {
    if (R.udev_util_encode_string) {
        return R.udev_util_encode_string(str, str_enc, len);
    }
    if (!str || !str_enc || len == 0) {
        return 0;
    }
    size_t copy_len = strlen(str);
    if (copy_len >= len) {
        copy_len = len - 1;
    }
    memcpy(str_enc, str, copy_len);
    str_enc[copy_len] = '\0';
    return (int)copy_len;
}
