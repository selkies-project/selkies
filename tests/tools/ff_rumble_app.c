/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * An application's force feedback on the session's virtual pad, for the
 * rumble suite: run under the Input Interposer (LD_PRELOAD), it uploads a
 * rumble effect on the pad's evdev node and plays it.
 *
 *   ff_rumble_app           one effect, strong at half and weak at a quarter,
 *                           for 400 ms, played once: {"played": T}
 *   ff_rumble_app endless   an effect without an end, full on the strong motor
 *                           and half on the weak, played until a line on
 *                           standard input asks for its stop:
 *                           {"played": T} then {"stopped": T}
 *
 * T is the time of the write on CLOCK_MONOTONIC, in seconds. Built and run by
 * tests/e2e/gamepad_rumble.spec.ts.
 */
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

static double monotonic(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

static int play(int fd, int id, int value)
{
    struct input_event ev;
    memset(&ev, 0, sizeof ev);
    ev.type = EV_FF;
    ev.code = (unsigned short)id;
    ev.value = value;
    return write(fd, &ev, sizeof ev) == (ssize_t)sizeof ev ? 0 : -1;
}

int main(int argc, char **argv)
{
    int endless = argc > 1 && strcmp(argv[1], "endless") == 0;
    int fd = open("/dev/input/event1000", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        fprintf(stderr, "ff_rumble_app: open: %s\n", strerror(errno));
        return 1;
    }
    struct ff_effect effect;
    memset(&effect, 0, sizeof effect);
    effect.type = FF_RUMBLE;
    effect.id = -1;
    effect.replay.length = endless ? 0 : 400;
    effect.u.rumble.strong_magnitude = endless ? 0xffff : 0x8000;
    effect.u.rumble.weak_magnitude = endless ? 0x8000 : 0x4000;
    if (ioctl(fd, EVIOCSFF, &effect) < 0) {
        fprintf(stderr, "ff_rumble_app: EVIOCSFF: %s\n", strerror(errno));
        return 1;
    }
    double played = monotonic();
    if (play(fd, effect.id, 1) < 0) {
        fprintf(stderr, "ff_rumble_app: play: %s\n", strerror(errno));
        return 1;
    }
    if (!endless) {
        usleep(800000);
        close(fd);
        printf("{\"played\": %.6f}\n", played);
        fflush(stdout);
        return 0;
    }
    printf("{\"played\": %.6f}\n", monotonic());
    fflush(stdout);
    char line[64];
    if (!fgets(line, sizeof line, stdin))
        line[0] = '\0';
    play(fd, effect.id, 0);
    printf("{\"stopped\": %.6f}\n", monotonic());
    fflush(stdout);
    usleep(300000);
    close(fd);
    return 0;
}
