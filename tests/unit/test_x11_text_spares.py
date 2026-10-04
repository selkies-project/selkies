#!/usr/bin/env python3
"""The X11 overlay binds a character to a spare every client types on.

Keysyms the layout lacks bind on demand to spare keycodes. Chromium reads an X
keycode as the evdev key it numbers and runs the arrows, Delete, Copy or Undo
it names as commands, so a character bound there never lands. An Xorg on
xfree86 keycodes leaves many of those numbers unbound (101, 114, 118-123,
129-146), and "한국어 입력" composed on the client landed as "한어" on the GLX
desktop. The shim takes the text spares (F13-F24, IntlRo, IntlYen) first, for
a single bind and a composition's batch alike, recycles them before it takes
any other keycode, and types a longer text in runs that fit them.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

from selkies import input_handler as ih  # noqa: E402

results = []
HANGUL = [0x100D55C, 0x100AD6D, 0x100C5B4, 0x100C785, 0x100B825]  # 한 국 어 입 력


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [x11-text-spares] {label}  {detail}", flush=True)


class FakeXDisplay:
    """A core keymap from 8 to 255 with the keycodes in `bound` mapped."""

    def __init__(self, bound: dict) -> None:
        self.map = {kc: list(bound.get(kc, [])) for kc in range(8, 256)}
        self.display = SimpleNamespace(info=SimpleNamespace(min_keycode=8, max_keycode=255))
        self.changes = []

    def get_keyboard_mapping(self, first: int, count: int) -> list:
        return [(self.map[kc] + [0, 0, 0, 0])[:4] for kc in range(first, first + count)]

    def get_modifier_mapping(self) -> list:
        return [[50, 62], [66, 0], [37, 105], [64, 108], [77, 0], [0, 0], [133, 134], [92, 0]]

    def keysym_to_keycode(self, keysym: int) -> int:
        return next((kc for kc, syms in self.map.items() if keysym in syms), 0)

    def keycode_to_keysym(self, keycode: int, level: int) -> int:
        return (self.map[keycode] + [0, 0, 0, 0])[level]

    def change_keyboard_mapping(self, first: int, rows: list) -> None:
        self.changes.append((first, len(rows)))
        for i, row in enumerate(rows):
            self.map[first + i] = list(row)

    def sync(self) -> None:
        pass

    def flush(self) -> None:
        pass


def keyboard(bound: dict) -> "ih._XTestKeyboard":
    open_xkb, ih.open_xkb_link = ih.open_xkb_link, None
    try:
        return ih._XTestKeyboard(FakeXDisplay(bound))
    finally:
        ih.open_xkb_link = open_xkb


# The xfree86 keycodes the GLX desktop's Xorg uses: Home at 97, Print at 111, Alt_R at 113, Super_R at 116, and
# 101, 114, 118-131 and 133-146 left unbound beside the text spares.
XFREE86 = {kc: [0x61 + (kc % 26)] for kc in range(8, 101)}
XFREE86.update({97: [0xFF50], 111: [0xFF61], 113: [0xFFEA], 116: [0xFFEC], 115: [0xFFEB], 108: [0xFE03],
                102: [0xFF53], 103: [0xFF57], 104: [0xFF54], 105: [0xFF56], 106: [0xFF63], 107: [0xFFFF]})
XFREE86_SPACE = 65
XFREE86[XFREE86_SPACE] = [0x20]
# An evdev keymap binds F13-F24 and the JIS keys, and leaves a few others free.
EVDEV = {kc: [0x61 + (kc % 26)] for kc in range(8, 248)}
for kc in (93, 157):
    EVDEV.pop(kc)


def pool_order() -> None:
    kb = keyboard(XFREE86)
    pool = kb._find_spare_keycodes()
    check("the text spares lead an xfree86 keymap's pool", pool[:13] == [*range(191, 203), 132], pool[:16])
    kb = keyboard(EVDEV)
    pool = kb._find_spare_keycodes()
    check("with the text spares bound, the free keycodes make the pool", pool == [93, 157, *range(248, 256)], pool)


def binds() -> None:
    kb = keyboard(XFREE86)
    kc = kb._overlay_keycode(HANGUL[0])
    check("a single bind takes a text spare", kc == 191, kc)
    kb = keyboard(XFREE86)
    check("a composition's batch binds", kb.prebind(HANGUL))
    placed = [kb._overlay[ks] for ks in HANGUL]
    check("on text spares, not on the unbound arrow, Delete or Copy codes", all(191 <= kc <= 202 for kc in placed),
          placed)
    check("in one request", kb._d.changes == [(191, 5)], kb._d.changes)
    kb = keyboard(XFREE86)
    many = [0x1004E00 + i for i in range(20)]
    check("a batch past the text spares binds", kb.prebind(many))
    placed = sorted(kb._overlay[ks] for ks in many)
    check("filling every text spare before another keycode", {132, *range(191, 203)} <= set(placed), placed)


def recycling() -> None:
    kb = keyboard(XFREE86)
    first = [kb._overlay_keycode(0x100AC00 + i) for i in range(13)]
    check("thirteen syllables fill the text spares", sorted(first) == [132, *range(191, 203)], first)
    kc = kb._overlay_keycode(0x100AC00 + 13)
    check("the next recycles the oldest text spare rather than take a free arrow or Delete code", kc == 191, kc)
    check("a free text spare counts toward a run's room", kb.text_room() == 13, kb.text_room())
    kb._d.changes.clear()
    reused = 0x100AC00 + 1
    check("a batch binds on full text spares", kb.prebind([reused, *HANGUL]))
    placed = [kb._overlay[ks] for ks in HANGUL]
    check("by recycling the oldest of them", all(kc in (132, *range(191, 203)) for kc in placed), placed)
    check("and not the bind a keysym of the batch still needs", kb._overlay.get(reused) == first[1],
          (kb._overlay.get(reused), first[1]))


class FakeXtest:
    """Records each key press as the keysym the server maps its keycode to at that moment."""

    def __init__(self, display: FakeXDisplay) -> None:
        self.display = display
        self.typed = []
        self.codes = set()

    def fake_input(self, display, kind: int, keycode: int) -> None:
        if kind == ih.X.KeyPress:
            self.typed.append(self.display.map[keycode][0])
            self.codes.add(keycode)


def typing() -> None:
    kb = keyboard(XFREE86)
    fake, saved = FakeXtest(kb._d), ih.xtest
    ih.xtest = fake
    try:
        text = "가나다라마바사아자차카타파하 한국어 입력"
        ok = asyncio.run(ih.WebRTCInput._type_text_xtest(SimpleNamespace(keyboard=kb), text))
    finally:
        ih.xtest = saved
    check("a text of more syllables than the text spares hold is typed", ok)
    check("whole", fake.typed == [ord(ch) if ch == " " else 0x1000000 + ord(ch) for ch in text],
          "".join(chr(ks & 0xFFFFFF) for ks in fake.typed))
    check("on text spares and the space key alone", fake.codes <= {132, *range(191, 203), XFREE86_SPACE},
          sorted(fake.codes))


pool_order()
binds()
recycling()
typing()
passed = sum(ok for _, ok in results)
print(f"[x11-text-spares] {passed}/{len(results)} passed")
sys.exit(0 if passed == len(results) else 1)
