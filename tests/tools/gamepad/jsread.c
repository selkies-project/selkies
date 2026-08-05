#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <linux/joystick.h>
int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/dev/input/js0";
    int fd = open(path, O_RDONLY);
    if (fd < 0) { printf("open(%s) FAILED: %s\nRESULT events=0\n", path, strerror(errno)); return 1; }
    char name[128] = {0};
    if (ioctl(fd, JSIOCGNAME(sizeof(name)), name) >= 0) printf("JSIOCGNAME: %s\n", name);
    else printf("JSIOCGNAME: failed (%s)\n", strerror(errno));
    struct js_event e; int n = 0;
    for (int i = 0; i < 40 && n < 6; i++) {
        struct pollfd p = { fd, POLLIN, 0 };
        if (poll(&p, 1, 200) <= 0) continue;
        ssize_t k = read(fd, &e, sizeof e);
        if (k == 0) { printf("read: EOF\n"); break; }
        if (k < 0) { printf("read: %s\n", strerror(errno)); break; }
        if (++n <= 3) printf("  event type=0x%02x number=%u value=%d\n", e.type, e.number, e.value);
    }
    printf("RESULT events=%d\n", n);
    close(fd); return 0;
}
