/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * Early-call check for the input interposer, run under its preload.
 *
 * Built twice: with -DEARLY_CALLS_LIB as a shared library whose constructor
 * makes the calls the interposer hooks (an eventfd added to epoll, ioctl, open,
 * read, openat, and close), and without it as the program linked to that
 * library. The loader runs a preloaded library's constructor after those of
 * the program's own libraries, so the calls reach the interposer before its
 * constructor has run, as they do from an application whose libraries set up
 * an event loop in a static initializer. The program exits 0 when every call
 * succeeded and 1, naming each failed call, when one did not.
 */
#ifdef EARLY_CALLS_LIB
#include <fcntl.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <unistd.h>

int early_call_failures = 0;

static void check(int ok, const char *call) {
    if (!ok) {
        perror(call);
        early_call_failures++;
    }
}

__attribute__((constructor)) static void early_calls(void) {
    char buf[8];
    int nonblocking = 1;
    struct epoll_event event = { .events = EPOLLIN };
    int ep = epoll_create1(0);
    int ev = eventfd(0, 0);
    check(ep >= 0 && ev >= 0, "epoll_create1/eventfd");
    check(epoll_ctl(ep, EPOLL_CTL_ADD, ev, &event) == 0, "epoll_ctl");
    check(ioctl(ev, FIONBIO, &nonblocking) == 0, "ioctl");
    int fd = open("/dev/null", O_RDONLY);
    check(fd >= 0, "open");
    check(read(fd, buf, sizeof(buf)) == 0, "read");
    check(close(fd) == 0, "close");
    fd = openat(AT_FDCWD, "/dev/null", O_RDONLY);
    check(fd >= 0, "openat");
    check(close(fd) == 0 && close(ev) == 0 && close(ep) == 0, "close");
}
#else
#include <stdio.h>

extern int early_call_failures;

int main(void) {
    if (early_call_failures) {
        fprintf(stderr, "%d hooked call(s) failed before the interposer's constructor\n",
                early_call_failures);
        return 1;
    }
    return 0;
}
#endif
