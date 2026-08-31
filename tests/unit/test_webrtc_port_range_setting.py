#!/usr/bin/env python3
"""The webrtc_port_range setting parser, imported through the real module.

Importing `selkies.rtc` is itself the first assertion: the parser must be
defined after the names its annotations use, or the whole server fails to
start on interpreters that evaluate annotations at definition time.
"""
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [port-range-setting] {label}  {detail}",
          flush=True)


def main() -> int:
    try:
        from selkies.rtc import parse_webrtc_port_range
    except Exception as exc:  # noqa: BLE001 - the import IS the test
        check("selkies.rtc imports cleanly", False, repr(exc))
        print(f"[port-range-setting] {passed} passed, {failed} failed",
              flush=True)
        return 1
    check("selkies.rtc imports cleanly", True)

    check('empty keeps ephemeral', parse_webrtc_port_range("") is None)
    check('whitespace keeps ephemeral', parse_webrtc_port_range("  ") is None)
    check('"50000-50100" parses',
          parse_webrtc_port_range("50000-50100") == (50000, 50100))
    check('"1024-1024" parses',
          parse_webrtc_port_range("1024-1024") == (1024, 1024))
    for bad in ("50000", "a-b", "50000:50100", "50000-", "80-90",
                "50100-50000", "1024-70000"):
        try:
            parse_webrtc_port_range(bad)
            check(f"rejects {bad!r}", False, "no ValueError")
        except ValueError:
            check(f"rejects {bad!r}", True)

    print(f"[port-range-setting] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
