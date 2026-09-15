/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * Signal re-entrancy check for the input interposer, run under its preload.
 *
 * Worker threads loop over open, read, ioctl and close on a plain file while
 * a signal handler reads and writes a pipe, the shape of uvloop's SIGCHLD
 * handler and of an SDL game's own handlers. The signal is blocked on the
 * main thread so it lands on a worker, which may be inside a hooked call when
 * it does. The process exits 0 once the storm has passed; a hook whose lock
 * the handler re-enters never lets it.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <termios.h>
#include <unistd.h>

#define WORKERS 8
#define STORM_SECONDS 3
#define SIGNAL_PERIOD_US 100

static int pipefd[2];
static volatile sig_atomic_t stop = 0;

static void handler(int sig) {
    char c;
    (void)sig;
    if (write(pipefd[1], "x", 1) == 1) {
        (void)!read(pipefd[0], &c, 1);
    }
}

static void *worker(void *arg) {
    char buf[8];
    struct winsize ws;
    (void)arg;
    while (!stop) {
        int fd = open("/dev/null", O_RDONLY);
        if (fd < 0) {
            continue;
        }
        (void)!read(fd, buf, sizeof(buf));
        ioctl(fd, TIOCGWINSZ, &ws);
        close(fd);
    }
    return NULL;
}

int main(void) {
    pthread_t threads[WORKERS];
    struct sigaction sa = {0};
    sigset_t block;
    struct itimerval storm = {{0, SIGNAL_PERIOD_US}, {0, SIGNAL_PERIOD_US}};
    struct itimerval calm = {{0, 0}, {0, 0}};

    if (pipe(pipefd) != 0) {
        perror("pipe");
        return 2;
    }
    sa.sa_handler = handler;
    sigaction(SIGALRM, &sa, NULL);
    for (int i = 0; i < WORKERS; i++) {
        pthread_create(&threads[i], NULL, worker, NULL);
    }
    sigemptyset(&block);
    sigaddset(&block, SIGALRM);
    pthread_sigmask(SIG_BLOCK, &block, NULL);
    setitimer(ITIMER_REAL, &storm, NULL);
    sleep(STORM_SECONDS);
    setitimer(ITIMER_REAL, &calm, NULL);
    stop = 1;
    for (int i = 0; i < WORKERS; i++) {
        pthread_join(threads[i], NULL);
    }
    puts("survived the signal storm");
    return 0;
}
