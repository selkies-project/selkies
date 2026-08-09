#!/usr/bin/env python3
"""Re-typed text keeps its case on a keymap that does not bind Shift.

The clipboard re-type and soft-input paths inject each character as a keysym.
Capitals and shifted symbols sit at a shifted level of their keycode, which the
server only selects while a key bound in the MODIFIER MAP is held. A keymap can
name Shift without binding it -- a bare Xvfb is the common case -- and there the
injected Shift does nothing, so every capital arrives lowercase and the paste is
silently corrupted rather than failing.

Runs against its own X server so the modifier map can be emptied, which is the
only way to exercise the broken configuration.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

DISPLAY = os.environ.get("RETYPE_DISPLAY", ":97")
SAMPLE = "Big Chicken A"


def start_server():
    """A bare X server, GLX off because it faults on some GPU hosts."""
    proc = subprocess.Popen(
        ["Xvfb", DISPLAY, "-screen", "0", "1280x720x24", "-extension", "GLX",
         "-nolisten", "tcp", "-ac", "-noreset"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(30):
        if subprocess.run(["xdpyinfo", "-display", DISPLAY],
                          capture_output=True).returncode == 0:
            return proc
        time.sleep(0.5)
    proc.kill()
    raise RuntimeError("test X server did not start")


def typed_text(display, keyboard, text):
    """Inject text and read back what a client actually receives."""
    from selkies.Xlib import X

    screen = display.screen()
    win = screen.root.create_window(
        0, 0, 100, 100, 0, screen.root_depth, window_class=X.InputOutput,
        event_mask=X.KeyPressMask, override_redirect=True)
    win.map()
    display.sync()
    win.set_input_focus(X.RevertToParent, X.CurrentTime)
    display.sync()

    for ch in text:
        cp = ord(ch)
        keyboard.press(cp)
        keyboard.release(cp)
    display.sync()
    time.sleep(0.3)

    out = []
    while display.pending_events():
        e = display.next_event()
        if e.type != X.KeyPress:
            continue
        shifted = bool(e.state & X.ShiftMask)
        # Queried from the server rather than the client-side keymap cache: the
        # overlay rebinds keycodes as it types, and a cached lookup would report
        # the symbol that keycode carried before the rebind.
        syms = display.get_keyboard_mapping(e.detail, 1)[0]
        idx = 1 if (shifted and len(syms) > 1 and syms[1]) else 0
        ks = syms[idx] if len(syms) > idx else 0
        if 0x20 <= ks <= 0xFF:
            out.append(chr(ks))
        elif ks & 0x01000000:
            out.append(chr(ks & 0x00FFFFFF))
    win.destroy()
    return "".join(out)


def main():
    res = H.Results("retype-case")
    server = start_server()
    try:
        from selkies.Xlib import display as xdisplay
        from selkies.input_handler import _XTestKeyboard

        d = xdisplay.Display(DISPLAY)
        try:
            res.check("shift starts bound",
                      any(kc for kc in d.get_modifier_mapping()[0]), "")

            keyboard = _XTestKeyboard(d)
            got = typed_text(d, keyboard, SAMPLE)
            res.check("case survives on a normal keymap", got == SAMPLE,
                      "{!r} want {!r}".format(got, SAMPLE))

            # Strip Shift from the modifier map: the keysym stays, the modifier
            # goes, which is what a keymap-less deployment looks like.
            subprocess.run(["xmodmap", "-display", DISPLAY, "-e", "clear shift"],
                           capture_output=True, timeout=10)
            d.sync()
            res.check("shift is now unbound",
                      not any(kc for kc in d.get_modifier_mapping()[0]), "")

            keyboard = _XTestKeyboard(d)
            # Assert the resolution directly: an end-to-end read cannot tell a
            # working Shift from a level-0 overlay, so it cannot fail when the
            # dependency on Shift comes back.
            kc, mods = keyboard._resolve(ord("B"))
            res.check("capital resolves without needing a modifier", mods == (),
                      "keycode={} mods={}".format(kc, mods))
            syms = d.get_keyboard_mapping(kc, 1)[0]
            res.check("its keycode carries the capital at level 0",
                      bool(syms) and syms[0] == ord("B"),
                      "level0={!r}".format(syms[0] if syms else None))

            got = typed_text(d, keyboard, SAMPLE)
            res.check("case survives without a Shift modifier", got == SAMPLE,
                      "{!r} want {!r}".format(got, SAMPLE))
        finally:
            d.close()
    finally:
        server.kill()
        server.wait(timeout=10)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
