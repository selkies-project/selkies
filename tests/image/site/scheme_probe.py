#!/usr/bin/env python3
"""What one toolkit in the session sees of the desktop's color scheme, live.

Started once in the session and left running, the probe writes the toolkit's
own reading to a JSON file at once and again on every change it is told of,
as a running application would restyle: GTK 3's theme name (`gtk3`), GTK 4's
theme name and interface color scheme (`gtk4`), libadwaita's dark flag
(`adw`), or Qt's color scheme and the lightness of the window color its palette
draws with (`qt`). A toolkit the session has no bindings
for writes `{"missing": reason}` and ends.

    python3 scheme_probe.py gtk3|gtk4|adw|qt OUT.json
"""
import json
import os
import sys
from typing import Any, Callable


def writer(out: str) -> Callable[[dict], None]:
    """A function that replaces `out` with a reading, never leaving it half-written."""
    def write(reading: dict) -> None:
        with open(out + ".tmp", "w") as f:
            json.dump(reading, f)
        os.replace(out + ".tmp", out)
    return write


def gtk3(write: Callable[[dict], None]) -> None:
    import gi
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gtk
    settings = Gtk.Settings.get_default()

    def tell(*_: Any) -> None:
        write({"theme": settings.props.gtk_theme_name})
    settings.connect("notify::gtk-theme-name", tell)
    tell()
    Gtk.main()


def gtk4(write: Callable[[dict], None]) -> None:
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import GLib, Gtk
    Gtk.init()
    settings = Gtk.Settings.get_default()
    has_scheme = settings.find_property("gtk-interface-color-scheme") is not None

    def tell(*_: Any) -> None:
        reading = {"theme": settings.props.gtk_theme_name}
        if has_scheme:
            reading["scheme"] = settings.get_property("gtk-interface-color-scheme").value_nick
        write(reading)
    settings.connect("notify::gtk-theme-name", tell)
    if has_scheme:
        settings.connect("notify::gtk-interface-color-scheme", tell)
    tell()
    GLib.MainLoop().run()


def adw(write: Callable[[dict], None]) -> None:
    import gi
    gi.require_version("Adw", "1")
    from gi.repository import Adw, GLib
    Adw.init()
    manager = Adw.StyleManager.get_default()

    def tell(*_: Any) -> None:
        write({"dark": manager.get_dark(), "system_supports": manager.get_system_supports_color_schemes()})
    manager.connect("notify::dark", tell)
    tell()
    GLib.MainLoop().run()


def qt(write: Callable[[dict], None]) -> None:
    from PyQt6.QtCore import qVersion
    from PyQt6.QtGui import QGuiApplication, QPalette
    app = QGuiApplication(sys.argv[:1])
    hints = app.styleHints()

    def tell(*_: Any) -> None:
        # The scheme Qt names, and the lightness (0-255) of the window color it draws with,
        # which is what a user sees where the platform theme names no scheme.
        window = app.palette().color(QPalette.ColorRole.Window).lightness()
        write({"scheme": hints.colorScheme().name, "window_lightness": window, "version": qVersion()})
    hints.colorSchemeChanged.connect(tell)
    app.paletteChanged.connect(tell)
    tell()
    app.exec()


def main() -> None:
    kind, out = sys.argv[1], sys.argv[2]
    write = writer(out)
    probe = {"gtk3": gtk3, "gtk4": gtk4, "adw": adw, "qt": qt}[kind]
    try:
        probe(write)
    except (ImportError, ValueError) as e:
        write({"missing": f"{type(e).__name__}: {e}"})


if __name__ == "__main__":
    main()
