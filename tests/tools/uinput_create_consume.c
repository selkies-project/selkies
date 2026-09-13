/* Exercises the interposer's app-created uinput path with no kernel node: one
   process opens /dev/uinput and builds a device, a second opens the resulting
   /dev/input/eventN and checks its identity (EVIOCG*) and that the event stream
   arrives byte for byte. Both run under the interposer preload. Prints PASS or
   FAIL and exits non-zero on failure. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static const struct { unsigned short type, code; int value; } CYCLE[] = {
    { EV_KEY, BTN_A, 1 }, { EV_SYN, SYN_REPORT, 0 },
    { EV_KEY, BTN_A, 0 }, { EV_SYN, SYN_REPORT, 0 },
    { EV_ABS, ABS_X, 1234 }, { EV_SYN, SYN_REPORT, 0 },
};
#define NCYC (int)(sizeof(CYCLE) / sizeof(CYCLE[0]))
#define DEVNAME "Selkies Test Pad"

static int run_creator(int rd, int wr) {
    int fd = open("/dev/uinput", O_RDWR | O_NONBLOCK);
    if (fd < 0) { dprintf(2, "creator open: %s\n", strerror(errno)); return 1; }
    ioctl(fd, UI_SET_EVBIT, EV_KEY); ioctl(fd, UI_SET_EVBIT, EV_ABS); ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_KEYBIT, BTN_A); ioctl(fd, UI_SET_KEYBIT, BTN_GAMEPAD);
    ioctl(fd, UI_SET_ABSBIT, ABS_X);
    struct uinput_abs_setup abs = { .code = ABS_X };
    abs.absinfo.minimum = -32767; abs.absinfo.maximum = 32767;
    ioctl(fd, UI_ABS_SETUP, &abs);
    struct uinput_setup us;
    memset(&us, 0, sizeof us);
    us.id.bustype = BUS_USB; us.id.vendor = 0x1234; us.id.product = 0x5678; us.id.version = 0x0111;
    snprintf(us.name, sizeof us.name, "%s", DEVNAME);
    if (ioctl(fd, UI_DEV_SETUP, &us) < 0) { dprintf(2, "UI_DEV_SETUP: %s\n", strerror(errno)); return 1; }
    if (ioctl(fd, UI_DEV_CREATE, 0) < 0) { dprintf(2, "UI_DEV_CREATE: %s\n", strerror(errno)); return 1; }
    char sys[64] = {0};
    ioctl(fd, UI_GET_SYSNAME(sizeof sys), sys);
    dprintf(wr, "%s\n", sys);
    char go = 0;
    if (read(rd, &go, 1) != 1) return 1;
    for (int rep = 0; rep < 100; rep++) {
        for (int i = 0; i < NCYC; i++) {
            struct input_event ev;
            memset(&ev, 0, sizeof ev);
            ev.type = CYCLE[i].type; ev.code = CYCLE[i].code; ev.value = CYCLE[i].value;
            if (write(fd, &ev, sizeof ev) != (ssize_t)sizeof ev) { dprintf(2, "write: %s\n", strerror(errno)); return 1; }
        }
        usleep(20000);
    }
    return 0;
}

static int run_consumer(int rd, int wr) {
    char sys[64] = {0};
    FILE *f = fdopen(rd, "r");
    if (!f || !fgets(sys, sizeof sys, f)) return 1;
    sys[strcspn(sys, "\n")] = 0;
    char path[128];
    snprintf(path, sizeof path, "/dev/input/%s", sys);
    int fd = -1;
    for (int t = 0; t < 200 && fd < 0; t++) { fd = open(path, O_RDONLY); if (fd < 0) usleep(10000); }
    if (fd < 0) { dprintf(2, "consumer open %s: %s\n", path, strerror(errno)); return 1; }

    char name[80] = {0}; ioctl(fd, EVIOCGNAME(sizeof name), name);
    struct input_id id; memset(&id, 0, sizeof id); ioctl(fd, EVIOCGID, &id);
    unsigned char keybits[(KEY_MAX / 8) + 1] = {0}; ioctl(fd, EVIOCGBIT(EV_KEY, sizeof keybits), keybits);
    struct input_absinfo ai; memset(&ai, 0, sizeof ai); ioctl(fd, EVIOCGABS(ABS_X), &ai);
    int name_ok = strcmp(name, DEVNAME) == 0;
    int id_ok = id.vendor == 0x1234 && id.product == 0x5678;
    int key_ok = (keybits[BTN_A / 8] >> (BTN_A % 8)) & 1;
    int abs_ok = ai.minimum == -32767 && ai.maximum == 32767;
    dprintf(2, "identity: name='%s'(%d) vendor=%04x product=%04x(%d) BTN_A=%d absX[%d,%d](%d)\n",
            name, name_ok, id.vendor, id.product, id_ok, key_ok, ai.minimum, ai.maximum, abs_ok);

    char go = 1;
    if (write(wr, &go, 1) != 1) return 1;

    /* Slide a window over the stream; succeed on a full cycle match, so the
       accept and the first events racing cannot flake the check. */
    struct input_event win[NCYC];
    int have = 0, matched = 0;
    for (int loops = 0; loops < 4000 && !matched; loops++) {
        struct input_event ev;
        ssize_t n = read(fd, &ev, sizeof ev);
        if (n != (ssize_t)sizeof ev) { if (n < 0) usleep(2000); continue; }
        if (have < NCYC) win[have++] = ev;
        else { memmove(win, win + 1, sizeof(win) - sizeof(win[0])); win[NCYC - 1] = ev; }
        if (have == NCYC) {
            matched = 1;
            for (int i = 0; i < NCYC; i++)
                if (win[i].type != CYCLE[i].type || win[i].code != CYCLE[i].code || win[i].value != CYCLE[i].value)
                    { matched = 0; break; }
        }
    }
    dprintf(2, "stream match=%d\n", matched);
    close(fd);
    return (matched && name_ok && id_ok && key_ok && abs_ok) ? 0 : 1;
}

static volatile sig_atomic_t hold_stop;
static void hold_onterm(int sig) { (void)sig; hold_stop = 1; }

/* Creates a gamepad-capable device and holds it open, printing its node name,
   so a separate udev/scan check can observe it. On a signal it closes the
   device (the graceful teardown that unlinks its socket) and exits. */
static int run_hold(void) {
    signal(SIGTERM, hold_onterm);
    signal(SIGINT, hold_onterm);
    int fd = open("/dev/uinput", O_RDWR | O_NONBLOCK);
    if (fd < 0) { dprintf(2, "hold open: %s\n", strerror(errno)); return 1; }
    ioctl(fd, UI_SET_EVBIT, EV_KEY); ioctl(fd, UI_SET_EVBIT, EV_ABS); ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_KEYBIT, BTN_GAMEPAD); ioctl(fd, UI_SET_KEYBIT, BTN_SOUTH); ioctl(fd, UI_SET_ABSBIT, ABS_X);
    struct uinput_setup us;
    memset(&us, 0, sizeof us);
    us.id.bustype = BUS_USB; us.id.vendor = 0x0aaa; us.id.product = 0x0bbb; us.id.version = 1;
    snprintf(us.name, sizeof us.name, "%s", DEVNAME);
    ioctl(fd, UI_DEV_SETUP, &us);
    if (ioctl(fd, UI_DEV_CREATE, 0) < 0) { dprintf(2, "hold UI_DEV_CREATE: %s\n", strerror(errno)); return 1; }
    char sys[64] = {0};
    ioctl(fd, UI_GET_SYSNAME(sizeof sys), sys);
    printf("%s\n", sys);
    fflush(stdout);
    while (!hold_stop) pause();
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "hold") == 0) return run_hold();
    int c2p[2], p2c[2];
    if (pipe(c2p) || pipe(p2c)) return 2;
    pid_t pid = fork();
    if (pid < 0) return 2;
    if (pid == 0) { close(c2p[1]); close(p2c[0]); _exit(run_consumer(c2p[0], p2c[1])); }
    close(c2p[0]); close(p2c[1]);
    int crc = run_creator(p2c[0], c2p[1]);
    int st = 0;
    waitpid(pid, &st, 0);
    int urc = WIFEXITED(st) ? WEXITSTATUS(st) : 1;
    int ok = (crc == 0 && urc == 0);
    printf("%s (creator=%d consumer=%d)\n", ok ? "PASS" : "FAIL", crc, urc);
    return ok ? 0 : 1;
}
