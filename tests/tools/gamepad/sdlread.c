/* Open one joystick through SDL2 and print the events SDL delivers for it.
 *
 * sdlenum only counts what SDL enumerates; this reads a pad, so it shows what
 * an SDL2 application actually gets out of the Joystick Interposer (or a
 * kernel device): name, GUID, vendor/product, control counts, then one line
 * per button, axis and hat event. Runs until SIGINT/SIGTERM, so
 * `timeout 10 sdlread` is a complete run.
 *
 *   sdlread [joystick-index]
 */
#include <SDL2/SDL.h>
#include <linux/input.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>

static volatile sig_atomic_t running = 1;

static void stop(int sig) { (void)sig; running = 0; }

int main(int argc, char **argv) {
    int index = argc > 1 ? atoi(argv[1]) : 0;
    /* Line buffered: the summary and every event line must reach a pipe as they
     * happen, since the usual way to run this is under `timeout`. */
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGINT, stop);
    signal(SIGTERM, stop);

    SDL_version compiled, linked;
    SDL_VERSION(&compiled);
    SDL_GetVersion(&linked);
    printf("SDL %u.%u.%u (compiled against %u.%u.%u), sizeof(struct input_event)=%zu\n",
           linked.major, linked.minor, linked.patch,
           compiled.major, compiled.minor, compiled.patch,
           sizeof(struct input_event));

    if (SDL_Init(SDL_INIT_JOYSTICK) < 0) {
        printf("SDL_Init: %s\nRESULT events=-1\n", SDL_GetError());
        return 1;
    }
    int n = SDL_NumJoysticks();
    printf("Joysticks: %d\n", n);
    for (int i = 0; i < n; i++) printf("  [%d] %s\n", i, SDL_JoystickNameForIndex(i));

    SDL_Joystick *js = index < n ? SDL_JoystickOpen(index) : NULL;
    if (js == NULL) {
        printf("SDL_JoystickOpen(%d): %s\nRESULT events=-1\n", index,
               index < n ? SDL_GetError() : "no joystick at that index");
        SDL_Quit();
        return 1;
    }

    char guid[33];
    SDL_JoystickGetGUIDString(SDL_JoystickGetGUID(js), guid, sizeof(guid));
    printf("Opened [%d] %s\n", index, SDL_JoystickName(js));
    printf("  guid=%s vendor=0x%04X product=0x%04X\n", guid,
           SDL_JoystickGetVendor(js), SDL_JoystickGetProduct(js));
    printf("  axes=%d buttons=%d hats=%d\n", SDL_JoystickNumAxes(js),
           SDL_JoystickNumButtons(js), SDL_JoystickNumHats(js));

    int events = 0;
    SDL_Event e;
    while (running) {
        while (SDL_PollEvent(&e)) {
            switch (e.type) {
            case SDL_JOYAXISMOTION:
                printf("  axis %d value %d\n", e.jaxis.axis, e.jaxis.value);
                events++;
                break;
            case SDL_JOYBUTTONDOWN:
                printf("  button %d down\n", e.jbutton.button);
                events++;
                break;
            case SDL_JOYBUTTONUP:
                printf("  button %d up\n", e.jbutton.button);
                events++;
                break;
            case SDL_JOYHATMOTION:
                printf("  hat %d value %d\n", e.jhat.hat, e.jhat.value);
                events++;
                break;
            case SDL_JOYDEVICEADDED:
                printf("  device added at index %d\n", e.jdevice.which);
                break;
            case SDL_JOYDEVICEREMOVED:
                printf("  device removed, instance %d\n", e.jdevice.which);
                break;
            case SDL_QUIT:
                running = 0;
                break;
            default:
                break;
            }
        }
        SDL_Delay(10);
    }

    printf("RESULT events=%d\n", events);
    SDL_JoystickClose(js);
    SDL_Quit();
    return 0;
}
