#!/usr/bin/env python3
"""Keysym resolution parity for the Wayland compositor keymap.

Selkies owns keysym->keycode policy: it resolves against the compositor's own keymap
and overlay-binds what the base cannot produce. Correctness is not "matches some other
implementation" but "agrees with xkbcommon about the keymap the compositor actually
delivers" — if Selkies presses keycode C at level L expecting keysym K, a client
looking C/L up in the delivered keymap must see K, or the app receives a different
character than the user typed.

The invariants below are the ones the policy has to keep whatever the layout: one
keymap swap per batch (a swap costs milliseconds on the compositor thread, which also
drives input and rendering), overlay binds that survive a base-layout change, and a
held keycode that is never recycled — its release must mean what its press meant.
"""
import ctypes
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

sys.path.insert(0, os.path.join(H.REPO, "src"))

XKB_KEYMAP_FORMAT_TEXT_V1 = 1


def _xkb() -> ctypes.CDLL:
    """libxkbcommon entry points used as the ground truth for a keymap's contents."""
    lib = ctypes.CDLL("libxkbcommon.so.0")
    lib.xkb_context_new.restype = ctypes.c_void_p
    lib.xkb_context_new.argtypes = [ctypes.c_int]
    lib.xkb_keymap_new_from_string.restype = ctypes.c_void_p
    lib.xkb_keymap_new_from_string.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
    lib.xkb_keymap_key_get_syms_by_level.restype = ctypes.c_int
    lib.xkb_keymap_key_get_syms_by_level.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
        ctypes.POINTER(ctypes.POINTER(ctypes.c_uint32))]
    return lib


def syms_at(lib: ctypes.CDLL, keymap, keycode: int, level: int) -> list[int]:
    """The keysyms the keymap binds at `keycode`/`level` (layout 0)."""
    out = ctypes.POINTER(ctypes.c_uint32)()
    n = lib.xkb_keymap_key_get_syms_by_level(
        keymap, keycode, 0, level, ctypes.byref(out))
    return [out[i] for i in range(n)]


class Recorder:
    """Capture handle wrapper that counts keymap swaps however they are delivered.

    A swap reaches the compositor either as a whole keymap text or as a set of binds
    spliced onto the base it already holds; both cost one swap, so both are counted.
    Resolution is always checked against the compositor's LIVE keymap rather than
    whatever text passed through here, so the assertions do not care which API ran."""

    def __init__(self, inner) -> None:
        self._inner = inner
        self.swaps: int = 0

    def set_keymap_string(self, text: str):
        self.swaps += 1
        return self._inner.set_keymap_string(text)

    def set_keymap_overlay(self, binds):
        """Count and forward an overlay splice.

        Fire-and-forget, like set_keymap_string: the call IS the swap, and
        nothing is awaited, so there is no return value to key off.
        """
        self.swaps += 1
        return self._inner.set_keymap_overlay(binds)

    def __getattr__(self, name):
        return getattr(self._inner, name)


def live_keymap(lib: ctypes.CDLL, capture):
    """Compile the keymap the compositor is currently delivering to clients."""
    text = capture.get_xkb_keymap_string()
    return lib.xkb_keymap_new_from_string(
        lib.xkb_context_new(0), (text or "").encode(), XKB_KEYMAP_FORMAT_TEXT_V1, 0)


def main() -> "H.Results":
    from pixelflux import ScreenCapture, ensure_wayland_display
    from selkies.input_handler import _WaylandKeymapOwner

    res = H.Results("keymap-parity")
    lib = _xkb()
    ensure_wayland_display(width=1280, height=720, render_node="",
                           auto_gpu="", cursor_size=24)
    capture = ScreenCapture()
    base = capture.get_xkb_keymap_string()
    res.check("compositor delivers a keymap", bool(base), f"{len(base or '')} bytes")
    if not base:
        res.summary()
        return res

    handle = Recorder(capture)
    owner = _WaylandKeymapOwner(handle, base)
    keymap = lib.xkb_keymap_new_from_string(
        lib.xkb_context_new(0), base.encode(), XKB_KEYMAP_FORMAT_TEXT_V1, 0)
    res.check("delivered keymap compiles", bool(keymap))

    checked = mismatched = 0
    examples = []
    for keysym in list(range(0x20, 0x7F)) + list(range(0xA0, 0x100)):
        resolved = owner._map.get(keysym)
        if resolved is None:
            continue
        keycode, level = resolved
        checked += 1
        if keysym not in syms_at(lib, keymap, keycode, level):
            mismatched += 1
            if len(examples) < 4:
                examples.append((hex(keysym), keycode, level))
    res.check(f"base keysyms resolve as the keymap says ({checked} checked)",
              checked > 40 and mismatched == 0, f"mismatched={mismatched} {examples}")

    handle2 = Recorder(capture)
    owner2 = _WaylandKeymapOwner(handle2, base)
    burst = [0x01000000 | (0x1F600 + i) for i in range(32)]
    bound = owner2._overlay_bind_many(burst)
    res.check("batch of 32 costs one keymap swap", handle2.swaps == 1,
              f"swaps={handle2.swaps}")
    res.check("batch keycodes are distinct",
              len(set(bound.values())) == len(burst), str(sorted(bound.values())[:6]))

    overlay_map = live_keymap(lib, capture)
    res.check("live keymap after the batch compiles", bool(overlay_map))
    unresolved = [hex(k) for k in burst
                  if not overlay_map or k not in syms_at(lib, overlay_map, bound[k], 0)]
    res.check("every overlay keysym resolves at its assigned keycode",
              not unresolved, f"unresolved={unresolved[:4]}")

    before = handle2.swaps
    owner2._overlay_bind_many(burst)
    res.check("re-binding bound keysyms does not swap",
              handle2.swaps == before, f"swaps={handle2.swaps - before}")

    # Fill every overlay slot first, so the held keycode is under pressure.
    handle3 = Recorder(capture)
    owner3 = _WaylandKeymapOwner(handle3, base)
    slots = len(owner3._overlay_codes)
    first = [0x01000000 | (0x2000 + i) for i in range(slots)]
    assigned = owner3._overlay_bind_many(first)
    held_sym = first[0]
    held_kc = assigned[held_sym]
    # Held by hand so the owner treats the keycode as in use.
    owner3._pressed[held_sym] = (held_kc, ())
    later = owner3._overlay_bind_many([0x01000000 | (0x3000 + i) for i in range(8)])
    res.check("held keycode is not handed to a new keysym",
              held_kc not in later.values(), f"held={held_kc} new={sorted(later.values())}")
    res.check("held keysym keeps its keycode",
              owner3._overlay.get(held_sym) == held_kc,
              f"{owner3._overlay.get(held_sym)} vs {held_kc}")

    # Typing text needs no compositor-side batch API.
    handle4 = Recorder(capture)
    owner4 = _WaylandKeymapOwner(handle4, base)
    typed = owner4.type_text("hi \U0001F600\U0001F601")
    res.check("type_text succeeds through the local overlay", typed is True)
    res.check("type_text swaps at most once", handle4.swaps <= 1,
              f"swaps={handle4.swaps}")

    # Each entry names an X11 keysym block the layout must actually deliver, so a
    # keymap that silently lost its script fails instead of passing vacuously.
    SCRIPTS = {
        "Cyrillic": (0x6A0, 0x6FF), "Greek": (0x7A0, 0x7FF),
        "Arabic": (0x5A0, 0x5FF), "Hebrew": (0xCA0, 0xCFF),
        "Thai": (0xDA0, 0xDFF), "Devanagari": (0x1000900, 0x100097F),
        "Kana": (0x4A0, 0x4DF), "Latin-1": (0xA0, 0xFF),
    }
    LAYOUTS = [
        ("de", "", "Latin-1"), ("fr", "", "Latin-1"), ("ru", "", "Cyrillic"),
        ("gr", "", "Greek"), ("il", "", "Hebrew"), ("ara", "", "Arabic"),
        ("th", "", "Thai"), ("in", "", "Devanagari"), ("jp", "kana", "Kana"),
        # CJK proper is IME-driven and carries no base keysyms; covered by the
        # Unicode overlay path below.
        ("kr", "", None), ("cn", "", None),
    ]
    build_ms = []
    for layout, variant, script in LAYOUTS:
        if not capture.set_xkb_layout(layout, variant):
            res.check(f"layout {layout} compiles", False, "set_xkb_layout refused")
            continue
        time.sleep(0.15)
        text = capture.get_xkb_keymap_string()
        started = time.perf_counter()
        lo = _WaylandKeymapOwner(Recorder(capture), text)
        build_ms.append((time.perf_counter() - started) * 1000)
        lkm = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), text.encode(), XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        wrong = sum(1 for sym, (kc, lv) in lo._map.items()
                    if sym not in syms_at(lib, lkm, kc, lv))
        detail = f"{len(lo._map)} keysyms"
        if script:
            first, last = SCRIPTS[script]
            n = sum(1 for s in lo._map if first <= s <= last)
            res.check(f"layout {layout} delivers {script}", n > 0, f"{n} keysyms")
            detail += f", {script}={n}"
        res.check(f"layout {layout} resolves as its keymap says", wrong == 0,
                  f"{detail}, wrong={wrong}")
    capture.set_xkb_layout("us", "")
    time.sleep(0.15)

    # CJK and emoji arrive as Unicode codepoints, so they take the overlay.
    handle5 = Recorder(capture)
    owner5 = _WaylandKeymapOwner(handle5, capture.get_xkb_keymap_string())
    cjk = "漢字テスト한글ひらがな\U0001F600"
    cjk_syms = [0x01000000 | ord(c) for c in cjk]
    cjk_bound = owner5._overlay_bind_many(cjk_syms)
    ckm = live_keymap(lib, capture)
    missed = [c for c, s in zip(cjk, cjk_syms)
              if not cjk_bound.get(s) or not ckm
              or s not in syms_at(lib, ckm, cjk_bound[s], 0)]
    res.check(f"CJK/emoji resolve through the overlay ({len(cjk)} chars)",
              not missed, f"missed={missed}")
    res.check("CJK batch costs one keymap swap", handle5.swaps == 1,
              f"swaps={handle5.swaps}")

    # Layouts like fr/de put accents behind dead keys, but the browser sends the
    # composed character, not the sequence. Selkies must therefore reach it directly
    # — from the base when the layout has it, else through the overlay — and never
    # need a two-keystroke dead-key sequence. Dead-key keysyms themselves must still
    # resolve, since a client may legitimately be sent one.
    DEAD_FIRST, DEAD_LAST = 0xFE50, 0xFE93
    for layout, accented in (("fr", "âéèçùî"), ("de", "äöüßÄ"), ("gr", "άέή")):
        if not capture.set_xkb_layout(layout, ""):
            continue
        time.sleep(0.15)
        handle6 = Recorder(capture)
        owner6 = _WaylandKeymapOwner(handle6, capture.get_xkb_keymap_string())
        dkm = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), owner6._base_text.encode(),
            XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        dead = {s: kc_lv for s, kc_lv in owner6._map.items()
                if DEAD_FIRST <= s <= DEAD_LAST}
        wrong_dead = sum(1 for s, (kc, lv) in dead.items()
                         if s not in syms_at(lib, dkm, kc, lv))
        res.check(f"{layout}: dead keysyms resolve as the keymap says",
                  wrong_dead == 0, f"{len(dead)} dead keys, wrong={wrong_dead}")

        # Every accented char reachable in one tap, base or overlay.
        syms = [ord(c) if ord(c) in owner6._map else (0x01000000 | ord(c))
                for c in accented]
        need = [s for s in syms if s not in owner6._map]
        placed = owner6._overlay_bind_many(need) if need else {}
        unreachable = [c for c, s in zip(accented, syms)
                       if s not in owner6._map and not placed.get(s)]
        res.check(f"{layout}: accented chars reachable in one keystroke ({accented})",
                  not unreachable, f"unreachable={unreachable}")
        res.check(f"{layout}: those chars cost at most one keymap swap",
                  handle6.swaps <= 1, f"swaps={handle6.swaps}")
    capture.set_xkb_layout("us", "")
    time.sleep(0.15)

    # Compose sequences are a client-side input method; Selkies bypasses them by
    # sending the composed character straight through, so what matters is that an
    # option-carrying keymap still resolves AND that _overlay_text's string surgery
    # (it splices keycodes and symbols into the text by index) survives the extra
    # sections those options introduce.
    for layout, variant, options in (
        ("us", "", "compose:ralt"),
        ("us", "", "caps:escape,compose:menu"),
        ("us", "intl", "compose:ralt"),
        ("de", "", "grp:alt_shift_toggle,compose:rctrl"),
        ("us", "", "lv3:ralt_switch,compose:rwin"),
    ):
        if not capture.set_xkb_layout(layout, variant, options):
            res.check(f"options [{options}] compile", False, "set_xkb_layout refused")
            continue
        time.sleep(0.15)
        owner7 = _WaylandKeymapOwner(Recorder(capture), capture.get_xkb_keymap_string())
        okm = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), owner7._base_text.encode(),
            XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        wrong = sum(1 for sym, (kc, lv) in owner7._map.items()
                    if sym not in syms_at(lib, okm, kc, lv))
        owner7._overlay[0x1004E2D] = owner7._overlay_codes[0]
        owner7._overlay_order.append(0x1004E2D)
        # Local assembly is the fallback path when the compositor cannot splice.
        spliced = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), owner7._overlay_text().encode(),
            XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        res.check(f"{layout}({variant or '-'}) [{options}] resolves and splices",
                  wrong == 0 and bool(spliced),
                  f"{len(owner7._map)} keysyms, wrong={wrong}, splice={bool(spliced)}")
    capture.set_xkb_layout("us", "")
    time.sleep(0.15)

    # An X11 keycode is a byte, so anything above 255 reaches Wayland apps only.
    # Emoji and CJK have to land below that ceiling, and the keycodes borrowed to
    # get them there must not be ones the session still needs.
    LOAD_BEARING = {
        # Alt/Meta/Super.
        0xFFE9, 0xFFEA, 0xFFE7, 0xFFE8, 0xFFEB, 0xFFEC,
        # Shift/Ctrl/AltGr/Level5.
        0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFE03, 0xFE11,
        # Print, Cancel, Redo.
        0xFF61, 0xFF69, 0xFF66,
    }
    for layout, variant in (("us", ""), ("de", ""), ("ru", ""), ("jp", "kana")):
        if not capture.set_xkb_layout(layout, variant):
            continue
        time.sleep(0.15)
        text = capture.get_xkb_keymap_string()
        owner8 = _WaylandKeymapOwner(Recorder(capture), text)
        pool = owner8._overlay_codes
        sub256 = [kc for kc in pool if kc < 256]
        res.check(f"{layout}: overlay pool has XWayland-reachable room",
                  len(sub256) >= 32, f"{len(sub256)} of {len(pool)} below 256")

        # Nothing the session depends on may be borrowed.
        bkm = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), text.encode(), XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        clashes = []
        for kc in sub256:
            for level in range(4):
                if any(s in LOAD_BEARING for s in syms_at(lib, bkm, kc, level)):
                    clashes.append(kc)
                    break
        res.check(f"{layout}: pool borrows nothing load-bearing", not clashes,
                  f"clashes={clashes[:6]}")

        mixed = "漢字한글ひらがな\U0001F600\U0001F389"
        syms = [0x01000000 | ord(c) for c in mixed]
        placed = owner8._overlay_bind_many(syms)
        above = [c for c, sym in zip(mixed, syms) if placed.get(sym, 0) >= 256]
        res.check(f"{layout}: CJK/emoji land below the X11 ceiling ({mixed})",
                  not above, f"above 255: {above}")
        lkm = live_keymap(lib, capture)
        missed = [c for c, sym in zip(mixed, syms)
                  if not lkm or sym not in syms_at(lib, lkm, placed[sym], 0)]
        res.check(f"{layout}: and still resolve there", not missed, f"missed={missed}")
    capture.set_xkb_layout("us", "")
    time.sleep(0.15)

    # A pc105 keymap declares a maximum well above 255 and binds a couple of hundred
    # keycodes up there. The locally-assembled keymap (used when the compositor has
    # no splice API) rewrites the maximum line, and lowering it would silently drop
    # every one of those keys.
    handle9 = Recorder(capture)
    owner9 = _WaylandKeymapOwner(handle9, capture.get_xkb_keymap_string())
    owner9._overlay_bind_many([0x01000000 | (0x1F600 + i) for i in range(8)])
    assembled = owner9._overlay_text()

    def declared_max(text: str) -> int:
        """The keycode ceiling a keymap text declares."""
        at = text.index("maximum = ") + len("maximum = ")
        return int(text[at:text.index(";", at)].strip())

    def bound_above_255(text: str) -> int:
        """Count keycode/level bindings above 255, or -1 if the text fails to compile."""
        km = lib.xkb_keymap_new_from_string(
            lib.xkb_context_new(0), text.encode(), XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        if not km:
            return -1
        return sum(1 for kc in range(256, declared_max(text) + 1)
                   for lvl in range(4) if syms_at(lib, km, kc, lvl))

    base_max, made_max = declared_max(owner9._base_text), declared_max(assembled)
    res.check("assembled keymap keeps the base's keycode ceiling",
              made_max >= base_max, f"base={base_max} assembled={made_max}")
    kept, had = bound_above_255(assembled), bound_above_255(owner9._base_text)
    res.check("assembled keymap keeps the keys above 255",
              kept >= had > 0, f"base had {had}, assembled has {kept}")

    # The per-keystroke path must stay far below the cost of a swap.
    keycode, _ = owner5._map[ord("a")]
    started = time.perf_counter()
    for _ in range(20000):
        owner5._map.get(ord("a"))
    resolve_us = (time.perf_counter() - started) / 20000 * 1e6
    res.check("keysym resolution stays under 5 us", resolve_us < 5.0,
              f"{resolve_us:.2f} us")
    res.check("building the reverse map stays under 25 ms",
              max(build_ms) < 25.0 if build_ms else True,
              f"max {max(build_ms):.1f} ms over {len(build_ms)} layouts")

    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
