#!/usr/bin/env python3
"""The per-display setting lists stay in their intended relationship.

Four copies exist: one per streaming core, and one per dashboard. They are not
identical on purpose -- JPEG quality has no meaning over WebRTC, and the WS
core alone carries the jpeg-stock paint-over pair -- but each dashboard has to
hold the union, because a dashboard drives both transports and a key missing
from its list is written to the primary's storage key instead of the display's
own. That is a silent failure: the control appears to work and changes the
wrong display.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDONS = os.path.join(ROOT, "addons")

SOURCES = {
    "ws-core": "selkies-web-core/selkies-ws-core.js",
    "wr-core": "selkies-web-core/selkies-wr-core.js",
    "dashboard": "selkies-dashboard/src/components/Sidebar.jsx",
    "wish": "selkies-dashboard-wish/src/utils.ts",
}

# Keys that belong to one transport only: the jpeg-stock controls ride the
# websockets framing jpeg consumes. One encoder knob drives both transports,
# so neither core may drop it.
WS_ONLY = {"jpeg_quality", "paint_over_jpeg_quality"}
WR_ONLY = set()


def read_list(rel):
    """The PER_DISPLAY_SETTINGS keys declared in `rel`, or None if absent."""
    path = os.path.join(ADDONS, rel)
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    match = re.search(r"PER_DISPLAY_SETTINGS\s*(?::[^=]*)?=\s*\[(.*?)\]", text, re.S)
    if not match:
        return None
    return set(re.findall(r"'([^']+)'", match.group(1)))


def main() -> bool:
    failed = 0

    def check(name: str, ok, detail="") -> None:
        nonlocal failed
        if not ok:
            failed += 1
        print(("PASS" if ok else "FAIL") + "  [per-display-settings] {}  {}".format(name, detail),
              flush=True)

    lists = {}
    for name, rel in SOURCES.items():
        keys = read_list(rel)
        check("{} declares the list".format(name), keys is not None, rel)
        if keys is None:
            return False
        lists[name] = keys

    union = lists["ws-core"] | lists["wr-core"]
    for dash in ("dashboard", "wish"):
        check("{} holds the union of both cores".format(dash), lists[dash] == union,
              "missing={} extra={}".format(sorted(union - lists[dash]),
                                           sorted(lists[dash] - union)))

    check("WebSocket-only keys are absent from the WebRTC core",
          not (WS_ONLY & lists["wr-core"]), sorted(WS_ONLY & lists["wr-core"]))
    check("WebRTC-only keys are absent from the WebSocket core",
          not (WR_ONLY & lists["ws-core"]), sorted(WR_ONLY & lists["ws-core"]))
    check("the cores agree on everything else",
          (lists["ws-core"] - WS_ONLY) == (lists["wr-core"] - WR_ONLY),
          "ws_extra={} wr_extra={}".format(
              sorted((lists["ws-core"] - WS_ONLY) - (lists["wr-core"] - WR_ONLY)),
              sorted((lists["wr-core"] - WR_ONLY) - (lists["ws-core"] - WS_ONLY))))

    print("[per-display-settings] {} check(s) failed".format(failed), flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
