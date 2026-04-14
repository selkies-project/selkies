#include "libudev.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include <errno.h>
#include <sys/eventfd.h>
#include <fnmatch.h>       // For fnmatch if used (like for "js*")
#include <sys/types.h>     // For dev_t
#include <sys/sysmacros.h> // For major() and minor()
#include <unistd.h>        // For STDIN_FILENO
static bool g_fake_udev_log_enabled = false;
static bool g_fake_udev_logging_initialized = false;
#define FAKE_UDEV_LOG_DEBUG(fmt, ...) do { if (g_fake_udev_log_enabled) fprintf(stderr, "[fake_udev_dbg:%s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); } while (0)
#define FAKE_UDEV_LOG_INFO(fmt, ...)  do { if (g_fake_udev_log_enabled) fprintf(stderr, "[fake_udev_info:%s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); } while (0)
#define FAKE_UDEV_LOG_WARN(fmt, ...)  do { if (g_fake_udev_log_enabled) fprintf(stderr, "[fake_udev_warn:%s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); } while (0)
#define FAKE_UDEV_LOG_ERROR(fmt, ...) do { if (g_fake_udev_log_enabled) fprintf(stderr, "[fake_udev_err:%s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); } while (0)

// --- Virtual Device Definitions ---
#define NUM_VIRTUAL_GAMEPADS 4

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

    // JS Device
    char js_syspath[256];
    char js_devnode[64];
    char js_sysname[64];
    const char *js_subsystem;
    key_value_pair_t js_properties[4]; // DEVNAME, ID_INPUT_JOYSTICK, ID_INPUT, NULL

    // Event Device
    char event_syspath[256];
    char event_devnode[64];
    char event_sysname[64];
    const char *event_subsystem;
    key_value_pair_t event_properties[6]; // DEVNAME, ID_INPUT_EVENT_JOYSTICK, ID_INPUT_JOYSTICK, ID_INPUT_GAMEPAD, ID_INPUT, NULL


    // Input Parent Device
    char input_parent_syspath[256];
    char input_parent_sysname[64];
    const char *input_parent_subsystem;
    key_value_pair_t input_parent_sysattrs[12]; // vendor, product, version, name, phys, uniq, caps, etc. +NULL
    key_value_pair_t input_parent_properties[4]; // ID_INPUT, ID_INPUT_JOYSTICK, DEVPATH, NULL


    // USB Parent Device
    char usb_parent_syspath[256];
    char usb_parent_sysname[64];
    const char *usb_parent_subsystem;
    const char *usb_parent_devtype;
    key_value_pair_t usb_parent_sysattrs[7]; // idVendor, idProduct, manufacturer, product, bcdDevice, serial (+NULL)
} virtual_gamepad_definition_t;

virtual_gamepad_definition_t virtual_gamepads[NUM_VIRTUAL_GAMEPADS];
bool virtual_gamepads_initialized = false;

// Buffers for strings that need to live as long as the lib
// static char input_names[NUM_VIRTUAL_GAMEPADS][128]; // No longer used as name is static
static char input_phys[NUM_VIRTUAL_GAMEPADS][64];
static char input_uniq[NUM_VIRTUAL_GAMEPADS][64];
static char input_devpaths[NUM_VIRTUAL_GAMEPADS][256];
// static char usb_prod_names[NUM_VIRTUAL_GAMEPADS][128]; // No longer used as product is static
static char usb_serials[NUM_VIRTUAL_GAMEPADS][64];

static void fake_udev_logging_init_if_needed() {
    if (g_fake_udev_logging_initialized) {
        return;
    }
    if (getenv("JS_LOG") != NULL) {
        g_fake_udev_log_enabled = true;
    }
    g_fake_udev_logging_initialized = true;
}

void initialize_virtual_gamepads_data_if_needed() {
    FAKE_UDEV_LOG_DEBUG("Enter");
    if (virtual_gamepads_initialized) {
        FAKE_UDEV_LOG_DEBUG("Already initialized, returning.");
        return;
    }

    int event_dev_id_base = 1000;
    FAKE_UDEV_LOG_INFO("Initializing data for %d virtual gamepads. Event base ID: %d", NUM_VIRTUAL_GAMEPADS, event_dev_id_base);

    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        FAKE_UDEV_LOG_DEBUG("Initializing gamepad %d", i);
        virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        def->id = i;

        // --- Input Parent Device ---
        // This sysname is for the unique "physical" device part of the path.
        snprintf(def->input_parent_sysname, sizeof(def->input_parent_sysname), "selkies_pad%d", i);

        snprintf(def->input_parent_syspath, sizeof(def->input_parent_syspath),
                 "/sys/devices/virtual/%s/input/input%d", def->input_parent_sysname, i + 10);
        def->input_parent_subsystem = "input"; // The subsystem of this node is still "input"
        FAKE_UDEV_LOG_DEBUG("  Gamepad %d Input Parent: sysname='%s', syspath='%s', subsystem='%s'",
                           i, def->input_parent_sysname, def->input_parent_syspath, def->input_parent_subsystem);

        // Sysattrs for the input parent node (e.g., /sys/devices/virtual/selkies_pad0/input/input10)
        def->input_parent_sysattrs[0] = (key_value_pair_t){"id/vendor", "0x045e"};
        def->input_parent_sysattrs[1] = (key_value_pair_t){"id/product", "0x028e"};
        def->input_parent_sysattrs[2] = (key_value_pair_t){"id/version", "0x0114"};
        def->input_parent_sysattrs[3] = (key_value_pair_t){"name", "Microsoft X-Box 360 pad"}; // Name of the input event interface

        snprintf(input_phys[i], sizeof(input_phys[i]), "selkies/virtpad%d/input0", i); // Physical path
        def->input_parent_sysattrs[4] = (key_value_pair_t){"phys", input_phys[i]};
        snprintf(input_uniq[i], sizeof(input_uniq[i]), "SGVP%04d", i); // Unique ID
        def->input_parent_sysattrs[5] = (key_value_pair_t){"uniq", input_uniq[i]};
        def->input_parent_sysattrs[6] = (key_value_pair_t){"capabilities/ev", "1b"};
        def->input_parent_sysattrs[7] = (key_value_pair_t){"capabilities/key", "ffff000000000000 0 0 0 0 0 7fdb000000000000 0 0 0 0"};
        def->input_parent_sysattrs[8] = (key_value_pair_t){"capabilities/abs", "3003f"};
        def->input_parent_sysattrs[9] = (key_value_pair_t){"id/bustype", "0003"}; // BUS_USB
        def->input_parent_sysattrs[10] = (key_value_pair_t){"event_count", "123"}; // Dummy value
        def->input_parent_sysattrs[11] = (key_value_pair_t){NULL, NULL};

        // Properties for the input parent node
        def->input_parent_properties[0] = (key_value_pair_t){"ID_INPUT", "1"};
        def->input_parent_properties[1] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"}; // The input parent itself is a joystick source
        // DEVPATH is the syspath relative to /sys
        snprintf(input_devpaths[i], sizeof(input_devpaths[i]), "%s", def->input_parent_syspath + strlen("/sys"));
        def->input_parent_properties[2] = (key_value_pair_t){"DEVPATH", input_devpaths[i]};
        def->input_parent_properties[3] = (key_value_pair_t){NULL, NULL};
        FAKE_UDEV_LOG_DEBUG("  Gamepad %d Input Parent: DEVPATH='%s'", i, input_devpaths[i]);


        // --- JS Device ---
        // JS device node is a child of the input parent node.
        snprintf(def->js_sysname, sizeof(def->js_sysname), "js%d", i);
        snprintf(def->js_syspath, sizeof(def->js_syspath), "%s/%s", def->input_parent_syspath, def->js_sysname);
        snprintf(def->js_devnode, sizeof(def->js_devnode), "/dev/input/js%d", i);
        def->js_subsystem = "input"; // The js node itself is also in the "input" subsystem in terms of udev classification
        FAKE_UDEV_LOG_DEBUG("  Gamepad %d JS: sysname='%s', syspath='%s', devnode='%s', subsystem='%s'",
                           i, def->js_sysname, def->js_syspath, def->js_devnode, def->js_subsystem);
        def->js_properties[0] = (key_value_pair_t){"DEVNAME", def->js_devnode};
        def->js_properties[1] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"};
        def->js_properties[2] = (key_value_pair_t){"ID_INPUT", "1"};
        def->js_properties[3] = (key_value_pair_t){NULL, NULL};

        // --- Event Device ---
        // Event device node is also a child of the input parent node.
        snprintf(def->event_sysname, sizeof(def->event_sysname), "event%d", event_dev_id_base + i);
        snprintf(def->event_syspath, sizeof(def->event_syspath), "%s/%s", def->input_parent_syspath, def->event_sysname);
        snprintf(def->event_devnode, sizeof(def->event_devnode), "/dev/input/event%d", event_dev_id_base + i);
        def->event_subsystem = "input"; // The event node is also in the "input" subsystem
        FAKE_UDEV_LOG_DEBUG("  Gamepad %d Event: sysname='%s', syspath='%s', devnode='%s', subsystem='%s'",
                           i, def->event_sysname, def->event_syspath, def->event_devnode, def->event_subsystem);
        def->event_properties[0] = (key_value_pair_t){"DEVNAME", def->event_devnode};
        def->event_properties[1] = (key_value_pair_t){"ID_INPUT_EVENT_JOYSTICK", "1"};
        def->event_properties[2] = (key_value_pair_t){"ID_INPUT_JOYSTICK", "1"};
        def->event_properties[3] = (key_value_pair_t){"ID_INPUT_GAMEPAD", "1"};
        def->event_properties[4] = (key_value_pair_t){"ID_INPUT", "1"};
        def->event_properties[5] = (key_value_pair_t){NULL, NULL};

        // --- USB Parent Device ---
        snprintf(def->usb_parent_sysname, sizeof(def->usb_parent_sysname), "selkies_usb_ctrl%d_dev", i);
        // Path for the USB device itself (parent of the USB interface that leads to the input device)
        snprintf(def->usb_parent_syspath, sizeof(def->usb_parent_syspath), "/sys/devices/virtual/usb/%s", def->usb_parent_sysname);
        def->usb_parent_subsystem = "usb";
        def->usb_parent_devtype = "usb_device";
        FAKE_UDEV_LOG_DEBUG("  Gamepad %d USB Parent: sysname='%s', syspath='%s', subsystem='%s', devtype='%s'",
                           i, def->usb_parent_sysname, def->usb_parent_syspath, def->usb_parent_subsystem, def->usb_parent_devtype);
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
    FAKE_UDEV_LOG_INFO("Successfully initialized %d virtual gamepads. Event devices: /dev/input/event%d to /dev/input/event%d",
                  NUM_VIRTUAL_GAMEPADS, event_dev_id_base, event_dev_id_base + NUM_VIRTUAL_GAMEPADS - 1);
    FAKE_UDEV_LOG_DEBUG("Exit");
}

const virtual_gamepad_definition_t* find_virtual_def_by_syspath(const char *syspath, virtual_device_node_type_t *node_type_out) {
    FAKE_UDEV_LOG_DEBUG("Enter for syspath: %s", syspath ? syspath : "NULL");
    initialize_virtual_gamepads_data_if_needed();
    if (!syspath || !node_type_out) {
        FAKE_UDEV_LOG_WARN("Invalid arguments: syspath=%p, node_type_out=%p", (void*)syspath, (void*)node_type_out);
        if (node_type_out) *node_type_out = VIRTUAL_TYPE_NONE;
        return NULL;
    }
    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        FAKE_UDEV_LOG_DEBUG("  Checking def %d: js_syspath='%s', event_syspath='%s', input_parent_syspath='%s', usb_parent_syspath='%s'",
                           i, def->js_syspath, def->event_syspath, def->input_parent_syspath, def->usb_parent_syspath);
        if (strcmp(syspath, def->js_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_JS; FAKE_UDEV_LOG_DEBUG("  Found JS match for %s", syspath); return def; }
        if (strcmp(syspath, def->event_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_EVENT; FAKE_UDEV_LOG_DEBUG("  Found EVENT match for %s", syspath); return def; }
        if (strcmp(syspath, def->input_parent_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_INPUT_PARENT; FAKE_UDEV_LOG_DEBUG("  Found INPUT_PARENT match for %s", syspath); return def; }
        if (strcmp(syspath, def->usb_parent_syspath) == 0) { *node_type_out = VIRTUAL_TYPE_USB_PARENT; FAKE_UDEV_LOG_DEBUG("  Found USB_PARENT match for %s", syspath); return def; }
    }
    *node_type_out = VIRTUAL_TYPE_NONE;
    FAKE_UDEV_LOG_DEBUG("No match found for syspath: %s", syspath);
    return NULL;
}

struct udev {
    int n_ref;
};

struct udev_list_entry {
    struct udev_list_entry *next;
    char *name;
    char *value;
};

struct udev_device {
    struct udev *udev_ctx;
    int n_ref;
    const virtual_gamepad_definition_t *gamepad_def;
    virtual_device_node_type_t node_type;
    struct udev_list_entry *properties_cache;
    bool properties_cached;
};

struct udev_enumerate {
    struct udev *udev_ctx;
    int n_ref;
    struct udev_list_entry *current_scan_results;
    bool filter_subsystem_input;
    char filter_sysname_pattern[64];
    struct udev_list_entry *property_filters;
};

struct udev_monitor {
    struct udev *udev_ctx;
    int n_ref;
    char name[64];
    int fd;
};

struct udev *udev_new(void) {
    fake_udev_logging_init_if_needed();
    initialize_virtual_gamepads_data_if_needed();
    struct udev *udev = (struct udev *)calloc(1, sizeof(struct udev));
    if (!udev) {
        FAKE_UDEV_LOG_ERROR("calloc failed for udev context");
        return NULL;
    }
    udev->n_ref = 1;
    return udev;
}

struct udev *udev_ref(struct udev *udev) {
    FAKE_UDEV_LOG_DEBUG("Enter for udev_ctx %p", (void*)udev);
    if (!udev) {
        FAKE_UDEV_LOG_WARN("udev_ref called with NULL udev_ctx");
        return NULL;
    }
    udev->n_ref++;
    FAKE_UDEV_LOG_DEBUG("udev_ctx %p new ref_count %d", (void*)udev, udev->n_ref);
    return udev;
}

struct udev *udev_unref(struct udev *udev) {
    FAKE_UDEV_LOG_DEBUG("Enter for udev_ctx %p", (void*)udev);
    if (!udev) {
        FAKE_UDEV_LOG_WARN("udev_unref called with NULL udev_ctx");
        return NULL;
    }
    udev->n_ref--;
    FAKE_UDEV_LOG_DEBUG("udev_ctx %p new ref_count %d", (void*)udev, udev->n_ref);
    if (udev->n_ref <= 0) {
        FAKE_UDEV_LOG_INFO("Freeing udev context %p", (void*)udev);
        free(udev);
        return NULL;
    }
    return udev;
}

void free_udev_list(struct udev_list_entry *head) {
    FAKE_UDEV_LOG_DEBUG("Enter for list head %p", (void*)head);
    struct udev_list_entry *current = head;
    int count = 0;
    while (current) {
        struct udev_list_entry *next = current->next;
        FAKE_UDEV_LOG_DEBUG("  Freeing list entry %p (name: '%s', value: '%s')",
                           (void*)current, current->name ? current->name : "NULL", current->value ? current->value : "NULL");
        free(current->name);
        free(current->value);
        free(current);
        current = next;
        count++;
    }
    FAKE_UDEV_LOG_DEBUG("Freed %d list entries.", count);
}

struct udev_list_entry *udev_list_entry_get_next(struct udev_list_entry *list_entry) {
    FAKE_UDEV_LOG_DEBUG("Enter for list_entry %p", (void*)list_entry);
    if (!list_entry) {
        FAKE_UDEV_LOG_DEBUG("  list_entry is NULL, returning NULL");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Returning next entry %p", (void*)list_entry->next);
    return list_entry->next;
}

const char *udev_list_entry_get_name(struct udev_list_entry *list_entry) {
    FAKE_UDEV_LOG_DEBUG("Enter for list_entry %p", (void*)list_entry);
    if (!list_entry) {
        FAKE_UDEV_LOG_DEBUG("  list_entry is NULL, returning NULL");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Returning name '%s'", list_entry->name ? list_entry->name : "NULL");
    return list_entry->name;
}

const char *udev_list_entry_get_value(struct udev_list_entry *list_entry) {
    FAKE_UDEV_LOG_DEBUG("Enter for list_entry %p", (void*)list_entry);
    if (!list_entry) {
        FAKE_UDEV_LOG_DEBUG("  list_entry is NULL, returning NULL");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Returning value '%s'", list_entry->value ? list_entry->value : "NULL");
    return list_entry->value;
}

struct udev_device *udev_device_new_from_syspath(struct udev *udev, const char *syspath) {
    FAKE_UDEV_LOG_INFO("called for udev_ctx %p, syspath: %s", (void*)udev, syspath ? syspath : "NULL");
    if (!udev || !syspath) {
        FAKE_UDEV_LOG_WARN("Invalid arguments: udev=%p, syspath=%s", (void*)udev, syspath ? syspath : "NULL");
        return NULL;
    }

    virtual_device_node_type_t node_type;
    const virtual_gamepad_definition_t *def = find_virtual_def_by_syspath(syspath, &node_type);

    if (!def) {
        FAKE_UDEV_LOG_WARN("No virtual device definition found for syspath: %s", syspath);
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Found definition for gamepad ID %d, node_type %d", def->id, node_type);

    struct udev_device *dev = (struct udev_device *)calloc(1, sizeof(struct udev_device));
    if (!dev) {
        FAKE_UDEV_LOG_ERROR("calloc failed for udev_device");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Allocated udev_device %p", (void*)dev);

    dev->udev_ctx = udev_ref(udev);
    if (!dev->udev_ctx) {
        FAKE_UDEV_LOG_ERROR("udev_ref returned NULL for udev_device. This is unexpected.");
        free(dev);
        return NULL;
    }
    dev->n_ref = 1;
    dev->gamepad_def = def;
    dev->node_type = node_type;
    dev->properties_cache = NULL;
    dev->properties_cached = false;

    FAKE_UDEV_LOG_INFO("Created VIRTUAL device %p (ref %d) for syspath: %s, type: %d", (void*)dev, dev->n_ref, syspath, node_type);
    return dev;
}

struct udev_device *udev_device_new_from_devnum(struct udev *udev, char type, dev_t devnum) {
    FAKE_UDEV_LOG_INFO("STUB called for udev_ctx %p, type '%c', devnum %llu (major %u, minor %u)",
                  (void*)udev, type, (unsigned long long)devnum, (unsigned int)major(devnum), (unsigned int)minor(devnum));
    return NULL;
}

struct udev_device *udev_device_new_from_subsystem_sysname(struct udev *udev, const char *subsystem, const char *sysname) {
    FAKE_UDEV_LOG_INFO("called for udev_ctx %p, subsystem: %s, sysname: %s",
                  (void*)udev, subsystem ? subsystem : "NULL", sysname ? sysname : "NULL");

    if (!udev || !subsystem || !sysname) {
        FAKE_UDEV_LOG_WARN("Invalid arguments: udev=%p, subsystem=%s, sysname=%s",
                          (void*)udev, subsystem ? subsystem : "NULL", sysname ? sysname : "NULL");
        return NULL;
    }

    initialize_virtual_gamepads_data_if_needed();

    const virtual_gamepad_definition_t *found_def = NULL;
    virtual_device_node_type_t found_node_type = VIRTUAL_TYPE_NONE;

    for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
        const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
        FAKE_UDEV_LOG_DEBUG("  Checking def %d: js_subsys='%s' js_sysname='%s', ev_subsys='%s' ev_sysname='%s', etc.",
                           i, def->js_subsystem, def->js_sysname, def->event_subsystem, def->event_sysname);
        if (strcmp(subsystem, def->js_subsystem) == 0 && strcmp(sysname, def->js_sysname) == 0) {
            found_def = def; found_node_type = VIRTUAL_TYPE_JS; break;
        }
        if (strcmp(subsystem, def->event_subsystem) == 0 && strcmp(sysname, def->event_sysname) == 0) {
            found_def = def; found_node_type = VIRTUAL_TYPE_EVENT; break;
        }
        if (strcmp(subsystem, def->input_parent_subsystem) == 0 && strcmp(sysname, def->input_parent_sysname) == 0) {
            found_def = def; found_node_type = VIRTUAL_TYPE_INPUT_PARENT; break;
        }
        if (strcmp(subsystem, def->usb_parent_subsystem) == 0 && strcmp(sysname, def->usb_parent_sysname) == 0) {
            found_def = def; found_node_type = VIRTUAL_TYPE_USB_PARENT; break;
        }
    }

    if (!found_def) {
        FAKE_UDEV_LOG_WARN("No virtual device definition found for subsystem '%s', sysname '%s'", subsystem, sysname);
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Found definition for gamepad ID %d, node_type %d", found_def->id, found_node_type);


    struct udev_device *dev = (struct udev_device *)calloc(1, sizeof(struct udev_device));
    if (!dev) {
        FAKE_UDEV_LOG_ERROR("calloc failed for udev_device (from subsystem/sysname)");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Allocated udev_device %p", (void*)dev);

    dev->udev_ctx = udev_ref(udev);
    if (!dev->udev_ctx) {
        FAKE_UDEV_LOG_ERROR("udev_ref returned NULL for udev_device (from subsystem/sysname). Unexpected.");
        free(dev);
        return NULL;
    }
    dev->n_ref = 1;
    dev->gamepad_def = found_def;
    dev->node_type = found_node_type;
    dev->properties_cache = NULL;
    dev->properties_cached = false;

    FAKE_UDEV_LOG_INFO("Created VIRTUAL device %p (ref %d) for subsystem '%s', sysname '%s', type: %d (syspath: %s)",
                  (void*)dev, dev->n_ref, subsystem, sysname, found_node_type, udev_device_get_syspath(dev));
    return dev;
}


struct udev_device *udev_device_ref(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p", (void*)udev_device);
    if (!udev_device) {
        FAKE_UDEV_LOG_WARN("udev_device_ref called with NULL device");
        return NULL;
    }
    udev_device->n_ref++;
    FAKE_UDEV_LOG_DEBUG("device %p (%s) new ref_count %d",
                       (void*)udev_device, udev_device_get_syspath(udev_device) ? udev_device_get_syspath(udev_device) : "NO_SYSPATH", udev_device->n_ref);
    return udev_device;
}

struct udev_device *udev_device_unref(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p", (void*)udev_device);
    if (!udev_device) {
        FAKE_UDEV_LOG_WARN("udev_device_unref called with NULL device");
        return NULL;
    }
    udev_device->n_ref--;
    const char* syspath_for_log = udev_device_get_syspath(udev_device);
    FAKE_UDEV_LOG_DEBUG("device %p (%s) new ref_count %d",
                       (void*)udev_device, syspath_for_log ? syspath_for_log : "NO_SYSPATH", udev_device->n_ref);
    if (udev_device->n_ref <= 0) {
        FAKE_UDEV_LOG_INFO("Freeing device %p (%s)", (void*)udev_device, syspath_for_log ? syspath_for_log : "NO_SYSPATH_ON_FREE");
        udev_unref(udev_device->udev_ctx);
        if (udev_device->properties_cached) {
            FAKE_UDEV_LOG_DEBUG("  Freeing cached properties for device %p", (void*)udev_device);
            free_udev_list(udev_device->properties_cache);
        }
        free(udev_device);
        return NULL;
    }
    return udev_device;
}

const char *udev_device_get_syspath(struct udev_device *udev_device) {
    if (!udev_device || !udev_device->gamepad_def) {
        return NULL;
    }
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: return udev_device->gamepad_def->js_syspath;
        case VIRTUAL_TYPE_EVENT: return udev_device->gamepad_def->event_syspath;
        case VIRTUAL_TYPE_INPUT_PARENT: return udev_device->gamepad_def->input_parent_syspath;
        case VIRTUAL_TYPE_USB_PARENT: return udev_device->gamepad_def->usb_parent_syspath;
        default: return NULL;
    }
}

const char *udev_device_get_devnode(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Device or gamepad_def is NULL");
        return NULL;
    }
    const char *val = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: val = udev_device->gamepad_def->js_devnode; break;
        case VIRTUAL_TYPE_EVENT: val = udev_device->gamepad_def->event_devnode; break;
        default: FAKE_UDEV_LOG_DEBUG("  No devnode for type %d", udev_device->node_type); val = NULL; break;
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), devnode requested -> %s", (void*)udev_device, udev_device_get_syspath(udev_device), val ? val : "NULL");
    return val;
}

const char *udev_device_get_subsystem(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Device or gamepad_def is NULL");
        return NULL;
    }
    const char *val = NULL;
     switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: val = udev_device->gamepad_def->js_subsystem; break;
        case VIRTUAL_TYPE_EVENT: val = udev_device->gamepad_def->event_subsystem; break;
        case VIRTUAL_TYPE_INPUT_PARENT: val = udev_device->gamepad_def->input_parent_subsystem; break;
        case VIRTUAL_TYPE_USB_PARENT: val = udev_device->gamepad_def->usb_parent_subsystem; break;
        default: FAKE_UDEV_LOG_DEBUG("  No subsystem for type %d", udev_device->node_type); return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), subsystem requested -> %s", (void*)udev_device, udev_device_get_syspath(udev_device), val ? val : "NULL");
    return val;
}

const char *udev_device_get_sysname(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Device or gamepad_def is NULL");
        return NULL;
    }
    const char *val = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: val = udev_device->gamepad_def->js_sysname; break;
        case VIRTUAL_TYPE_EVENT: val = udev_device->gamepad_def->event_sysname; break;
        case VIRTUAL_TYPE_INPUT_PARENT: val = udev_device->gamepad_def->input_parent_sysname; break;
        case VIRTUAL_TYPE_USB_PARENT: val = udev_device->gamepad_def->usb_parent_sysname; break;
        default: FAKE_UDEV_LOG_DEBUG("  No sysname for type %d", udev_device->node_type); return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), sysname requested -> %s", (void*)udev_device, udev_device_get_syspath(udev_device), val ? val : "NULL");
    return val;
}

const char *udev_device_get_devtype(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Device or gamepad_def is NULL");
        return NULL;
    }
    const char *val = NULL;
    if (udev_device->node_type == VIRTUAL_TYPE_USB_PARENT) {
        val = udev_device->gamepad_def->usb_parent_devtype;
    } else {
        FAKE_UDEV_LOG_DEBUG("  No devtype for non-USB_PARENT type %d", udev_device->node_type);
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), devtype requested -> %s", (void*)udev_device, udev_device_get_syspath(udev_device), val ? val : "NULL");
    return val;
}

const char *udev_device_get_property_value(struct udev_device *udev_device, const char *key) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s), key '%s'", (void*)udev_device, udev_device_get_syspath(udev_device), key ? key : "NULL");
    if (!udev_device || !udev_device->gamepad_def || !key) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: device=%p, gamepad_def=%p, key=%s",
                          (void*)udev_device, (void*)(udev_device ? udev_device->gamepad_def : NULL), key ? key : "NULL");
        return NULL;
    }
    const key_value_pair_t *props_to_search = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: props_to_search = udev_device->gamepad_def->js_properties; break;
        case VIRTUAL_TYPE_EVENT: props_to_search = udev_device->gamepad_def->event_properties; break;
        case VIRTUAL_TYPE_INPUT_PARENT: props_to_search = udev_device->gamepad_def->input_parent_properties; break;
        default: FAKE_UDEV_LOG_DEBUG("  No properties defined for type %d", udev_device->node_type); break;
    }

    if (props_to_search) {
        for (int i = 0; props_to_search[i].name != NULL; ++i) {
            FAKE_UDEV_LOG_DEBUG("  Checking property [%d]: name='%s', value='%s'", i, props_to_search[i].name, props_to_search[i].value);
            if (strcmp(props_to_search[i].name, key) == 0) {
                FAKE_UDEV_LOG_DEBUG("  Device %p (%s), property '%s' -> FOUND '%s'",
                                   (void*)udev_device, udev_device_get_syspath(udev_device), key, props_to_search[i].value);
                return props_to_search[i].value;
            }
        }
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), property '%s' -> NOT FOUND", (void*)udev_device, udev_device_get_syspath(udev_device), key);
    return NULL;
}

const char *udev_device_get_sysattr_value(struct udev_device *udev_device, const char *sysattr) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s), sysattr '%s'", (void*)udev_device, udev_device_get_syspath(udev_device), sysattr ? sysattr : "NULL");
    if (!udev_device || !udev_device->gamepad_def || !sysattr) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: device=%p, gamepad_def=%p, sysattr=%s",
                          (void*)udev_device, (void*)(udev_device ? udev_device->gamepad_def : NULL), sysattr ? sysattr : "NULL");
        return NULL;
    }
    const key_value_pair_t *attrs_to_search = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_INPUT_PARENT: attrs_to_search = udev_device->gamepad_def->input_parent_sysattrs; break;
        case VIRTUAL_TYPE_USB_PARENT: attrs_to_search = udev_device->gamepad_def->usb_parent_sysattrs; break;
        default: FAKE_UDEV_LOG_DEBUG("  No sysattrs defined for type %d", udev_device->node_type); break;
    }

    if (attrs_to_search) {
        for (int i = 0; attrs_to_search[i].name != NULL; ++i) {
            FAKE_UDEV_LOG_DEBUG("  Checking sysattr [%d]: name='%s', value='%s'", i, attrs_to_search[i].name, attrs_to_search[i].value);
            if (strcmp(attrs_to_search[i].name, sysattr) == 0) {
                FAKE_UDEV_LOG_DEBUG("  Device %p (%s), sysattr '%s' -> FOUND '%s'",
                                   (void*)udev_device, udev_device_get_syspath(udev_device), sysattr, attrs_to_search[i].value);
                return attrs_to_search[i].value;
            }
        }
    }
    FAKE_UDEV_LOG_DEBUG("  Device %p (%s), sysattr '%s' -> NOT FOUND", (void*)udev_device, udev_device_get_syspath(udev_device), sysattr);
    return NULL;
}

struct udev_device *udev_device_get_parent_with_subsystem_devtype(
        struct udev_device *udev_device,
        const char *subsystem,
        const char *devtype) {
    FAKE_UDEV_LOG_INFO("called for child %p (%s), find parent with subsys '%s', devtype '%s'",
        (void*)udev_device, udev_device_get_syspath(udev_device), subsystem, devtype ? devtype : "(any)");
    if (!udev_device || !udev_device->gamepad_def || !subsystem) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: udev_device=%p, gamepad_def=%p, subsystem=%s",
                          (void*)udev_device, (void*)(udev_device ? udev_device->gamepad_def : NULL), subsystem ? subsystem : "NULL");
        return NULL;
    }

    const char *parent_syspath_str = NULL;
    virtual_device_node_type_t parent_expected_node_type = VIRTUAL_TYPE_NONE;

    if (udev_device->node_type == VIRTUAL_TYPE_JS || udev_device->node_type == VIRTUAL_TYPE_EVENT) {
        FAKE_UDEV_LOG_DEBUG("  Child is JS or EVENT type.");
        if (strcmp(subsystem, "input") == 0 && (devtype == NULL || devtype[0] == '\0') ) {
            parent_syspath_str = udev_device->gamepad_def->input_parent_syspath;
            parent_expected_node_type = VIRTUAL_TYPE_INPUT_PARENT;
            FAKE_UDEV_LOG_DEBUG("    Seeking 'input' parent: syspath='%s', expected_type=INPUT_PARENT", parent_syspath_str);
        } else {
            FAKE_UDEV_LOG_DEBUG("    Subsystem/devtype ('%s'/'%s') does not match criteria for input parent.", subsystem, devtype ? devtype : "(any)");
        }
    } else if (udev_device->node_type == VIRTUAL_TYPE_INPUT_PARENT) {
        FAKE_UDEV_LOG_DEBUG("  Child is INPUT_PARENT type.");
        if (strcmp(subsystem, "usb") == 0 && devtype && strcmp(devtype, "usb_device") == 0) {
            parent_syspath_str = udev_device->gamepad_def->usb_parent_syspath;
            parent_expected_node_type = VIRTUAL_TYPE_USB_PARENT;
            FAKE_UDEV_LOG_DEBUG("    Seeking 'usb/usb_device' parent: syspath='%s', expected_type=USB_PARENT", parent_syspath_str);
        } else {
            FAKE_UDEV_LOG_DEBUG("    Subsystem/devtype ('%s'/'%s') does not match criteria for usb parent.", subsystem, devtype ? devtype : "(any)");
        }
    } else {
        FAKE_UDEV_LOG_DEBUG("  Child type %d does not have a defined parent search logic here.", udev_device->node_type);
    }

    if (parent_syspath_str) {
        FAKE_UDEV_LOG_DEBUG("  Potential parent syspath for %s: %s (expected type %d)", udev_device_get_syspath(udev_device), parent_syspath_str, parent_expected_node_type);
        struct udev_device *parent_dev = udev_device_new_from_syspath(udev_device->udev_ctx, parent_syspath_str);
        if (parent_dev) {
            if (parent_dev->node_type == parent_expected_node_type) {
                 FAKE_UDEV_LOG_INFO("  MATCHED parent: %p (%s) for child %p (%s)",
                                    (void*)parent_dev, udev_device_get_syspath(parent_dev),
                                    (void*)udev_device, udev_device_get_syspath(udev_device));
                return parent_dev;
            } else {
                FAKE_UDEV_LOG_WARN("  Parent %p (%s) found but type mismatch (got %d, expected %d). Unreffing.",
                    (void*)parent_dev, udev_device_get_syspath(parent_dev), parent_dev->node_type, parent_expected_node_type);
                udev_device_unref(parent_dev);
            }
        } else {
            FAKE_UDEV_LOG_WARN("  udev_device_new_from_syspath failed for potential parent syspath %s", parent_syspath_str);
        }
    }
    FAKE_UDEV_LOG_INFO("  NO MATCH for parent of %s with specified criteria.", udev_device_get_syspath(udev_device));
    return NULL;
}

struct udev_list_entry *udev_device_get_properties_list_entry(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_INFO("called for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: device=%p, gamepad_def=%p",
                          (void*)udev_device, (void*)(udev_device ? udev_device->gamepad_def : NULL));
        return NULL;
    }

    if (udev_device->properties_cached) {
        FAKE_UDEV_LOG_DEBUG("  Returning cached properties list (head: %p) for %s", (void*)udev_device->properties_cache, udev_device_get_syspath(udev_device));
        return udev_device->properties_cache;
    }
    FAKE_UDEV_LOG_DEBUG("  Properties not cached for %s, building new list.", udev_device_get_syspath(udev_device));

    const key_value_pair_t *props_to_add = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS: props_to_add = udev_device->gamepad_def->js_properties; break;
        case VIRTUAL_TYPE_EVENT: props_to_add = udev_device->gamepad_def->event_properties; break;
        case VIRTUAL_TYPE_INPUT_PARENT: props_to_add = udev_device->gamepad_def->input_parent_properties; break;
        default:
            FAKE_UDEV_LOG_WARN("  No properties defined for device type %d (%s)", udev_device->node_type, udev_device_get_syspath(udev_device));
            return NULL;
    }

    struct udev_list_entry *head = NULL;
    struct udev_list_entry *tail = NULL;
    int count = 0;
    for (int i = 0; props_to_add && props_to_add[i].name != NULL; ++i) {
        FAKE_UDEV_LOG_DEBUG("  Processing property to add: name='%s', value='%s'", props_to_add[i].name, props_to_add[i].value);
        struct udev_list_entry *entry = (struct udev_list_entry *)calloc(1, sizeof(struct udev_list_entry));
        if (!entry) {
            FAKE_UDEV_LOG_ERROR("  calloc failed for property list entry");
            free_udev_list(head);
            return NULL;
        }
        entry->name = strdup(props_to_add[i].name);
        entry->value = strdup(props_to_add[i].value);
        if (!entry->name || !entry->value) {
            FAKE_UDEV_LOG_ERROR("  strdup failed for property name/value");
            free(entry->name);
            free(entry->value);
            free(entry);
            free_udev_list(head);
            return NULL;
        }
        if (!head) {
            head = entry;
        } else {
            tail->next = entry;
        }
        tail = entry;
        count++;
        FAKE_UDEV_LOG_DEBUG("    Added property to list for %s: %s = %s (entry %p)", udev_device_get_syspath(udev_device), entry->name, entry->value, (void*)entry);
    }
    udev_device->properties_cache = head;
    udev_device->properties_cached = true;
    FAKE_UDEV_LOG_INFO("  Finished building properties list for %s (head: %p, %d entries). Caching.",
                      udev_device_get_syspath(udev_device), (void*)head, count);
    return head;
}


struct udev *udev_device_get_udev(struct udev_device *udev_device) {
    FAKE_UDEV_LOG_DEBUG("Enter for device %p (%s)", (void*)udev_device, udev_device_get_syspath(udev_device));
    if (!udev_device) {
        FAKE_UDEV_LOG_WARN("  Device is NULL");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Returning udev_ctx %p", (void*)udev_device->udev_ctx);
    return udev_device->udev_ctx;
}

struct udev_enumerate *udev_enumerate_new(struct udev *udev) {
    FAKE_UDEV_LOG_INFO("called with udev_ctx %p", (void*)udev);
    if (!udev) {
        FAKE_UDEV_LOG_WARN("  udev_ctx is NULL");
        return NULL;
    }
    struct udev_enumerate *e = (struct udev_enumerate *)calloc(1, sizeof(struct udev_enumerate));
    if (!e) {
        FAKE_UDEV_LOG_ERROR("calloc failed for udev_enumerate");
        return NULL;
    }
    FAKE_UDEV_LOG_DEBUG("  Allocated udev_enumerate %p", (void*)e);
    e->udev_ctx = udev_ref(udev);
    if (!e->udev_ctx) {
        FAKE_UDEV_LOG_ERROR("udev_ref returned NULL for udev_enumerate. Unexpected.");
        free(e);
        return NULL;
    }
    e->n_ref = 1;
    e->filter_subsystem_input = false;
    e->filter_sysname_pattern[0] = '\0';
    e->current_scan_results = NULL;
    e->property_filters = NULL;
    FAKE_UDEV_LOG_INFO("Created enumerate %p (ref %d) for udev_ctx %p", (void*)e, e->n_ref, (void*)e->udev_ctx);
    return e;
}

struct udev_enumerate *udev_enumerate_ref(struct udev_enumerate *udev_enumerate) {
    FAKE_UDEV_LOG_DEBUG("Enter for enumerate %p", (void*)udev_enumerate);
    if (!udev_enumerate) {
        FAKE_UDEV_LOG_WARN("  udev_enumerate is NULL");
        return NULL;
    }
    udev_enumerate->n_ref++;
    FAKE_UDEV_LOG_DEBUG("enumerate %p new ref_count %d", (void*)udev_enumerate, udev_enumerate->n_ref);
    return udev_enumerate;
}

struct udev_enumerate *udev_enumerate_unref(struct udev_enumerate *udev_enumerate) {
    FAKE_UDEV_LOG_DEBUG("Enter for enumerate %p", (void*)udev_enumerate);
    if (!udev_enumerate) {
        FAKE_UDEV_LOG_WARN("  udev_enumerate is NULL");
        return NULL;
    }
    udev_enumerate->n_ref--;
    FAKE_UDEV_LOG_DEBUG("enumerate %p new ref_count %d", (void*)udev_enumerate, udev_enumerate->n_ref);
    if (udev_enumerate->n_ref <= 0) {
        FAKE_UDEV_LOG_INFO("Freeing enumerate object %p", (void*)udev_enumerate);
        udev_unref(udev_enumerate->udev_ctx);
        if (udev_enumerate->current_scan_results) {
            FAKE_UDEV_LOG_DEBUG("  Freeing scan results for enumerate %p", (void*)udev_enumerate);
            free_udev_list(udev_enumerate->current_scan_results);
        }
        if (udev_enumerate->property_filters) {
            FAKE_UDEV_LOG_DEBUG("  Freeing property filters for enumerate %p", (void*)udev_enumerate);
            free_udev_list(udev_enumerate->property_filters); // free_udev_list is suitable
        }
        free(udev_enumerate);
        return NULL;
    }
    return udev_enumerate;
}

int udev_enumerate_add_match_subsystem(struct udev_enumerate *udev_enumerate, const char *subsystem) {
    FAKE_UDEV_LOG_INFO("called for enumerate %p, subsystem: %s", (void*)udev_enumerate, subsystem ? subsystem : "NULL");
    if (!udev_enumerate || !subsystem) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: udev_enumerate=%p, subsystem=%s", (void*)udev_enumerate, subsystem ? subsystem : "NULL");
        return -EINVAL;
    }
    if (strcmp(subsystem, "input") == 0) {
        udev_enumerate->filter_subsystem_input = true;
        FAKE_UDEV_LOG_INFO("  Filter subsystem_input SET to true for enumerate %p", (void*)udev_enumerate);
    } else {
        FAKE_UDEV_LOG_WARN("  Subsystem '%s' is not 'input', filter_subsystem_input remains %d", subsystem, udev_enumerate->filter_subsystem_input);
    }
    return 0;
}

int udev_enumerate_add_match_sysname(struct udev_enumerate *udev_enumerate, const char *sysname) {
    FAKE_UDEV_LOG_INFO("called for enumerate %p, sysname: %s", (void*)udev_enumerate, sysname ? sysname : "NULL");
    if (!udev_enumerate || !sysname) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: udev_enumerate=%p, sysname=%s", (void*)udev_enumerate, sysname ? sysname : "NULL");
        return -EINVAL;
    }
    strncpy(udev_enumerate->filter_sysname_pattern, sysname, sizeof(udev_enumerate->filter_sysname_pattern) - 1);
    udev_enumerate->filter_sysname_pattern[sizeof(udev_enumerate->filter_sysname_pattern)-1] = '\0';
    FAKE_UDEV_LOG_INFO("  Filter sysname_pattern SET to '%s' for enumerate %p", udev_enumerate->filter_sysname_pattern, (void*)udev_enumerate);
    return 0;
}


int udev_enumerate_add_match_property(struct udev_enumerate *udev_enumerate, const char *property, const char *value) {
    FAKE_UDEV_LOG_INFO("called for enumerate %p, property: '%s', value: '%s'",
                  (void*)udev_enumerate, property ? property : "NULL", value ? value : "NULL");

    if (!udev_enumerate) {
        FAKE_UDEV_LOG_WARN("  Invalid argument: udev_enumerate is NULL.");
        return -EINVAL;
    }

    if (!property) {
        // The real libudev-enumerate.c returns 0 if property is NULL.
        FAKE_UDEV_LOG_WARN("  Property parameter is NULL. Doing nothing, returning 0.");
        return 0;
    }

    struct udev_list_entry *new_filter = (struct udev_list_entry *)calloc(1, sizeof(struct udev_list_entry));
    if (!new_filter) {
        FAKE_UDEV_LOG_ERROR("  calloc failed for property filter entry");
        return -ENOMEM;
    }
    new_filter->name = strdup(property);
    if (value) { // Value can be NULL, which might mean "property exists"
        new_filter->value = strdup(value);
    } else {
        new_filter->value = NULL; // Explicitly NULL if value arg is NULL
    }

    if (!new_filter->name || (value && !new_filter->value)) {
        FAKE_UDEV_LOG_ERROR("  strdup failed for property filter name/value");
        free(new_filter->name); // handles if name was strdup'd but value failed
        free(new_filter->value);
        free(new_filter);
        return -ENOMEM;
    }

    // Prepend to the list of filters
    new_filter->next = udev_enumerate->property_filters;
    udev_enumerate->property_filters = new_filter;

    FAKE_UDEV_LOG_INFO("  Filter by property '%s'='%s' ADDED to enumerate %p.",
                      property, value ? value : "(exists check)", (void*)udev_enumerate);

    // Any existing scan results are now potentially stale.
    // udev_enumerate_scan_devices already frees and rebuilds, so this is implicitly handled.
    if (udev_enumerate->current_scan_results) {
        FAKE_UDEV_LOG_DEBUG("  A property match filter was added. Any previous scan results in %p are now considered stale.", (void*)udev_enumerate);
    }
    return 0; // Success
}

int udev_enumerate_add_match_sysattr(struct udev_enumerate *udev_enumerate, const char *sysattr, const char *value) {
    if (!udev_enumerate) {
        return -EINVAL; // Standard error for invalid argument
    }
    // No-op, always succeed for now.
    return 0;
}

int udev_enumerate_add_nomatch_sysattr(struct udev_enumerate *udev_enumerate, const char *sysattr, const char *value) {
    if (!udev_enumerate) {
        return -EINVAL;
    }
    // No-op, always succeed for now.
    return 0;
}

int udev_enumerate_add_match_tag(struct udev_enumerate *udev_enumerate, const char *tag) {
    if (!udev_enumerate) {
        return -EINVAL;
    }
    // No-op, always succeed for now.
    return 0;
}

int udev_enumerate_add_match_parent(struct udev_enumerate *udev_enumerate, struct udev_device *parent) {
    if (!udev_enumerate) return -EINVAL;
    return 0; // No-op
}
int udev_enumerate_add_match_is_initialized(struct udev_enumerate *udev_enumerate) {
    if (!udev_enumerate) return -EINVAL;
    return 0; // No-op
}
int udev_enumerate_add_match_sysnum(struct udev_enumerate *udev_enumerate, const char *sysnum) {
   if (!udev_enumerate) return -EINVAL;
   return 0; // No-op
}
int udev_enumerate_add_match_devicenode(struct udev_enumerate *udev_enumerate, const char *devnode) {
   if (!udev_enumerate) return -EINVAL;
   return 0; // No-op
}
int udev_enumerate_add_syspath(struct udev_enumerate *udev_enumerate, const char *syspath) {
   if (!udev_enumerate) return -EINVAL;
   return 0; // No-op
}
int udev_enumerate_scan_children(struct udev_enumerate *udev_enumerate, struct udev_device *parent) {
   if (!udev_enumerate || !parent) return -EINVAL;
   // For scanning children, we would typically not find any for our virtual devices.
   // Clear any existing scan results.
   if (udev_enumerate->current_scan_results) {
       free_udev_list(udev_enumerate->current_scan_results);
       udev_enumerate->current_scan_results = NULL;
   }
   return 0;
}

// C-compatible helper function for adding to scan results
static void add_syspath_to_results_list(
    struct udev_list_entry **head_ptr,
    struct udev_list_entry **tail_ptr,
    int *count_ptr,
    const char* syspath_to_add,
    const char* device_type_log_str,
    int def_id_for_log) {

    if (!syspath_to_add || syspath_to_add[0] == '\0') return;

    FAKE_UDEV_LOG_DEBUG("    Adding %s device %s to results for def %d", device_type_log_str, syspath_to_add, def_id_for_log);
    struct udev_list_entry *entry = (struct udev_list_entry *)calloc(1, sizeof(struct udev_list_entry));
    if (!entry) {
        FAKE_UDEV_LOG_ERROR("    calloc failed for list entry for %s", syspath_to_add);
        // Note: Caller might need to free partially built list if this is critical.
        return;
    }
    entry->name = strdup(syspath_to_add);
    if (!entry->name) {
        FAKE_UDEV_LOG_ERROR("    strdup failed for list entry name %s", syspath_to_add);
        free(entry);
        return;
    }
    entry->value = NULL;
    entry->next = NULL;

    if (!*head_ptr) { // If list is empty
        *head_ptr = entry;
    } else { // Append to existing list
        (*tail_ptr)->next = entry;
    }
    *tail_ptr = entry; // Update tail to the new entry
    (*count_ptr)++;
}


static bool device_matches_all_property_filters(const virtual_gamepad_definition_t *def,
                                                virtual_device_node_type_t node_type,
                                                struct udev_list_entry *filters) {
    if (!filters) { // No filters means the device always matches this criteria
        return true;
    }

    const key_value_pair_t *device_properties = NULL;
    const char* node_type_str = "UNKNOWN";
    switch (node_type) {
        case VIRTUAL_TYPE_JS: device_properties = def->js_properties; node_type_str = "JS"; break;
        case VIRTUAL_TYPE_EVENT: device_properties = def->event_properties; node_type_str = "EVENT"; break;
        case VIRTUAL_TYPE_INPUT_PARENT: device_properties = def->input_parent_properties; node_type_str = "INPUT_PARENT"; break;
        default:
            FAKE_UDEV_LOG_DEBUG("    Device node type %d has no properties defined for filtering.", node_type);
            return false; // Or true, depending on how you want to treat types without properties
    }

    if (!device_properties) { // Should not happen if cases above are comprehensive for prop-having types
        FAKE_UDEV_LOG_DEBUG("    Device (type %s, def %d) has no properties array.", node_type_str, def->id);
        return false;
    }

    for (struct udev_list_entry *filter = filters; filter != NULL; filter = filter->next) {
        bool current_filter_matched = false;
        for (int i = 0; device_properties[i].name != NULL; ++i) {
            if (strcmp(device_properties[i].name, filter->name) == 0) {
                // Property name matches. Now check value.
                // If filter->value is NULL, it means "property exists, value doesn't matter".
                // If filter->value is not NULL, property value must also match.
                if (filter->value == NULL || (device_properties[i].value && strcmp(device_properties[i].value, filter->value) == 0)) {
                    current_filter_matched = true;
                    break; // Found match for this filter, move to next device property
                }
            }
        }
        if (!current_filter_matched) {
            FAKE_UDEV_LOG_DEBUG("    Device (type %s, def %d, syspath %s) FAILED to match filter: %s=%s",
                               node_type_str, def->id,
                               (node_type == VIRTUAL_TYPE_JS) ? def->js_syspath :
                               (node_type == VIRTUAL_TYPE_EVENT) ? def->event_syspath : def->input_parent_syspath,
                               filter->name, filter->value ? filter->value : "(exists)");
            return false; // This specific filter was not matched by any device property.
        }
        FAKE_UDEV_LOG_DEBUG("    Device (type %s, def %d) matched filter: %s=%s", node_type_str, def->id, filter->name, filter->value ? filter->value : "(exists)");
    }
    return true; // All filters were matched
}


int udev_enumerate_scan_devices(struct udev_enumerate *udev_enumerate) {
    FAKE_UDEV_LOG_INFO("called for enumerate %p (filters: subsystem_input=%d, sysname_pattern='%s')",
                  (void*)udev_enumerate, udev_enumerate->filter_subsystem_input, udev_enumerate->filter_sysname_pattern);
    if (!udev_enumerate) {
        FAKE_UDEV_LOG_WARN("  udev_enumerate is NULL");
        return -EINVAL;
    }

    if (udev_enumerate->current_scan_results) {
        FAKE_UDEV_LOG_DEBUG("  Freeing previous scan results for enumerate %p", (void*)udev_enumerate);
        free_udev_list(udev_enumerate->current_scan_results);
        udev_enumerate->current_scan_results = NULL;
    }

    struct udev_list_entry *head = NULL;
    struct udev_list_entry *tail = NULL;
    int count = 0;

    // We only proceed if subsystem_input is true OR if there are property filters.
    // The original libudev might behave differently if no subsystem filter is set but property filters are.

    if (udev_enumerate->filter_subsystem_input) { // Primary condition for scanning input devices
        FAKE_UDEV_LOG_DEBUG("  filter_subsystem_input is true, proceeding with scan.");
        initialize_virtual_gamepads_data_if_needed();

        for (int i = 0; i < NUM_VIRTUAL_GAMEPADS; ++i) {
            const virtual_gamepad_definition_t *def = &virtual_gamepads[i];
            FAKE_UDEV_LOG_DEBUG("  Scanning gamepad def %d (js: '%s', event: '%s', input_parent: '%s')",
                               i, def->js_sysname, def->event_sysname, def->input_parent_sysname);

            bool is_generic_sysname_scan = (udev_enumerate->filter_sysname_pattern[0] == '\0');

            // Check JS device
            if (is_generic_sysname_scan || fnmatch(udev_enumerate->filter_sysname_pattern, def->js_sysname, 0) == 0) {
                if (device_matches_all_property_filters(def, VIRTUAL_TYPE_JS, udev_enumerate->property_filters)) {
                    add_syspath_to_results_list(&head, &tail, &count, def->js_syspath, "JS", i);
                } else {
                    FAKE_UDEV_LOG_DEBUG("    JS device %s for def %d excluded by property filter(s).", def->js_syspath, i);
                }
            }

            // Check EVENT device
            if (is_generic_sysname_scan || fnmatch(udev_enumerate->filter_sysname_pattern, def->event_sysname, 0) == 0) {
                if (device_matches_all_property_filters(def, VIRTUAL_TYPE_EVENT, udev_enumerate->property_filters)) {
                    add_syspath_to_results_list(&head, &tail, &count, def->event_syspath, "EVENT", i);
                } else {
                    FAKE_UDEV_LOG_DEBUG("    EVENT device %s for def %d excluded by property filter(s).", def->event_syspath, i);
                }
            }

            // Check INPUT_PARENT device (only if pattern specifically matches it, not for generic scan)
            // And if it matches property filters (though joystick properties are usually not on the input parent directly)
            if (!is_generic_sysname_scan && fnmatch(udev_enumerate->filter_sysname_pattern, def->input_parent_sysname, 0) == 0) {
                 if (device_matches_all_property_filters(def, VIRTUAL_TYPE_INPUT_PARENT, udev_enumerate->property_filters)) {
                    add_syspath_to_results_list(&head, &tail, &count, def->input_parent_syspath, "INPUT_PARENT (by pattern)", i);
                 } else {
                    FAKE_UDEV_LOG_DEBUG("    INPUT_PARENT device %s for def %d excluded by property filter(s).", def->input_parent_syspath, i);
                 }
            }
        }
    } else if (udev_enumerate->property_filters) {
         // If subsystem is NOT "input", but there ARE property filters, we might still need to scan.
         FAKE_UDEV_LOG_DEBUG("  filter_subsystem_input is false, but property filters exist. This scenario is not fully implemented for non-input subsystems.");
    }
    else {
        FAKE_UDEV_LOG_DEBUG("  filter_subsystem_input is false and no property filters, not scanning for input devices.");
    }

    udev_enumerate->current_scan_results = head;
    FAKE_UDEV_LOG_INFO("Scan complete. Found %d matching devices for enumerate %p. List head: %p", count, (void*)udev_enumerate, (void*)head);
    return 0;
}

struct udev_list_entry *udev_enumerate_get_list_entry(struct udev_enumerate *udev_enumerate) {
    if (!udev_enumerate) {
        FAKE_UDEV_LOG_WARN("  udev_enumerate is NULL");
        return NULL;
    }
    return udev_enumerate->current_scan_results;
}

struct udev_monitor *udev_monitor_new_from_netlink(struct udev *udev, const char *name) {
    if (!udev) {
        return NULL;
    }
    struct udev_monitor *mon = (struct udev_monitor *)calloc(1, sizeof(struct udev_monitor));
    if (!mon) {
        return NULL;
    }
    mon->udev_ctx = udev_ref(udev);
    if (!mon->udev_ctx) {
        free(mon);
        return NULL;
    }
    mon->n_ref = 1;
    mon->fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    if (name) {
        strncpy(mon->name, name, sizeof(mon->name) - 1);
        mon->name[sizeof(mon->name) - 1] = '\0';
    } else {
        strncpy(mon->name, "(unnamed_monitor)", sizeof(mon->name) -1);
        mon->name[sizeof(mon->name)-1] = '\0';
    }
    return mon;
}

struct udev_monitor *udev_monitor_ref(struct udev_monitor *udev_monitor) {
    if (!udev_monitor) {
        return NULL;
    }
    udev_monitor->n_ref++;
    return udev_monitor;
}

struct udev_monitor *udev_monitor_unref(struct udev_monitor *udev_monitor) {
    FAKE_UDEV_LOG_DEBUG("Enter for monitor %p", (void*)udev_monitor);
    if (!udev_monitor) return NULL;
    udev_monitor->n_ref--;
    if (udev_monitor->n_ref <= 0) {
        udev_unref(udev_monitor->udev_ctx);
        if (udev_monitor->fd >= 0) {
            close(udev_monitor->fd);
        }
        free(udev_monitor);
        return NULL;
    }
    return udev_monitor;
}

int udev_monitor_enable_receiving(struct udev_monitor *udev_monitor) {
    if (!udev_monitor) return -EINVAL;
    return 0;
}

int udev_monitor_get_fd(struct udev_monitor *udev_monitor) {
    if (!udev_monitor) return -1;
    return udev_monitor->fd;
}

struct udev_device *udev_monitor_receive_device(struct udev_monitor *udev_monitor) {
    if (!udev_monitor) return NULL;
    return NULL;
}

int udev_monitor_filter_add_match_subsystem_devtype(
        struct udev_monitor *udev_monitor,
        const char *subsystem,
        const char *devtype) {
    if (!udev_monitor) return -EINVAL;
    return 0;
}

// --- HWDB Stubs ---
struct udev_hwdb *udev_hwdb_new(struct udev *udev) {
    FAKE_UDEV_LOG_INFO("STUB: udev_hwdb_new called for udev_ctx %p, returning NULL", (void*)udev);
    return NULL;
}
struct udev_hwdb *udev_hwdb_ref(struct udev_hwdb *udev_hwdb) {
    FAKE_UDEV_LOG_INFO("STUB: udev_hwdb_ref called for hwdb %p, returning input", (void*)udev_hwdb);
    return udev_hwdb;
}
struct udev_hwdb *udev_hwdb_unref(struct udev_hwdb *udev_hwdb) {
    FAKE_UDEV_LOG_INFO("STUB: udev_hwdb_unref called for hwdb %p, returning NULL", (void*)udev_hwdb);
    return NULL;
}
struct udev_list_entry *udev_hwdb_get_properties_list_entry(
        struct udev_hwdb *hwdb, const char *modalias, unsigned int flags) {
    FAKE_UDEV_LOG_INFO("STUB: udev_hwdb_get_properties_list_entry called for hwdb %p, modalias: %s, flags: %u. Returning NULL",
                  (void*)hwdb, modalias ? modalias : "NULL", flags);
    return NULL;
}

// --- Other udev_device Stubs/Placeholders ---
const char *udev_device_get_action(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("called for device %p (%s), returning 'add'", (void*)udev_device, syspath);
    return "add";
}

const char *udev_device_get_devpath(struct udev_device *udev_device) {
    const char *syspath = udev_device_get_syspath(udev_device);
    FAKE_UDEV_LOG_INFO("called for device %p (%s)", (void*)udev_device, syspath ? syspath : "NULL_DEVICE");
    if (syspath && strncmp(syspath, "/sys", 4) == 0) {
        FAKE_UDEV_LOG_DEBUG("  Returning syspath + 4: '%s'", syspath + 4);
        return syspath + 4;
    }
    FAKE_UDEV_LOG_DEBUG("  Returning original syspath (or NULL if syspath was NULL): '%s'", syspath ? syspath : "NULL");
    return syspath;
}

dev_t udev_device_get_devnum(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB called for device %p (%s), returning 0 (no devnum for virtual devices)", (void*)udev_device, syspath);
    return 0;
}

int udev_device_get_is_initialized(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB called for device %p (%s), returning 1 (always initialized for fake)", (void*)udev_device, syspath);
    return 1;
}

struct udev_device *udev_device_get_parent(struct udev_device *udev_device) {
    const char* child_syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("called for device %p (%s) (generic parent request)", (void*)udev_device, child_syspath);
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Invalid arguments: udev_device=%p or gamepad_def is NULL", (void*)udev_device);
        return NULL;
    }

    const char *parent_syspath_str = NULL;
    virtual_device_node_type_t parent_expected_node_type = VIRTUAL_TYPE_NONE;

    if (udev_device->node_type == VIRTUAL_TYPE_JS || udev_device->node_type == VIRTUAL_TYPE_EVENT) {
        parent_syspath_str = udev_device->gamepad_def->input_parent_syspath;
        parent_expected_node_type = VIRTUAL_TYPE_INPUT_PARENT;
        FAKE_UDEV_LOG_DEBUG("  Child is JS/EVENT, generic parent is INPUT_PARENT: %s", parent_syspath_str);
    } else if (udev_device->node_type == VIRTUAL_TYPE_INPUT_PARENT) {
        parent_syspath_str = udev_device->gamepad_def->usb_parent_syspath;
        parent_expected_node_type = VIRTUAL_TYPE_USB_PARENT;
        FAKE_UDEV_LOG_DEBUG("  Child is INPUT_PARENT, generic parent is USB_PARENT: %s", parent_syspath_str);
    } else {
        FAKE_UDEV_LOG_DEBUG("  Child type %d has no generic parent defined here.", udev_device->node_type);
    }

    if (parent_syspath_str) {
        FAKE_UDEV_LOG_DEBUG("  Generic parent attempt: child %s -> potential parent syspath %s (expected type %d)",
            child_syspath, parent_syspath_str, parent_expected_node_type);
        struct udev_device* parent_dev = udev_device_new_from_syspath(udev_device->udev_ctx, parent_syspath_str);
        if (parent_dev) {
            if (parent_dev->node_type == parent_expected_node_type) {
                FAKE_UDEV_LOG_INFO("  Generic parent found and type matches: %p (%s) for child %p (%s)",
                                   (void*)parent_dev, udev_device_get_syspath(parent_dev),
                                   (void*)udev_device, child_syspath);
                return parent_dev;
            } else {
                FAKE_UDEV_LOG_WARN("  Generic parent %p (%s) found but type mismatch (got %d, expected %d). Unreffing.",
                    (void*)parent_dev, udev_device_get_syspath(parent_dev), parent_dev->node_type, parent_expected_node_type);
                udev_device_unref(parent_dev);
            }
        } else {
             FAKE_UDEV_LOG_WARN("  udev_device_new_from_syspath failed for generic parent syspath %s", parent_syspath_str);
        }
    }
    FAKE_UDEV_LOG_INFO("  No generic parent defined or found for %s", child_syspath);
    return NULL;
}

struct udev_list_entry *udev_device_get_devlinks_list_entry(struct udev_device *udev_device) {
    const char* device_syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("called for device %p (%s)", (void*)udev_device, device_syspath);
    if (!udev_device || !udev_device->gamepad_def) {
        FAKE_UDEV_LOG_WARN("  Invalid device or gamepad_def for %s", device_syspath);
        return NULL;
    }

    const char* devnode_str = NULL;
    switch (udev_device->node_type) {
        case VIRTUAL_TYPE_JS:
            devnode_str = udev_device->gamepad_def->js_devnode;
            FAKE_UDEV_LOG_DEBUG("  Devlink for JS device: %s", devnode_str);
            break;
        case VIRTUAL_TYPE_EVENT:
            devnode_str = udev_device->gamepad_def->event_devnode;
            FAKE_UDEV_LOG_DEBUG("  Devlink for EVENT device: %s", devnode_str);
            break;
        default:
            FAKE_UDEV_LOG_WARN("  No devlinks defined for device type %d (%s)", udev_device->node_type, device_syspath);
            return NULL;
    }

    if (!devnode_str) {
        FAKE_UDEV_LOG_ERROR("  Devnode string is NULL for %s, cannot create devlink entry. This is unexpected.", device_syspath);
        return NULL;
    }

    struct udev_list_entry *entry = (struct udev_list_entry *)calloc(1, sizeof(struct udev_list_entry));
    if (!entry) {
        FAKE_UDEV_LOG_ERROR("  calloc failed for devlink entry for %s", device_syspath);
        return NULL;
    }
    entry->name = strdup(devnode_str);
    entry->value = NULL;

    if (!entry->name) {
        FAKE_UDEV_LOG_ERROR("  strdup failed for devlink name for %s", device_syspath);
        free(entry);
        return NULL;
    }
    entry->next = NULL;

    FAKE_UDEV_LOG_INFO("  Added devlink for %s: %s (entry %p)", device_syspath, entry->name, (void*)entry);
    return entry;
}

struct udev_list_entry *udev_device_get_sysattr_list_entry(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB called for device %p (%s), returning NULL", (void*)udev_device, syspath);
    return NULL;
}

struct udev_list_entry *udev_device_get_tags_list_entry(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB called for device %p (%s), returning NULL", (void*)udev_device, syspath);
    return NULL;
}

// --- udev context ---
void udev_set_log_fn(struct udev *udev,
                            void (*log_fn)(struct udev *udev,
                                           int priority, const char *file, int line, const char *fn,
                                           const char *format, va_list args)) {
    (void)udev; (void)log_fn; // Suppress unused parameter warnings
    FAKE_UDEV_LOG_INFO("STUB: udev_set_log_fn called.");
    // No-op
}

int udev_get_log_priority(struct udev *udev) {
    (void)udev;
    FAKE_UDEV_LOG_INFO("STUB: udev_get_log_priority called, returning 0.");
    return 0;
}

void udev_set_log_priority(struct udev *udev, int priority) {
    (void)udev; (void)priority;
    FAKE_UDEV_LOG_INFO("STUB: udev_set_log_priority called with priority %d.", priority);
    // No-op
}

void *udev_get_userdata(struct udev *udev) {
    (void)udev;
    FAKE_UDEV_LOG_INFO("STUB: udev_get_userdata called, returning NULL.");
    return NULL;
}

void udev_set_userdata(struct udev *udev, void *userdata) {
    (void)udev; (void)userdata;
    FAKE_UDEV_LOG_INFO("STUB: udev_set_userdata called.");
    // No-op
}

// --- udev_list_entry ---
struct udev_list_entry *udev_list_entry_get_by_name(struct udev_list_entry *list_entry, const char *name) {
    (void)list_entry; (void)name;
    FAKE_UDEV_LOG_INFO("STUB: udev_list_entry_get_by_name called for name '%s', returning NULL.", name ? name : "NULL");
    // A real implementation would iterate through the list.
    // For a simple stub, just return NULL.
    struct udev_list_entry *current = list_entry;
    while (current) {
        if (current->name && name && strcmp(current->name, name) == 0) {
            FAKE_UDEV_LOG_DEBUG("  Found match for '%s'", name);
            return current;
        }
        current = current->next;
    }
    FAKE_UDEV_LOG_DEBUG("  No match found for '%s'", name ? name : "NULL");
    return NULL;
}

// --- udev_device ---
struct udev_device *udev_device_new_from_device_id(struct udev *udev, const char *id) {
    (void)udev; (void)id;
    FAKE_UDEV_LOG_INFO("STUB: udev_device_new_from_device_id called for id '%s', returning NULL.", id ? id : "NULL");
    return NULL;
}

struct udev_device *udev_device_new_from_environment(struct udev *udev) {
    (void)udev;
    FAKE_UDEV_LOG_INFO("STUB: udev_device_new_from_environment called, returning NULL.");
    return NULL;
}

const char *udev_device_get_sysnum(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_get_sysnum called for device %p (%s), returning NULL.", (void*)udev_device, syspath);
    return NULL;
}

struct udev_list_entry *udev_device_get_current_tags_list_entry(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_get_current_tags_list_entry called for device %p (%s), returning NULL.", (void*)udev_device, syspath);
    return NULL;
}

const char *udev_device_get_driver(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_get_driver called for device %p (%s), returning NULL.", (void*)udev_device, syspath);
    return NULL;
}

unsigned long long int udev_device_get_seqnum(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_get_seqnum called for device %p (%s), returning 0.", (void*)udev_device, syspath);
    return 0;
}

unsigned long long int udev_device_get_usec_since_initialized(struct udev_device *udev_device) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_get_usec_since_initialized called for device %p (%s), returning 0.", (void*)udev_device, syspath);
    return 0;
}

int udev_device_set_sysattr_value(struct udev_device *udev_device, const char *sysattr, const char *value) {
    const char* dev_syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_set_sysattr_value called for device %p (%s), sysattr '%s', value '%s'. Returning 0 (success).",
                  (void*)udev_device, dev_syspath, sysattr ? sysattr : "NULL", value ? value : "NULL");
    return 0; // Indicate success, though it's a no-op
}

int udev_device_has_tag(struct udev_device *udev_device, const char *tag) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_has_tag called for device %p (%s), tag '%s'. Returning 0 (false).",
                  (void*)udev_device, syspath, tag ? tag : "NULL");
    return 0;
}

int udev_device_has_current_tag(struct udev_device *udev_device, const char *tag) {
    const char* syspath = udev_device ? udev_device_get_syspath(udev_device) : "NULL_DEVICE";
    FAKE_UDEV_LOG_INFO("STUB: udev_device_has_current_tag called for device %p (%s), tag '%s'. Returning 0 (false).",
                  (void*)udev_device, syspath, tag ? tag : "NULL");
    return 0;
}

// --- udev_monitor ---
struct udev *udev_monitor_get_udev(struct udev_monitor *udev_monitor) {
    FAKE_UDEV_LOG_INFO("STUB: udev_monitor_get_udev called for monitor %p.", (void*)udev_monitor);
    if (!udev_monitor) return NULL;
    return udev_monitor->udev_ctx; // Assuming udev_monitor struct has udev_ctx
}

int udev_monitor_set_receive_buffer_size(struct udev_monitor *udev_monitor, int size) {
    (void)udev_monitor; (void)size;
    FAKE_UDEV_LOG_INFO("STUB: udev_monitor_set_receive_buffer_size called for monitor %p, size %d. Returning 0.", (void*)udev_monitor, size);
    return 0;
}

int udev_monitor_filter_add_match_tag(struct udev_monitor *udev_monitor, const char *tag) {
    (void)udev_monitor; (void)tag;
    FAKE_UDEV_LOG_INFO("STUB: udev_monitor_filter_add_match_tag called for monitor %p, tag '%s'. Returning 0.",
                  (void*)udev_monitor, tag ? tag : "NULL");
    return 0;
}

int udev_monitor_filter_update(struct udev_monitor *udev_monitor) {
    (void)udev_monitor;
    FAKE_UDEV_LOG_INFO("STUB: udev_monitor_filter_update called for monitor %p. Returning 0.", (void*)udev_monitor);
    return 0;
}

int udev_monitor_filter_remove(struct udev_monitor *udev_monitor) {
    (void)udev_monitor;
    FAKE_UDEV_LOG_INFO("STUB: udev_monitor_filter_remove called for monitor %p. Returning 0.", (void*)udev_monitor);
    return 0;
}

// --- udev_enumerate ---
struct udev *udev_enumerate_get_udev(struct udev_enumerate *udev_enumerate) {
    FAKE_UDEV_LOG_INFO("STUB: udev_enumerate_get_udev called for enumerate %p.", (void*)udev_enumerate);
    if (!udev_enumerate) return NULL;
    return udev_enumerate->udev_ctx; // Assuming udev_enumerate struct has udev_ctx
}

int udev_enumerate_add_nomatch_subsystem(struct udev_enumerate *udev_enumerate, const char *subsystem) {
    (void)udev_enumerate; (void)subsystem;
    FAKE_UDEV_LOG_INFO("STUB: udev_enumerate_add_nomatch_subsystem called for enumerate %p, subsystem '%s'. Returning 0.",
                  (void*)udev_enumerate, subsystem ? subsystem : "NULL");
    // This would typically invert the logic of add_match_subsystem or add to a separate list.
    // For a simple stub, just return 0.
    return 0;
}

int udev_enumerate_scan_subsystems(struct udev_enumerate *udev_enumerate) {
    (void)udev_enumerate;
    FAKE_UDEV_LOG_INFO("STUB: udev_enumerate_scan_subsystems called for enumerate %p. Returning 0.", (void*)udev_enumerate);
    // This would scan for subsystems and populate current_scan_results with subsystem names.
    // For a simple stub, clear existing results and return 0.
    if (udev_enumerate && udev_enumerate->current_scan_results) {
        free_udev_list(udev_enumerate->current_scan_results);
        udev_enumerate->current_scan_results = NULL;
    }
    return 0;
}

// --- udev_queue ---
// (Need to define struct udev_queue if not already done, e.g., in libudev.h or locally if opaque)
// Assuming struct udev_queue is defined similarly to udev_monitor or udev_enumerate for ref counting
struct udev_queue {
    struct udev *udev_ctx;
    int n_ref;
    // Add other necessary fields if any specific logic is ever implemented
};


struct udev_queue *udev_queue_new(struct udev *udev) {
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_new called for udev_ctx %p.", (void*)udev);
    if (!udev) return NULL;
    struct udev_queue *q = (struct udev_queue *)calloc(1, sizeof(struct udev_queue));
    if (!q) {
        FAKE_UDEV_LOG_ERROR("calloc failed for udev_queue");
        return NULL;
    }
    q->udev_ctx = udev_ref(udev);
    if (!q->udev_ctx) {
        FAKE_UDEV_LOG_ERROR("udev_ref failed for udev_queue context");
        free(q);
        return NULL;
    }
    q->n_ref = 1;
    FAKE_UDEV_LOG_DEBUG("  Created udev_queue %p", (void*)q);
    return q;
}

struct udev_queue *udev_queue_ref(struct udev_queue *udev_queue) {
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_ref called for queue %p.", (void*)udev_queue);
    if (!udev_queue) return NULL;
    udev_queue->n_ref++;
    FAKE_UDEV_LOG_DEBUG("  udev_queue %p new ref_count %d", (void*)udev_queue, udev_queue->n_ref);
    return udev_queue;
}

struct udev_queue *udev_queue_unref(struct udev_queue *udev_queue) {
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_unref called for queue %p.", (void*)udev_queue);
    if (!udev_queue) return NULL;
    udev_queue->n_ref--;
    FAKE_UDEV_LOG_DEBUG("  udev_queue %p new ref_count %d", (void*)udev_queue, udev_queue->n_ref);
    if (udev_queue->n_ref <= 0) {
        FAKE_UDEV_LOG_DEBUG("  Freeing udev_queue %p", (void*)udev_queue);
        udev_unref(udev_queue->udev_ctx);
        free(udev_queue);
        return NULL;
    }
    return udev_queue;
}

struct udev *udev_queue_get_udev(struct udev_queue *udev_queue) {
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_udev called for queue %p.", (void*)udev_queue);
    if (!udev_queue) return NULL;
    return udev_queue->udev_ctx;
}

unsigned long long int udev_queue_get_kernel_seqnum(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_kernel_seqnum called for queue %p, returning 0.", (void*)udev_queue);
    return 0;
}

unsigned long long int udev_queue_get_udev_seqnum(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_udev_seqnum called for queue %p, returning 0.", (void*)udev_queue);
    return 0;
}

int udev_queue_get_udev_is_active(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_udev_is_active called for queue %p, returning 0 (false).", (void*)udev_queue);
    return 0;
}

int udev_queue_get_queue_is_empty(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_queue_is_empty called for queue %p, returning 1 (true).", (void*)udev_queue);
    return 1; // Typically means empty
}

int udev_queue_get_seqnum_is_finished(struct udev_queue *udev_queue, unsigned long long int seqnum) {
    (void)udev_queue; (void)seqnum;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_seqnum_is_finished called for queue %p, seqnum %llu, returning 1 (true).", (void*)udev_queue, seqnum);
    return 1; // Typically means finished
}

int udev_queue_get_seqnum_sequence_is_finished(struct udev_queue *udev_queue,
                                               unsigned long long int start, unsigned long long int end) {
    (void)udev_queue; (void)start; (void)end;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_seqnum_sequence_is_finished called for queue %p, start %llu, end %llu, returning 1 (true).",
                  (void*)udev_queue, start, end);
    return 1; // Typically means finished
}

int udev_queue_get_fd(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_fd called for queue %p, returning -1.", (void*)udev_queue);
    return -1; // No valid fd for a stub
}

int udev_queue_flush(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_flush called for queue %p, returning 0.", (void*)udev_queue);
    return 0;
}

struct udev_list_entry *udev_queue_get_queued_list_entry(struct udev_queue *udev_queue) {
    (void)udev_queue;
    FAKE_UDEV_LOG_INFO("STUB: udev_queue_get_queued_list_entry called for queue %p, returning NULL.", (void*)udev_queue);
    return NULL;
}

// --- udev_util ---
int udev_util_encode_string(const char *str, char *str_enc, size_t len) {
    FAKE_UDEV_LOG_INFO("STUB: udev_util_encode_string called for str '%s', len %zu.", str ? str : "NULL", len);
    if (!str || !str_enc || len == 0) return 0; // Or -EINVAL
    // Simple passthrough, not actual encoding. Ensure null termination if space.
    size_t copy_len = strlen(str);
    if (copy_len >= len) {
        copy_len = len - 1;
    }
    memcpy(str_enc, str, copy_len);
    str_enc[copy_len] = '\0';
    FAKE_UDEV_LOG_DEBUG("  Copied '%s' to encoded string.", str_enc);
    return (int)copy_len; // Return number of bytes written (excluding null)
}
