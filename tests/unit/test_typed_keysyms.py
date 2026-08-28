#!/usr/bin/env python3
"""Which keysym spells a typed character.

Latin-1 keeps its own keysym. The Unicode-plane spelling of the same character
(0x010000E9 for e-acute) looks more uniform and xkbcommon reads it back, but an
X client that looks keys up through an input context gets the bare Latin-1 byte
for it, which a UTF-8 client drops -- so XWayland apps lose exactly the accented
characters. Codepoints above Latin-1 have no legacy keysym every toolkit agrees
on and ride the Unicode plane. Controls type nothing beyond the Return and Tab
bindings. tests/integration/test_xwayland_typed_text.py measures the same policy
end to end against a nested compositor; these checks are the policy itself.
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

from selkies import input_handler as IH  # noqa: E402

res = H.Results("typed-keysyms")

latin1 = [chr(c) for c in list(range(0x20, 0x7F)) + list(range(0xA0, 0x100))]
wrong = [c for c in latin1 if IH.universal_text_keysym(c) != ord(c)]
res.check("Latin-1 types as its own keysym", not wrong, wrong[:8])

above = {0x100: 0x01000100, 0x3A9: 0x010003A9, 0x4E2D: 0x01004E2D, 0x1F600: 0x0101F600}
res.check("codepoints above Latin-1 ride the Unicode plane",
          all(IH.universal_text_keysym(chr(cp)) == ks for cp, ks in above.items()),
          {hex(cp): hex(IH.universal_text_keysym(chr(cp))) for cp in above})

res.check("Return and Tab are the only control bindings",
          IH.universal_text_keysym("\n") == 0xFF0D
          and IH.universal_text_keysym("\r") == 0xFF0D
          and IH.universal_text_keysym("\t") == 0xFF09)

untypeable = [chr(c) for c in list(range(0x00, 0x09)) + list(range(0x0B, 0x0D))
              + list(range(0x0E, 0x20)) + list(range(0x7F, 0xA0))]
typed = [c for c in untypeable if IH.universal_text_keysym(c) is not None]
res.check("controls and C1 have no keysym", not typed, [hex(ord(c)) for c in typed[:8]])

sample = "aé ü ñ £ ¿ ÿ Ω ф 中 \U0001F600 Z9\t\n"
keysyms = IH.text_to_wayland_keysyms(sample)
res.check("the typer is handed one keysym per typeable character",
          len(keysyms) == len([c for c in sample if IH.universal_text_keysym(c)]),
          f"{len(keysyms)} keysyms for {len(sample)} characters")
res.check("nothing typeable is dropped from a mixed string",
          keysyms == [ks for ks in map(IH.universal_text_keysym, sample) if ks is not None])
res.check("a string of nothing but controls types nothing",
          IH.text_to_wayland_keysyms("\x00\x7f\x80\x9f\x1b") == [],
          IH.text_to_wayland_keysyms("\x00\x7f\x80\x9f\x1b"))

# The round trip the receiving side does: every keysym has to decode back to the
# character it was chosen for, or the text arrives altered rather than missing.
printable = [c for c in sample if c not in "\t\n"]
back = {c: IH.keysym_to_character(IH.universal_text_keysym(c)) for c in printable}
res.check("every keysym decodes back to its character",
          all(v == k for k, v in back.items()), {k: v for k, v in back.items() if v != k})

if IH.libxkb is None:
    res.skip("xkbcommon decodes every keysym the same way", "libxkbcommon absent")
else:
    import ctypes
    def xkb_char(keysym: int) -> str:
        buf = ctypes.create_string_buffer(8)
        n = IH.libxkb.xkb_keysym_to_utf8(keysym, buf, 8)
        return buf.value.decode("utf-8") if n > 0 else ""
    xkb = {c: xkb_char(IH.universal_text_keysym(c)) for c in printable}
    res.check("xkbcommon decodes every keysym the same way",
              all(v == k for k, v in xkb.items()), {k: v for k, v in xkb.items() if v != k})

sys.exit(0 if res.summary() else 1)
