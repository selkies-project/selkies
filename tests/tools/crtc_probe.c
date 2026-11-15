/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * A GDK client that re-reads the CRTCs as MATE's panel does
 * (mate-panel's panel-multimonitor.c): one idle read of the connected
 * outputs' CRTCs after any screen or monitor signal, appended to a file as a
 * JSON line with GDK's monitors at that moment.
 *
 *   crtc_probe READY LOG    (DISPLAY names the X server)
 *
 * READY is created once the signals are connected. Built and run by
 * crates/selkies-suites/tests/lone_display_crtc_follows.rs.
 */
#include <gtk/gtk.h>
#include <X11/Xlib.h>
#include <X11/extensions/Xrandr.h>
#include <stdio.h>
#include <stdlib.h>

#define MAX_RECTS 64

static Display *x;
static Window root;
static GdkDisplay *disp;
static const char *log_path;
static guint pending;
static GHashTable *hooked;

static int compare_rects(const void *a, const void *b) {
    const int *p = a, *q = b;
    for (int i = 0; i < 4; i++)
        if (p[i] != q[i]) return p[i] < q[i] ? -1 : 1;
    return 0;
}

static void write_rects(FILE *f, int (*rects)[4], int n) {
    qsort(rects, (size_t)n, sizeof rects[0], compare_rects);
    fputc('[', f);
    for (int i = 0; i < n; i++)
        fprintf(f, "%s[%d, %d, %d, %d]", i ? ", " : "", rects[i][0], rects[i][1], rects[i][2], rects[i][3]);
    fputc(']', f);
}

static gboolean reread(gpointer unused) {
    (void)unused;
    pending = 0;
    int crtcs[MAX_RECTS][4], ncrtcs = 0;
    XRRScreenResources *res = XRRGetScreenResourcesCurrent(x, root);
    for (int i = 0; res && i < res->noutput && ncrtcs < MAX_RECTS; i++) {
        XRROutputInfo *oi = XRRGetOutputInfo(x, res, res->outputs[i]);
        if (oi && oi->connection != RR_Disconnected && oi->crtc) {
            XRRCrtcInfo *c = XRRGetCrtcInfo(x, res, oi->crtc);
            if (c) {
                crtcs[ncrtcs][0] = c->x;
                crtcs[ncrtcs][1] = c->y;
                crtcs[ncrtcs][2] = (int)c->width;
                crtcs[ncrtcs][3] = (int)c->height;
                ncrtcs++;
                XRRFreeCrtcInfo(c);
            }
        }
        if (oi) XRRFreeOutputInfo(oi);
    }
    if (res) XRRFreeScreenResources(res);

    int monitors[MAX_RECTS][4], nmonitors = 0;
    for (int n = 0; n < gdk_display_get_n_monitors(disp) && nmonitors < MAX_RECTS; n++) {
        GdkRectangle g;
        gdk_monitor_get_geometry(gdk_display_get_monitor(disp, n), &g);
        monitors[nmonitors][0] = g.x;
        monitors[nmonitors][1] = g.y;
        monitors[nmonitors][2] = g.width;
        monitors[nmonitors][3] = g.height;
        nmonitors++;
    }

    FILE *f = fopen(log_path, "a");
    if (f) {
        fputs("{\"crtcs\": ", f);
        write_rects(f, crtcs, ncrtcs);
        fputs(", \"monitors\": ", f);
        write_rects(f, monitors, nmonitors);
        fputs("}\n", f);
        fclose(f);
    }
    return G_SOURCE_REMOVE;
}

static void changed(void);

static void on_invalidate(GdkMonitor *monitor, gpointer data) {
    (void)monitor;
    (void)data;
    changed();
}

static void on_screen(GdkScreen *screen, gpointer data) {
    (void)screen;
    (void)data;
    changed();
}

static void on_monitor(GdkDisplay *display, GdkMonitor *monitor, gpointer data) {
    (void)display;
    (void)monitor;
    (void)data;
    changed();
}

static void changed(void) {
    if (!pending) pending = g_idle_add(reread, NULL);
    for (int n = 0; n < gdk_display_get_n_monitors(disp); n++) {
        GdkMonitor *m = gdk_display_get_monitor(disp, n);
        if (!g_hash_table_contains(hooked, m)) {
            g_hash_table_add(hooked, m);
            g_signal_connect(m, "invalidate", G_CALLBACK(on_invalidate), NULL);
        }
    }
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s READY LOG\n", argv[0]);
        return 2;
    }
    const char *ready = argv[1];
    log_path = argv[2];
    gtk_init(&argc, &argv);
    x = XOpenDisplay(NULL);
    if (!x) {
        fprintf(stderr, "crtc_probe: no X display\n");
        return 1;
    }
    root = DefaultRootWindow(x);
    disp = gdk_display_get_default();
    GdkScreen *screen = gdk_screen_get_default();
    hooked = g_hash_table_new(g_direct_hash, g_direct_equal);
    g_signal_connect(screen, "size-changed", G_CALLBACK(on_screen), NULL);
    g_signal_connect(screen, "monitors-changed", G_CALLBACK(on_screen), NULL);
    g_signal_connect(disp, "monitor-added", G_CALLBACK(on_monitor), NULL);
    g_signal_connect(disp, "monitor-removed", G_CALLBACK(on_monitor), NULL);
    changed();
    FILE *f = fopen(ready, "w");
    if (f) fclose(f);
    gtk_main();
    return 0;
}
