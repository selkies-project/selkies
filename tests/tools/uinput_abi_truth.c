#include <linux/uinput.h>
#include <linux/input.h>
#include <linux/joystick.h>
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
    printf("sizeof_js_event %zu\n", sizeof(struct js_event));
    printf("off_js_value %zu\n", __builtin_offsetof(struct js_event, value));
    printf("off_js_type %zu\n", __builtin_offsetof(struct js_event, type));
    printf("off_js_number %zu\n", __builtin_offsetof(struct js_event, number));
    printf("JS_EVENT_BUTTON %d\n", JS_EVENT_BUTTON);
    printf("JS_EVENT_AXIS %d\n", JS_EVENT_AXIS);
    printf("JS_EVENT_INIT %d\n", JS_EVENT_INIT);
    printf("UI_SET_FFBIT %lu\n", (unsigned long)UI_SET_FFBIT);
    printf("UI_SET_PHYS %lu\n", (unsigned long)UI_SET_PHYS);
    printf("UI_BEGIN_FF_UPLOAD %lu\n", (unsigned long)UI_BEGIN_FF_UPLOAD);
    printf("UI_END_FF_UPLOAD %lu\n", (unsigned long)UI_END_FF_UPLOAD);
    printf("UI_BEGIN_FF_ERASE %lu\n", (unsigned long)UI_BEGIN_FF_ERASE);
    printf("UI_END_FF_ERASE %lu\n", (unsigned long)UI_END_FF_ERASE);
    printf("EV_UINPUT %d\n", EV_UINPUT);
    printf("UI_FF_UPLOAD %d\n", UI_FF_UPLOAD);
    printf("UI_FF_ERASE %d\n", UI_FF_ERASE);
    printf("EV_FF %d\n", EV_FF);
    printf("FF_RUMBLE %d\n", FF_RUMBLE);
    printf("FF_PERIODIC %d\n", FF_PERIODIC);
    printf("FF_SQUARE %d\n", FF_SQUARE);
    printf("FF_TRIANGLE %d\n", FF_TRIANGLE);
    printf("FF_SINE %d\n", FF_SINE);
    printf("FF_GAIN %d\n", FF_GAIN);
    printf("sizeof_ff_effect %zu\n", sizeof(struct ff_effect));
    printf("off_ff_replay %zu\n", __builtin_offsetof(struct ff_effect, replay));
    printf("off_ff_u %zu\n", __builtin_offsetof(struct ff_effect, u));
    printf("off_periodic_magnitude %zu\n", __builtin_offsetof(struct ff_periodic_effect, magnitude));
    printf("sizeof_uinput_ff_upload %zu\n", sizeof(struct uinput_ff_upload));
    printf("off_ff_upload_effect %zu\n", __builtin_offsetof(struct uinput_ff_upload, effect));
    printf("sizeof_uinput_ff_erase %zu\n", sizeof(struct uinput_ff_erase));
    return 0;
}
