#!/usr/bin/env python3
"""What a client is allowed to change, and what it is given back when it is not.

Both transports hand every client-proposed setting to the one sanitizer in
settings.py, so a value is accepted, clamped or refused identically whichever
socket delivered it. The rules that are easy to break silently: a range clamps
into the operator's own bounds while an int clamps into the declared ones; an
enum outside the allowed stops falls back to the server's resolved value rather
than to the first stop, so a stale stored choice cannot land a client on the
cheapest or slowest option; a locked bool keeps the server's value; a missing
value asks for the server's; and unparsable input (JSON infinity included)
resolves to a default instead of raising through the settings path.

Usage: python3 tests/unit/test_client_setting_sanitizer.py
"""
import logging
import os
import sys
import types

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

from selkies.settings import sanitize_client_setting  # noqa: E402

passed = failed = 0
logger = logging.getLogger("client-setting-sanitizer")


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [setting-sanitizer] {label}  {detail}", flush=True)


class Warnings(logging.Handler):
    """The operator-facing warnings one sanitize call emitted."""

    def __init__(self) -> None:
        super().__init__(level=logging.WARNING)
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(record.getMessage())


def server(**values) -> types.SimpleNamespace:
    """A stand-in for the parsed settings: ranges and bools carry their pair."""
    return types.SimpleNamespace(**values)


def sanitize(name, value, source, warnings=None):
    if warnings is None:
        return sanitize_client_setting(name, value, source, logger)
    logger.addHandler(warnings)
    try:
        return sanitize_client_setting(name, value, source, logger)
    finally:
        logger.removeHandler(warnings)


def ranges() -> None:
    src = server(framerate=(30, 120))
    check("a value inside the operator's range is taken", sanitize("framerate", 60, src) == 60)
    warnings = Warnings()
    check("a value above the range is clamped to it",
          sanitize("framerate", 240, src, warnings) == 120)
    check("the operator is told a value was clamped",
          any("clamped" in line and "framerate" in line for line in warnings.lines),
          warnings.lines)
    check("a value below the range is clamped to it", sanitize("framerate", 5, src) == 30)
    check("a fractional value stays fractional", sanitize("framerate", 59.5, src) == 59.5)
    whole = sanitize("framerate", 60.0, src)
    check("an integral value comes back an int, not 60.0",
          whole == 60 and isinstance(whole, int), repr(whole))
    check("no value asks for the built-in default", sanitize("framerate", None, src) == 60)
    check("a range pinned to one value answers with it",
          sanitize("framerate", None, server(framerate=(24, 24))) == 24)


def enums() -> None:
    src = server(rate_control_mode="crf")
    check("an allowed stop is taken", sanitize("rate_control_mode", "cbr", src) == "cbr")
    warnings = Warnings()
    check("a value off the list falls back to the server's own, not the first stop",
          sanitize("rate_control_mode", "vbr", src, warnings) == "crf")
    check("the operator is told the list was missed",
          any("allowed list" in line for line in warnings.lines), warnings.lines)
    check("an operator value outside the list is still agreed with when echoed back",
          sanitize("rate_control_mode", "vbr", server(rate_control_mode="vbr")) == "vbr")
    check("no value asks for the server's own",
          sanitize("rate_control_mode", None, src) == "crf")
    numeric = sanitize("audio_bitrate", 128000, server(audio_bitrate="128000"))
    check("a numeric stop comes back as the string the list holds",
          numeric == "128000" and isinstance(numeric, str), repr(numeric))
    check("an encoder alias resolves to the name the list carries",
          sanitize("encoder", "openh264enc", server(encoder="h264enc")) == "h264enc")


def numbers() -> None:
    src = server(video_min_qp=10, keyframe_interval=3.0)
    check("an int inside the declared bounds is taken", sanitize("video_min_qp", 20, src) == 20)
    check("an int above the declared maximum is clamped", sanitize("video_min_qp", 99, src) == 51)
    check("an int below the declared minimum is clamped", sanitize("video_min_qp", -5, src) == 0)
    check("the operator's own value does not bound the client's",
          sanitize("video_min_qp", 40, src) == 40)
    check("a float keeps its fraction", sanitize("keyframe_interval", 2.5, src) == 2.5)
    check("a float is clamped into its declared bounds",
          sanitize("keyframe_interval", 9999.0, src) == 300.0)
    check("text where a number belongs resolves to the default rather than raising",
          sanitize("video_min_qp", "not a number", src) == 0)
    check("JSON infinity clamps where the type can hold it",
          sanitize("keyframe_interval", 1e999, src) == 300.0)
    check("JSON infinity resolves to the default where it cannot be an int",
          sanitize("video_min_qp", 1e999, src) == 0)


def bools() -> None:
    src = server(enable_binary_clipboard=(True, False))
    check("a client may turn an unlocked bool off",
          sanitize("enable_binary_clipboard", "false", src) is False)
    check("the string form of true is accepted",
          sanitize("enable_binary_clipboard", "1", src) is True)
    check("a real bool is accepted", sanitize("enable_binary_clipboard", True, src) is True)
    warnings = Warnings()
    locked = server(enable_binary_clipboard=(True, True))
    check("a locked bool keeps the server's value",
          sanitize("enable_binary_clipboard", False, locked, warnings) is True)
    check("the operator is told a locked setting was refused",
          any("locked setting" in line for line in warnings.lines), warnings.lines)
    check("no value asks for the server's own",
          sanitize("enable_binary_clipboard", None, src) is True)


def unknown() -> None:
    check("a setting the server does not define is refused outright",
          sanitize("no_such_setting", 1, server()) is None)


def main() -> bool:
    ranges()
    enums()
    numbers()
    bools()
    unknown()
    print(f"\n[setting-sanitizer] {passed} passed, {failed} failed")
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
