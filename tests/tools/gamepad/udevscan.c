#include <libudev.h>
#include <stdio.h>
#include <string.h>
/* Scans the "input" subsystem (or the one named as the argument) and reports
 * how many devices it holds, how many carry ID_INPUT_JOYSTICK and how many
 * are Selkies virtual pads, then the devnode of each device on its own line. */
int main(int argc, char **argv) {
    const char *subsystem = argc > 1 ? argv[1] : "input";
    struct udev *u = udev_new();
    if (!u) { printf("udev_new failed\nRESULT joydevs=-1\n"); return 1; }
    struct udev_enumerate *e = udev_enumerate_new(u);
    udev_enumerate_add_match_subsystem(e, subsystem);
    udev_enumerate_scan_devices(e);
    struct udev_list_entry *le, *list = udev_enumerate_get_list_entry(e);
    int n = 0, joy = 0, virt = 0;
    udev_list_entry_foreach(le, list) {
        struct udev_device *d = udev_device_new_from_syspath(u, udev_list_entry_get_name(le));
        if (!d) continue;
        n++;
        const char *j = udev_device_get_property_value(d, "ID_INPUT_JOYSTICK");
        const char *dn = udev_device_get_devnode(d);
        const char *sp = udev_device_get_syspath(d);
        if (sp && strncmp(sp, "/sys/devices/virtual/selkies_pad", 32) == 0) virt++;
        if (j && *j == '1') { joy++; printf("  joystick: %s\n", dn ? dn : "(no devnode)"); }
        printf("DEV %s %s\n", sp, dn ? dn : "-");
        udev_device_unref(d);
    }
    udev_enumerate_unref(e);
    udev_unref(u);
    printf("RESULT input_devs=%d joydevs=%d virtual=%d\n", n, joy, virt);
    return 0;
}
