#include <linux/uinput.h>
#include <linux/input.h>
#include <stdio.h>
int main(void) {
    printf("UI_DEV_CREATE %lu\n",  (unsigned long)UI_DEV_CREATE);
    printf("UI_DEV_DESTROY %lu\n", (unsigned long)UI_DEV_DESTROY);
    printf("UI_DEV_SETUP %lu\n",   (unsigned long)UI_DEV_SETUP);
    printf("UI_ABS_SETUP %lu\n",   (unsigned long)UI_ABS_SETUP);
    printf("UI_SET_EVBIT %lu\n",   (unsigned long)UI_SET_EVBIT);
    printf("UI_SET_KEYBIT %lu\n",  (unsigned long)UI_SET_KEYBIT);
    printf("UI_SET_ABSBIT %lu\n",  (unsigned long)UI_SET_ABSBIT);
    printf("UI_GET_SYSNAME %lu\n", (unsigned long)UI_GET_SYSNAME(64));
    printf("sizeof_uinput_setup %zu\n", sizeof(struct uinput_setup));
    printf("sizeof_uinput_abs_setup %zu\n", sizeof(struct uinput_abs_setup));
    printf("sizeof_input_event %zu\n", sizeof(struct input_event));
    printf("off_abs_setup_absinfo %zu\n", __builtin_offsetof(struct uinput_abs_setup, absinfo));
    printf("off_setup_name %zu\n", __builtin_offsetof(struct uinput_setup, name));
    printf("off_setup_ff %zu\n", __builtin_offsetof(struct uinput_setup, ff_effects_max));
    printf("UINPUT_MAX_NAME_SIZE %d\n", UINPUT_MAX_NAME_SIZE);
    printf("BUS_USB %d\n", BUS_USB);
    return 0;
}
