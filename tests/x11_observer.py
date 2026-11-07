#!/usr/bin/env python3
"""A focused libX11 window that decodes the key events it receives the way an application
does, on a connection of its own (`Observer`): what the keyboard suites read typed keys back
through, so they check the X server's own translation rather than a keymap reading of theirs.
"""
import ctypes
import time

XKB_USE_CORE_KBD = 0x0100
KEY_PRESS, KEY_RELEASE = 2, 3
KEY_PRESS_MASK, KEY_RELEASE_MASK = 1 << 0, 1 << 1
REVERT_TO_PARENT = 2
# The group bits XKB folds into a key event's state field.
GROUP_SHIFT = 13


class XKeyEvent(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong), ("send_event", ctypes.c_int),
                ("display", ctypes.c_void_p), ("window", ctypes.c_ulong), ("root", ctypes.c_ulong),
                ("subwindow", ctypes.c_ulong), ("time", ctypes.c_ulong), ("x", ctypes.c_int),
                ("y", ctypes.c_int), ("x_root", ctypes.c_int), ("y_root", ctypes.c_int),
                ("state", ctypes.c_uint), ("keycode", ctypes.c_uint), ("same_screen", ctypes.c_int)]


class XEvent(ctypes.Union):
    _fields_ = [("type", ctypes.c_int), ("xkey", XKeyEvent), ("pad", ctypes.c_long * 24)]


class XkbStateRec(ctypes.Structure):
    _fields_ = [("group", ctypes.c_ubyte), ("locked_group", ctypes.c_ubyte),
                ("base_group", ctypes.c_ushort), ("latched_group", ctypes.c_ushort),
                ("mods", ctypes.c_ubyte), ("base_mods", ctypes.c_ubyte), ("latched_mods", ctypes.c_ubyte),
                ("locked_mods", ctypes.c_ubyte), ("compat_state", ctypes.c_ubyte),
                ("grab_mods", ctypes.c_ubyte), ("compat_grab_mods", ctypes.c_ubyte),
                ("lookup_mods", ctypes.c_ubyte), ("compat_lookup_mods", ctypes.c_ubyte),
                ("ptr_buttons", ctypes.c_ushort)]


def _libx11() -> ctypes.CDLL:
    lib = ctypes.CDLL("libX11.so.6")
    lib.XOpenDisplay.restype = ctypes.c_void_p
    lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    lib.XDefaultRootWindow.restype = ctypes.c_ulong
    lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    lib.XCreateSimpleWindow.restype = ctypes.c_ulong
    lib.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                        ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong,
                                        ctypes.c_ulong]
    lib.XSelectInput.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_long]
    lib.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    lib.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    lib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    lib.XPending.argtypes = [ctypes.c_void_p]
    lib.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.POINTER(XEvent)]
    lib.XLookupString.argtypes = [ctypes.POINTER(XKeyEvent), ctypes.c_char_p, ctypes.c_int,
                                  ctypes.POINTER(ctypes.c_ulong), ctypes.c_void_p]
    lib.XkbGetState.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.POINTER(XkbStateRec)]
    lib.XkbLockGroup.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_uint]
    lib.XDestroyWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    return lib


class Observer:
    """A focused libX11 window that decodes the key events it receives the way
    an application does, on a connection of its own."""

    def __init__(self, display_name: str) -> None:
        self.lib = _libx11()
        self.dpy = self.lib.XOpenDisplay(display_name.encode())
        if not self.dpy:
            raise RuntimeError("XOpenDisplay failed for the observer")
        root = self.lib.XDefaultRootWindow(self.dpy)
        self.win = self.lib.XCreateSimpleWindow(self.dpy, root, 0, 0, 100, 100, 0, 0, 0)
        self.lib.XSelectInput(self.dpy, self.win, KEY_PRESS_MASK | KEY_RELEASE_MASK)
        self.lib.XMapWindow(self.dpy, self.win)
        self.lib.XSync(self.dpy, 0)
        self.lib.XSetInputFocus(self.dpy, self.win, REVERT_TO_PARENT, 0)
        self.lib.XSync(self.dpy, 0)

    def locked_group(self) -> int:
        st = XkbStateRec()
        self.lib.XkbGetState(self.dpy, XKB_USE_CORE_KBD, ctypes.byref(st))
        return st.locked_group

    def lock_group(self, group: int) -> None:
        self.lib.XkbLockGroup(self.dpy, XKB_USE_CORE_KBD, group)
        self.lib.XSync(self.dpy, 0)

    def drain(self, settle: float = 0.25) -> list:
        """Key events received since the last drain: `(press, keycode, group, keysym)`."""
        time.sleep(settle)
        self.lib.XSync(self.dpy, 0)
        out = []
        ev = XEvent()
        while self.lib.XPending(self.dpy):
            self.lib.XNextEvent(self.dpy, ctypes.byref(ev))
            if ev.type not in (KEY_PRESS, KEY_RELEASE):
                continue
            ks = ctypes.c_ulong(0)
            buf = ctypes.create_string_buffer(16)
            self.lib.XLookupString(ctypes.byref(ev.xkey), buf, 16, ctypes.byref(ks), None)
            out.append((ev.type == KEY_PRESS, ev.xkey.keycode,
                        (ev.xkey.state >> GROUP_SHIFT) & 3, ks.value))
        return out

    def close(self) -> None:
        self.lib.XDestroyWindow(self.dpy, self.win)
        self.lib.XCloseDisplay(self.dpy)
