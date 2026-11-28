/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * A GTK 3 window holding one focused text entry, for the suites that type
 * into a toolkit application rather than a raw key observer: GTK takes its
 * keys through its input method context, which on Wayland speaks
 * zwp_text_input_v3 where the compositor offers it.
 *
 * With the argument `fixed` the window keeps its 640x360 size, so a window
 * manager places it rather than maximizing it.
 *
 * One JSON line per event on stdout, every position in the window's surface
 * coordinates (its GdkWindow's, the wl_surface's on Wayland):
 *   {"kind":"ready"}                         the entry has the keyboard focus
 *   {"kind":"text","text":"...","caret":[x,y,h]}
 *                                            the entry's text and where its
 *                                            caret is drawn, after each change
 *   {"kind":"pointer","x":X,"y":Y}           the pointer, on each motion and
 *                                            crossing
 */
#include <gtk/gtk.h>
#include <stdio.h>

static GtkWidget *window;
static GtkWidget *entry;

/* The text as a JSON string body. */
static void print_json_string(const char *s) {
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
        if (*p == '"' || *p == '\\') printf("\\%c", *p);
        else if (*p < 0x20) printf("\\u%04x", *p);
        else putchar(*p);
    }
}

/* Where the entry draws its caret: the layout's cursor position at the
 * entry's cursor, moved from the entry's coordinates into the window's. */
static gboolean report_text(gpointer data) {
    (void)data;
    GtkEntry *e = GTK_ENTRY(entry);
    const char *text = gtk_entry_get_text(e);
    gint offset = gtk_editable_get_position(GTK_EDITABLE(e));
    gint byte = (gint)(g_utf8_offset_to_pointer(text, offset) - text);
    PangoRectangle strong;
    pango_layout_get_cursor_pos(gtk_entry_get_layout(e), gtk_entry_text_index_to_layout_index(e, byte), &strong, NULL);
    gint lx = 0, ly = 0, wx = 0, wy = 0;
    gtk_entry_get_layout_offsets(e, &lx, &ly);
    gtk_widget_translate_coordinates(entry, window, lx + PANGO_PIXELS(strong.x), ly + PANGO_PIXELS(strong.y), &wx, &wy);
    printf("{\"kind\":\"text\",\"text\":\"");
    print_json_string(text);
    printf("\",\"caret\":[%d,%d,%d]}\n", wx, wy, PANGO_PIXELS(strong.height));
    fflush(stdout);
    return G_SOURCE_REMOVE;
}

static void on_changed(GtkEditable *editable, gpointer data) {
    (void)editable;
    (void)data;
    g_idle_add(report_text, NULL);
}

static gboolean on_focus_in(GtkWidget *widget, GdkEvent *event, gpointer data) {
    (void)widget;
    (void)event;
    (void)data;
    printf("{\"kind\":\"ready\"}\n");
    fflush(stdout);
    g_idle_add(report_text, NULL);
    return FALSE;
}

/* Every motion, and every entry into one of the window's GdkWindows (a
 * single move onto the entry's text area is a crossing alone), moved into the
 * window's own coordinates. */
static void on_event(GdkEvent *event, gpointer data) {
    (void)data;
    GdkWindow *top = gtk_widget_get_window(window);
    gdouble x = 0, y = 0;
    if (top && gdk_event_get_coords(event, &x, &y) &&
        (event->type == GDK_MOTION_NOTIFY || event->type == GDK_ENTER_NOTIFY)) {
        for (GdkWindow *w = event->any.window; w && w != top; w = gdk_window_get_parent(w)) {
            gdk_window_coords_to_parent(w, x, y, &x, &y);
        }
        printf("{\"kind\":\"pointer\",\"x\":%d,\"y\":%d}\n", (int)x, (int)y);
        fflush(stdout);
    }
    gtk_main_do_event(event);
}

int main(int argc, char **argv) {
    gtk_init(&argc, &argv);
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "gtk-entry-probe");
    gtk_window_set_default_size(GTK_WINDOW(window), 640, 360);
    gboolean fixed = argc > 1 && g_strcmp0(argv[1], "fixed") == 0;
    if (fixed) gtk_window_set_resizable(GTK_WINDOW(window), FALSE);
    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_widget_set_margin_start(box, 120);
    gtk_widget_set_margin_top(box, 90);
    gtk_widget_set_margin_end(box, 120);
    entry = gtk_entry_new();
    gtk_box_pack_start(GTK_BOX(box), entry, FALSE, FALSE, 0);
    gtk_container_add(GTK_CONTAINER(window), box);
    g_signal_connect(entry, "changed", G_CALLBACK(on_changed), NULL);
    g_signal_connect(entry, "focus-in-event", G_CALLBACK(on_focus_in), NULL);
    g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
    gdk_event_handler_set(on_event, NULL, NULL);
    gtk_widget_show_all(window);
    gtk_widget_grab_focus(entry);
    if (fixed) gtk_window_unmaximize(GTK_WINDOW(window));
    gtk_main();
    return 0;
}
