/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * A game's view of the session pointer: an SDL2 window in relative mouse mode.
 *
 * SDL2 is what most first-person games read input through, and its relative
 * mode takes the sources a game takes: XInput2 raw motion on X11, and the
 * relative-pointer protocol under a pointer lock on Wayland. A pointer that is
 * warped rather than moved shows up here as a jump or as nothing, which a
 * check of the pointer's position cannot tell apart from a delta. Every event
 * the window sees is printed as one JSON line for the driving test.
 *
 *   SDL_VIDEODRIVER=x11 DISPLAY=:99 sdl_relative_probe [seconds]
 *   SDL_VIDEODRIVER=wayland WAYLAND_DISPLAY=wayland-1 sdl_relative_probe
 *
 * The window covers the desktop so the pointer is over it wherever it starts.
 * libSDL2 is loaded at run time (SDL2_LIB when set), so building this needs no
 * SDL headers; without a libSDL2 to load, one "unavailable" line is printed
 * and the exit status says so, for a suite to skip on rather than fail. Built
 * and run by tests/e2e/pointer_lock.spec.ts.
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

#define UNAVAILABLE_EXIT 3
#define SDL_INIT_VIDEO 0x20u
#define SDL_WINDOWPOS_UNDEFINED 0x1FFF0000
#define SDL_WINDOW_FULLSCREEN_DESKTOP 0x1001u
#define SDL_WINDOW_SHOWN 0x4u
#define SDL_WINDOW_INPUT_FOCUS 0x200u
#define SDL_WINDOW_MOUSE_FOCUS 0x400u
#define SDL_QUIT 0x100u
#define SDL_WINDOWEVENT 0x200u
#define SDL_KEYDOWN 0x300u
#define SDL_KEYUP 0x301u
#define SDL_MOUSEMOTION 0x400u
#define SDL_MOUSEBUTTONDOWN 0x401u
#define SDL_MOUSEBUTTONUP 0x402u
#define SDL_MOUSEWHEEL 0x403u
#define SDL_WINDOWEVENT_FOCUS_GAINED 12

static struct {
    int (*Init)(uint32_t);
    const char *(*GetError)(void);
    const char *(*GetCurrentVideoDriver)(void);
    void *(*CreateWindow)(const char *, int, int, int, int, uint32_t);
    void (*RaiseWindow)(void *);
    void (*DestroyWindow)(void *);
    void *(*GetWindowSurface)(void *);
    int (*FillRect)(void *, const void *, uint32_t);
    int (*UpdateWindowSurface)(void *);
    int (*SetHint)(const char *, const char *);
    int (*SetRelativeMouseMode)(int);
    int (*GetRelativeMouseMode)(void);
    int (*PollEvent)(void *);
    void (*Quit)(void);
} sdl;

static double now(void)
{
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1e6;
}

/* `text` as a JSON string. */
static void json_string(const char *text)
{
    putchar('"');
    for (const unsigned char *c = (const unsigned char *)(text ? text : ""); *c; c++) {
        if (*c == '"' || *c == '\\')
            printf("\\%c", *c);
        else if (*c < 0x20)
            printf("\\u%04x", *c);
        else
            putchar(*c);
    }
    putchar('"');
}

/* The start of a line of `kind`; the caller adds its fields and calls end(). */
static void begin(const char *kind)
{
    printf("{\"kind\": ");
    json_string(kind);
    printf(", \"t\": %.3f", now());
}

static void end(void)
{
    printf("}\n");
    fflush(stdout);
}

static void unavailable(const char *why, const char *detail)
{
    char reason[512];
    snprintf(reason, sizeof reason, "%s%s", why, detail ? detail : "");
    begin("unavailable");
    printf(", \"reason\": ");
    json_string(reason);
    end();
}

static int load(void)
{
    const char *names[] = {getenv("SDL2_LIB"), "libSDL2-2.0.so.0", "libSDL2-2.0.so", "libSDL2.so"};
    void *lib = NULL;
    for (size_t i = 0; i < sizeof names / sizeof *names && !lib; i++)
        if (names[i] && *names[i])
            lib = dlopen(names[i], RTLD_NOW);
    if (!lib)
        return 0;
#define BIND(field, name) \
    do { \
        *(void **)&sdl.field = dlsym(lib, name); \
        if (!sdl.field) \
            return 0; \
    } while (0)
    BIND(Init, "SDL_Init");
    BIND(GetError, "SDL_GetError");
    BIND(GetCurrentVideoDriver, "SDL_GetCurrentVideoDriver");
    BIND(CreateWindow, "SDL_CreateWindow");
    BIND(RaiseWindow, "SDL_RaiseWindow");
    BIND(DestroyWindow, "SDL_DestroyWindow");
    BIND(GetWindowSurface, "SDL_GetWindowSurface");
    BIND(FillRect, "SDL_FillRect");
    BIND(UpdateWindowSurface, "SDL_UpdateWindowSurface");
    BIND(SetHint, "SDL_SetHint");
    BIND(SetRelativeMouseMode, "SDL_SetRelativeMouseMode");
    BIND(GetRelativeMouseMode, "SDL_GetRelativeMouseMode");
    BIND(PollEvent, "SDL_PollEvent");
    BIND(Quit, "SDL_Quit");
#undef BIND
    return 1;
}

static void relative_mode(int rc, const char *err)
{
    begin("relative_mode");
    printf(", \"rc\": %d, \"err\": ", rc);
    json_string(err);
    printf(", \"on\": %s, \"driver\": ", sdl.GetRelativeMouseMode() ? "true" : "false");
    json_string(sdl.GetCurrentVideoDriver());
    end();
}

static void *window;
static int paint_failed;

/* A Wayland surface is mapped, and so reachable by the pointer, only once it
 * carries a buffer; X11 needs none, and one plain fill serves both. A surface
 * SDL cannot give is reported, since the window then never maps. */
static void paint(void)
{
    void *surface = sdl.GetWindowSurface(window);
    if (surface) {
        sdl.FillRect(surface, NULL, 0);
        sdl.UpdateWindowSurface(window);
    } else if (!paint_failed) {
        paint_failed = 1;
        begin("paint_failed");
        printf(", \"err\": ");
        json_string(sdl.GetError());
        end();
    }
}

static const char *window_event(int event)
{
    switch (event) {
    case 1: return "shown";
    case 2: return "hidden";
    case 3: return "exposed";
    case 4: return "moved";
    case 5: return "resized";
    case 6: return "size_changed";
    case 10: return "enter";
    case 11: return "leave";
    case 12: return "focus_gained";
    case 13: return "focus_lost";
    case 14: return "close";
    default: return NULL;
    }
}

#define U32(at) (*(uint32_t *)(ev + (at)))
#define I32(at) (*(int32_t *)(ev + (at)))
#define U16(at) (*(uint16_t *)(ev + (at)))
#define U8(at) (*(uint8_t *)(ev + (at)))

int main(int argc, char **argv)
{
    double duration = argc > 1 ? atof(argv[1]) : 60.0;
    if (!load()) {
        unavailable("no libSDL2 to load", NULL);
        return UNAVAILABLE_EXIT;
    }
    /* On X11 the window's surface goes through the server's own framebuffer
     * path when acceleration is off; left on, SDL's emulation wants a GL
     * renderer and fails on a display without GLX. The Wayland driver has no
     * such path, so there the emulation stays on and draws through EGL. */
    const char *driver = getenv("SDL_VIDEODRIVER");
    if (driver && strcmp(driver, "x11") == 0) {
        sdl.SetHint("SDL_FRAMEBUFFER_ACCELERATION", "0");
        sdl.SetHint("SDL_RENDER_DRIVER", "software");
    }
    if (sdl.Init(SDL_INIT_VIDEO) != 0) {
        unavailable("SDL_Init: ", sdl.GetError());
        return UNAVAILABLE_EXIT;
    }
    uint32_t flags = SDL_WINDOW_SHOWN | SDL_WINDOW_INPUT_FOCUS | SDL_WINDOW_MOUSE_FOCUS | SDL_WINDOW_FULLSCREEN_DESKTOP;
    window = sdl.CreateWindow("relative probe", SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED, 640, 480, flags);
    if (!window) {
        unavailable("SDL_CreateWindow: ", sdl.GetError());
        return UNAVAILABLE_EXIT;
    }
    sdl.RaiseWindow(window);
    paint();
    double painted = now();
    int rc = sdl.SetRelativeMouseMode(1);
    relative_mode(rc, rc ? sdl.GetError() : "");
    /* SDL_Event is 56 bytes; the buffer is the size the Python probe gave it. */
    unsigned char ev[64];
    double deadline = now() + duration;
    while (now() < deadline) {
        while (sdl.PollEvent(ev)) {
            uint32_t type = U32(0);
            if (type == SDL_MOUSEMOTION) {
                begin("motion");
                printf(", \"which\": %u, \"state\": %u, \"x\": %d, \"y\": %d, \"dx\": %d, \"dy\": %d",
                       U32(12), U32(16), I32(20), I32(24), I32(28), I32(32));
                end();
            } else if (type == SDL_MOUSEBUTTONDOWN || type == SDL_MOUSEBUTTONUP) {
                begin("button");
                printf(", \"button\": %u, \"down\": %s, \"x\": %d, \"y\": %d",
                       U8(16), type == SDL_MOUSEBUTTONDOWN ? "true" : "false", I32(20), I32(24));
                end();
            } else if (type == SDL_KEYDOWN || type == SDL_KEYUP) {
                begin("key");
                printf(", \"down\": %s, \"repeat\": %u, \"scancode\": %d, \"sym\": %d, \"mod\": %u",
                       type == SDL_KEYDOWN ? "true" : "false", U8(13), I32(16), I32(20), U16(24));
                end();
            } else if (type == SDL_MOUSEWHEEL) {
                begin("wheel");
                printf(", \"dx\": %d, \"dy\": %d", I32(16), I32(20));
                end();
            } else if (type == SDL_WINDOWEVENT) {
                int event = U8(12);
                const char *name = window_event(event);
                begin("window");
                printf(", \"event\": ");
                if (name)
                    json_string(name);
                else
                    printf("%d", event);
                printf(", \"d1\": %d, \"d2\": %d", I32(16), I32(20));
                end();
                /* A relative mode asked for before the window had focus is
                 * applied by SDL when focus arrives; the state is reported
                 * again so the driver sees it take. */
                if (event == SDL_WINDOWEVENT_FOCUS_GAINED)
                    relative_mode(0, "");
            } else if (type == SDL_QUIT) {
                begin("quit");
                end();
                return 0;
            }
        }
        if (now() - painted > 0.25) {
            paint();
            painted = now();
        }
        usleep(2000);
    }
    sdl.DestroyWindow(window);
    sdl.Quit();
    return 0;
}
