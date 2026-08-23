/* libc-level V4L2 capture probe for the webcam tests: enumerates the device,
 * streams N frames in MMAP or read() mode and prints what it saw as key=value
 * lines, including YUV samples at requested pixels so a test can check colours.
 *
 *   v4l2probe [--read] [--sample X,Y]... [--timeout MS] [--duration MS] [--dump FILE] [DEVICE] [FRAMES]
 *
 * --timeout bounds one poll() wait; --duration ends the capture after that long even if fewer
 * than FRAMES arrived (a rate measurement over a fixed window).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void fourcc_str(unsigned f, char out[5]) {
    out[0] = f & 0xff; out[1] = (f >> 8) & 0xff; out[2] = (f >> 16) & 0xff; out[3] = (f >> 24) & 0xff; out[4] = 0;
}

/* Y, U, V of pixel (x, y) for the raw formats the virtual camera advertises. */
static int sample_yuv(const unsigned char *d, unsigned fmt, unsigned w, unsigned h, unsigned bpl,
                      unsigned x, unsigned y, int *Y, int *U, int *V) {
    if (x >= w || y >= h) return -1;
    unsigned cw = (w + 1) / 2;
    if (fmt == V4L2_PIX_FMT_YUV420) {
        unsigned ystride = bpl ? bpl : w;
        const unsigned char *up = d + ystride * h;
        const unsigned char *vp = up + cw * ((h + 1) / 2);
        *Y = d[y * ystride + x]; *U = up[(y / 2) * cw + x / 2]; *V = vp[(y / 2) * cw + x / 2];
        return 0;
    }
    if (fmt == V4L2_PIX_FMT_NV12) {
        unsigned ystride = bpl ? bpl : w;
        const unsigned char *uv = d + ystride * h;
        *Y = d[y * ystride + x]; *U = uv[(y / 2) * cw * 2 + (x / 2) * 2]; *V = uv[(y / 2) * cw * 2 + (x / 2) * 2 + 1];
        return 0;
    }
    if (fmt == V4L2_PIX_FMT_YUYV) {
        unsigned stride = bpl ? bpl : w * 2;
        const unsigned char *row = d + y * stride;
        unsigned pair = (x / 2) * 4;
        *Y = row[x * 2]; *U = row[pair + 1]; *V = row[pair + 3];
        return 0;
    }
    return -1;
}

int main(int argc, char **argv) {
    const char *dev = "/dev/video0";
    int want = 10, use_read = 0, timeout_ms = 5000, duration_ms = 0, nsamples = 0;
    const char *dump_path = NULL;
    unsigned sx[16], sy[16];
    int positional = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--read")) { use_read = 1; continue; }
        if (!strcmp(argv[i], "--sample") && i + 1 < argc && nsamples < 16) {
            if (sscanf(argv[++i], "%u,%u", &sx[nsamples], &sy[nsamples]) == 2) nsamples++;
            continue;
        }
        if (!strcmp(argv[i], "--timeout") && i + 1 < argc) { timeout_ms = atoi(argv[++i]); continue; }
        if (!strcmp(argv[i], "--duration") && i + 1 < argc) { duration_ms = atoi(argv[++i]); continue; }
        if (!strcmp(argv[i], "--dump") && i + 1 < argc) { dump_path = argv[++i]; continue; }
        if (positional == 0) dev = argv[i]; else if (positional == 1) want = atoi(argv[i]);
        positional++;
    }
    int fd = open(dev, O_RDWR | O_NONBLOCK);
    if (fd < 0) { printf("error=open:%s\n", strerror(errno)); return 1; }
    struct v4l2_capability cap; memset(&cap, 0, sizeof cap);
    if (ioctl(fd, VIDIOC_QUERYCAP, &cap) < 0) { printf("error=querycap:%s\n", strerror(errno)); return 1; }
    printf("driver=%s\ncard=%s\ncaps=0x%08x\n", cap.driver, cap.card, cap.device_caps);
    int nfmt = 0;
    for (int i = 0;; i++) {
        struct v4l2_fmtdesc f; memset(&f, 0, sizeof f); f.index = i; f.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if (ioctl(fd, VIDIOC_ENUM_FMT, &f) < 0) break;
        char fc[5]; fourcc_str(f.pixelformat, fc);
        printf("enumfmt=%s flags=%u desc=%s\n", fc, f.flags, f.description);
        nfmt++;
        for (int j = 0;; j++) {
            struct v4l2_frmsizeenum s; memset(&s, 0, sizeof s); s.index = j; s.pixel_format = f.pixelformat;
            if (ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &s) < 0) break;
            if (s.type != V4L2_FRMSIZE_TYPE_DISCRETE) break;
            struct v4l2_frmivalenum iv; memset(&iv, 0, sizeof iv); iv.pixel_format = f.pixelformat;
            iv.width = s.discrete.width; iv.height = s.discrete.height;
            if (ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &iv) == 0 && iv.type == V4L2_FRMIVAL_TYPE_DISCRETE)
                printf("framesize=%ux%u interval=%u/%u\n", s.discrete.width, s.discrete.height, iv.discrete.numerator, iv.discrete.denominator);
            else
                printf("framesize=%ux%u\n", s.discrete.width, s.discrete.height);
        }
    }
    printf("nformats=%d\n", nfmt);
    struct v4l2_format fmt; memset(&fmt, 0, sizeof fmt); fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(fd, VIDIOC_G_FMT, &fmt) < 0) { printf("error=g_fmt:%s\n", strerror(errno)); return 1; }
    if (ioctl(fd, VIDIOC_S_FMT, &fmt) < 0) { printf("error=s_fmt:%s\n", strerror(errno)); return 1; }
    char fc[5]; fourcc_str(fmt.fmt.pix.pixelformat, fc);
    unsigned W = fmt.fmt.pix.width, H = fmt.fmt.pix.height, BPL = fmt.fmt.pix.bytesperline, PF = fmt.fmt.pix.pixelformat;
    printf("format=%s\nwidth=%u\nheight=%u\nbytesperline=%u\nsizeimage=%u\ncolorspace=%u\n", fc, W, H, BPL, fmt.fmt.pix.sizeimage, fmt.fmt.pix.colorspace);
    struct v4l2_streamparm sp; memset(&sp, 0, sizeof sp); sp.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (ioctl(fd, VIDIOC_G_PARM, &sp) == 0)
        printf("timeperframe=%u/%u\n", sp.parm.capture.timeperframe.numerator, sp.parm.capture.timeperframe.denominator);

    unsigned char *last = NULL; size_t last_len = 0; int got = 0; unsigned long long sum = 0; double t0 = 0;
    /* Remaining poll budget: the per-wait timeout, capped by what is left of --duration. */
#define POLL_BUDGET(t0) (duration_ms > 0 ? (int)((duration_ms - (now_s() - (t0)) * 1000.0) < timeout_ms ? (duration_ms - (now_s() - (t0)) * 1000.0) : timeout_ms) : timeout_ms)
#define WINDOW_OPEN(t0) (duration_ms == 0 || (now_s() - (t0)) * 1000.0 < duration_ms)
    if (use_read) {
        size_t sz = fmt.fmt.pix.sizeimage ? fmt.fmt.pix.sizeimage : (4u << 20);
        last = malloc(sz);
        t0 = now_s();
        while (got < want && WINDOW_OPEN(t0)) {
            struct pollfd p = { fd, POLLIN, 0 };
            int budget = POLL_BUDGET(t0);
            if (budget <= 0) break;
            if (poll(&p, 1, budget) <= 0) { if (duration_ms == 0) printf("error=poll-timeout\n"); break; }
            ssize_t n = read(fd, last, sz);
            if (n < 0) { if (errno == EAGAIN) continue; printf("error=read:%s\n", strerror(errno)); break; }
            got++; sum += n; last_len = n;
            if (got <= 3) printf("frame=%d bytes=%zd\n", got, n);
        }
    } else {
        struct v4l2_requestbuffers rb; memset(&rb, 0, sizeof rb); rb.count = 4; rb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE; rb.memory = V4L2_MEMORY_MMAP;
        if (ioctl(fd, VIDIOC_REQBUFS, &rb) < 0) { printf("error=reqbufs:%s\n", strerror(errno)); return 1; }
        printf("buffers=%u\n", rb.count);
        if (rb.count > 16) rb.count = 16;
        void *maps[16]; size_t lens[16];
        for (unsigned i = 0; i < rb.count; i++) {
            struct v4l2_buffer b; memset(&b, 0, sizeof b); b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE; b.memory = V4L2_MEMORY_MMAP; b.index = i;
            if (ioctl(fd, VIDIOC_QUERYBUF, &b) < 0) { printf("error=querybuf:%s\n", strerror(errno)); return 1; }
            maps[i] = mmap(NULL, b.length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, b.m.offset); lens[i] = b.length;
            if (maps[i] == MAP_FAILED) { printf("error=mmap:%s\n", strerror(errno)); return 1; }
            if (ioctl(fd, VIDIOC_QBUF, &b) < 0) { printf("error=qbuf:%s\n", strerror(errno)); return 1; }
        }
        int type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if (ioctl(fd, VIDIOC_STREAMON, &type) < 0) { printf("error=streamon:%s\n", strerror(errno)); return 1; }
        t0 = now_s();
        while (got < want && WINDOW_OPEN(t0)) {
            struct pollfd p = { fd, POLLIN, 0 };
            int budget = POLL_BUDGET(t0);
            if (budget <= 0) break;
            int pr = poll(&p, 1, budget);
            if (pr <= 0) { if (duration_ms == 0) printf("error=poll-timeout\n"); break; }
            struct v4l2_buffer b; memset(&b, 0, sizeof b); b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE; b.memory = V4L2_MEMORY_MMAP;
            if (ioctl(fd, VIDIOC_DQBUF, &b) < 0) { if (errno == EAGAIN) continue; printf("error=dqbuf:%s\n", strerror(errno)); break; }
            got++; sum += b.bytesused;
            if (got <= 3) printf("frame=%d idx=%u bytes=%u seq=%u\n", got, b.index, b.bytesused, b.sequence);
            if (got == want) {
                last = malloc(b.bytesused); memcpy(last, maps[b.index], b.bytesused); last_len = b.bytesused;
            }
            if (ioctl(fd, VIDIOC_QBUF, &b) < 0) { printf("error=qbuf:%s\n", strerror(errno)); break; }
        }
        ioctl(fd, VIDIOC_STREAMOFF, &type);
        for (unsigned i = 0; i < rb.count; i++) munmap(maps[i], lens[i]);
    }
    double dt = now_s() - t0;
    printf("frames=%d\nfps=%.1f\navgbytes=%llu\n", got, dt > 0 ? got / dt : 0.0, got ? sum / got : 0ULL);
    if (last && last_len) {
        printf("first_bytes=%02x%02x%02x%02x\n", last[0], last[1], last[2], last[3]);
        if (dump_path) {
            FILE *df = fopen(dump_path, "wb");
            if (df) { fwrite(last, 1, last_len, df); fclose(df); printf("dumped=%zu\n", last_len); }
        }
        for (int i = 0; i < nsamples; i++) {
            int Y, U, V;
            if (sample_yuv(last, PF, W, H, BPL, sx[i], sy[i], &Y, &U, &V) == 0)
                printf("sample=%u,%u:%d,%d,%d\n", sx[i], sy[i], Y, U, V);
        }
    }
    free(last);
    close(fd);
    if (duration_ms > 0) return 0;
    return got == want ? 0 : 2;
}
