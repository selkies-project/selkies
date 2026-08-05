#include <libudev.h>
#include <stdio.h>
#include <string.h>
int main(void) {
    struct udev *u = udev_new();
    if (!u) { printf("udev_new failed\nRESULT joydevs=-1\n"); return 1; }
    struct udev_enumerate *e = udev_enumerate_new(u);
    udev_enumerate_add_match_subsystem(e, "input");
    udev_enumerate_scan_devices(e);
    struct udev_list_entry *le, *list = udev_enumerate_get_list_entry(e);
    int n = 0, joy = 0;
    udev_list_entry_foreach(le, list) {
        struct udev_device *d = udev_device_new_from_syspath(u, udev_list_entry_get_name(le));
        if (!d) continue;
        n++;
        const char *j = udev_device_get_property_value(d, "ID_INPUT_JOYSTICK");
        const char *dn = udev_device_get_devnode(d);
        if (j && *j == '1') { joy++; printf("  joystick: %s\n", dn ? dn : "(no devnode)"); }
        udev_device_unref(d);
    }
    printf("RESULT input_devs=%d joydevs=%d\n", n, joy);
    return 0;
}
