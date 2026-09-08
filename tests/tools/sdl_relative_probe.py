#!/usr/bin/env python3
"""A game's view of the session pointer: an SDL2 window in relative mouse mode.

SDL2 is what most first-person games read input through, and its relative
mode takes the sources a game takes: XInput2 raw motion on X11, and the
relative-pointer protocol under a pointer lock on Wayland. A pointer that is
warped rather than moved shows up here as a jump or as nothing, which a check
of the pointer's position cannot tell apart from a delta. Every event the
window sees is printed as one JSON line for the driving test.

    SDL_VIDEODRIVER=x11 DISPLAY=:99 python3 sdl_relative_probe.py [seconds]
    SDL_VIDEODRIVER=wayland WAYLAND_DISPLAY=wayland-1 python3 sdl_relative_probe.py

The window covers the desktop so the pointer is over it wherever it starts.
Without a libSDL2 to load, one `unavailable` line is printed and the exit
status says so, for a suite to skip on rather than fail.
"""
import ctypes
import ctypes.util
import json
import os
import struct
import sys
import time

UNAVAILABLE_EXIT = 3

SDL_INIT_VIDEO = 0x20
SDL_WINDOWPOS_UNDEFINED = 0x1FFF0000
SDL_WINDOW_FULLSCREEN_DESKTOP = 0x1001
SDL_WINDOW_SHOWN = 0x4
SDL_WINDOW_INPUT_FOCUS = 0x200
SDL_WINDOW_MOUSE_FOCUS = 0x400
SDL_QUIT = 0x100
SDL_WINDOWEVENT = 0x200
SDL_KEYDOWN = 0x300
SDL_KEYUP = 0x301
SDL_MOUSEMOTION = 0x400
SDL_MOUSEBUTTONDOWN = 0x401
SDL_MOUSEBUTTONUP = 0x402
SDL_MOUSEWHEEL = 0x403
SDL_WINDOWEVENT_FOCUS_GAINED = 12
WINDOW_EVENTS = {1: "shown", 2: "hidden", 3: "exposed", 4: "moved", 5: "resized",
                 6: "size_changed", 10: "enter", 11: "leave", 12: "focus_gained",
                 13: "focus_lost", 14: "close"}


def emit(kind: str, **kv) -> None:
    print(json.dumps({"kind": kind, "t": round(time.time(), 3), **kv}), flush=True)


def load():
    """libSDL2, from `SDL2_LIB` when set, else wherever the loader finds it."""
    names = [os.environ.get("SDL2_LIB"), "libSDL2-2.0.so.0", ctypes.util.find_library("SDL2-2.0")]
    for name in names:
        if not name:
            continue
        try:
            return ctypes.CDLL(name)
        except OSError:
            continue
    return None


def relative_mode(sdl) -> dict:
    return {"on": bool(sdl.SDL_GetRelativeMouseMode()),
            "driver": (sdl.SDL_GetCurrentVideoDriver() or b"").decode()}


def main() -> int:
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 60.0
    sdl = load()
    if sdl is None:
        emit("unavailable", reason="no libSDL2 to load")
        return UNAVAILABLE_EXIT
    sdl.SDL_GetError.restype = ctypes.c_char_p
    sdl.SDL_GetCurrentVideoDriver.restype = ctypes.c_char_p
    sdl.SDL_CreateWindow.restype = ctypes.c_void_p
    sdl.SDL_CreateWindow.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int,
                                     ctypes.c_int, ctypes.c_int, ctypes.c_uint32]
    sdl.SDL_RaiseWindow.argtypes = [ctypes.c_void_p]
    sdl.SDL_DestroyWindow.argtypes = [ctypes.c_void_p]
    sdl.SDL_GetWindowSurface.restype = ctypes.c_void_p
    sdl.SDL_GetWindowSurface.argtypes = [ctypes.c_void_p]
    sdl.SDL_FillRect.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]
    sdl.SDL_UpdateWindowSurface.argtypes = [ctypes.c_void_p]
    # On X11 the window's surface goes through the server's own framebuffer
    # path when acceleration is off; left on, SDL's emulation wants a GL
    # renderer and fails on a display without GLX. The Wayland driver has no
    # such path, so there the emulation stays on and draws through EGL.
    if os.environ.get("SDL_VIDEODRIVER", "") == "x11":
        sdl.SDL_SetHint(b"SDL_FRAMEBUFFER_ACCELERATION", b"0")
        sdl.SDL_SetHint(b"SDL_RENDER_DRIVER", b"software")
    if sdl.SDL_Init(SDL_INIT_VIDEO) != 0:
        emit("unavailable", reason="SDL_Init: " + sdl.SDL_GetError().decode())
        return UNAVAILABLE_EXIT
    flags = SDL_WINDOW_SHOWN | SDL_WINDOW_INPUT_FOCUS | SDL_WINDOW_MOUSE_FOCUS | SDL_WINDOW_FULLSCREEN_DESKTOP
    win = sdl.SDL_CreateWindow(b"relative probe", SDL_WINDOWPOS_UNDEFINED, SDL_WINDOWPOS_UNDEFINED,
                               640, 480, flags)
    if not win:
        emit("unavailable", reason="SDL_CreateWindow: " + sdl.SDL_GetError().decode())
        return UNAVAILABLE_EXIT
    sdl.SDL_RaiseWindow(win)

    def paint() -> None:
        # A Wayland surface is mapped, and so reachable by the pointer, only once
        # it carries a buffer; X11 needs none, and one plain fill serves both. A
        # surface SDL cannot give is reported, since the window then never maps.
        surface = sdl.SDL_GetWindowSurface(win)
        if surface:
            sdl.SDL_FillRect(surface, None, 0)
            sdl.SDL_UpdateWindowSurface(win)
        elif not paint.failed:
            paint.failed = True
            emit("paint_failed", err=sdl.SDL_GetError().decode())
    paint.failed = False
    paint()
    painted = time.time()
    rc = sdl.SDL_SetRelativeMouseMode(1)
    emit("relative_mode", rc=rc, err=sdl.SDL_GetError().decode() if rc else "", **relative_mode(sdl))
    ev = ctypes.create_string_buffer(64)
    deadline = time.time() + duration
    while time.time() < deadline:
        while sdl.SDL_PollEvent(ev):
            etype = struct.unpack_from("I", ev, 0)[0]
            if etype == SDL_MOUSEMOTION:
                _t, _ts, _w, which, state, x, y, xrel, yrel = struct.unpack_from("IIIIIiiii", ev, 0)
                emit("motion", which=which, state=state, x=x, y=y, dx=xrel, dy=yrel)
            elif etype in (SDL_MOUSEBUTTONDOWN, SDL_MOUSEBUTTONUP):
                _t, _ts, _w, _which, button, _state, _clicks, _p, x, y = struct.unpack_from("IIIIBBBBii", ev, 0)
                emit("button", button=button, down=etype == SDL_MOUSEBUTTONDOWN, x=x, y=y)
            elif etype in (SDL_KEYDOWN, SDL_KEYUP):
                _t, _ts, _w, _state, repeat, _p1, _p2, scancode, sym, mod = struct.unpack_from("IIIBBBBiiH", ev, 0)
                emit("key", down=etype == SDL_KEYDOWN, repeat=repeat, scancode=scancode, sym=sym, mod=mod)
            elif etype == SDL_MOUSEWHEEL:
                _t, _ts, _w, _which, x, y = struct.unpack_from("IIIIii", ev, 0)
                emit("wheel", dx=x, dy=y)
            elif etype == SDL_WINDOWEVENT:
                _t, _ts, _w, event, _p1, _p2, _p3, d1, d2 = struct.unpack_from("IIIBBBBii", ev, 0)
                emit("window", event=WINDOW_EVENTS.get(event, event), d1=d1, d2=d2)
                if event == SDL_WINDOWEVENT_FOCUS_GAINED:
                    # A relative mode asked for before the window had focus is
                    # applied by SDL when focus arrives; the state is reported
                    # again so the driver sees it take.
                    emit("relative_mode", rc=0, err="", **relative_mode(sdl))
            elif etype == SDL_QUIT:
                emit("quit")
                return 0
        if time.time() - painted > 0.25:
            paint()
            painted = time.time()
        time.sleep(0.002)
    sdl.SDL_DestroyWindow(win)
    sdl.SDL_Quit()
    return 0


if __name__ == "__main__":
    sys.exit(main())
