/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * A focused libX11 window that decodes the key events it receives the way an
 * application does, through XLookupString, so the keyboard suites check the X
 * server's own translation rather than a keymap reading of theirs.
 *
 *   x11_keyobs DISPLAY
 *
 * Prints {"kind": "ready"} once the window has the focus, then one line per
 * key event: {"press": true|false, "keycode": N, "group": G, "keysym": N}.
 * A line "sync" on standard input has the server deliver every event it
 * holds first, answered by {"kind": "synced"} after them; the end of
 * standard input ends the program. Built and run by tests/lib/keys.ts.
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* The group bits XKB folds into a key event's state field. */
#define GROUP_SHIFT 13

static void drain(Display *dpy)
{
    while (XPending(dpy)) {
        XEvent ev;
        XNextEvent(dpy, &ev);
        /* The server binds a character no key carries to a spare keycode just
         * before the key event; Xlib's cached keymap learns of it only here. */
        if (ev.type == MappingNotify) {
            XRefreshKeyboardMapping(&ev.xmapping);
            continue;
        }
        if (ev.type != KeyPress && ev.type != KeyRelease)
            continue;
        KeySym ks = 0;
        char buf[16];
        XLookupString(&ev.xkey, buf, sizeof buf, &ks, NULL);
        printf("{\"press\": %s, \"keycode\": %u, \"group\": %u, \"keysym\": %lu}\n",
               ev.type == KeyPress ? "true" : "false", ev.xkey.keycode,
               (ev.xkey.state >> GROUP_SHIFT) & 3, (unsigned long)ks);
    }
    fflush(stdout);
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: x11_keyobs DISPLAY\n");
        return 2;
    }
    Display *dpy = XOpenDisplay(argv[1]);
    if (!dpy) {
        fprintf(stderr, "x11_keyobs: cannot open %s\n", argv[1]);
        return 1;
    }
    Window win = XCreateSimpleWindow(dpy, DefaultRootWindow(dpy), 0, 0, 100, 100, 0, 0, 0);
    XSelectInput(dpy, win, KeyPressMask | KeyReleaseMask);
    XMapWindow(dpy, win);
    XSync(dpy, False);
    XSetInputFocus(dpy, win, RevertToParent, CurrentTime);
    XSync(dpy, False);
    printf("{\"kind\": \"ready\"}\n");
    fflush(stdout);

    /* Unbuffered: a line read leaves what follows it in the pipe, where poll sees it. */
    setvbuf(stdin, NULL, _IONBF, 0);
    struct pollfd fds[2] = {{ConnectionNumber(dpy), POLLIN, 0}, {STDIN_FILENO, POLLIN, 0}};
    char line[64];
    for (;;) {
        drain(dpy);
        if (poll(fds, 2, 100) < 0)
            break;
        if (fds[1].revents & (POLLIN | POLLHUP)) {
            if (!fgets(line, sizeof line, stdin))
                break;
            if (strcmp(line, "sync\n") == 0) {
                XSync(dpy, False);
                drain(dpy);
                printf("{\"kind\": \"synced\"}\n");
                fflush(stdout);
            }
        }
    }
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return 0;
}
