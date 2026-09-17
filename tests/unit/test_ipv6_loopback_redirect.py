#!/usr/bin/env python3
"""Where a page that arrived on the IPv6 loopback is served from.

A browser gathers no ICE host candidates for a page whose origin is `::1`, so a
WebRTC session opened from one negotiates and then carries no media. The server
listens on both loopback families and the browser chooses between them, so the
page moves itself to the IPv4 literal rather than leaving the choice to
resolution order.

What must not move is a reverse proxy's traffic: one that forwards to `[::1]`
carries the site's own name, and redirecting it would send a remote browser to
its own machine. Runs the shipped helper against stub requests.
"""
import os
import socket
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from yarl import URL  # noqa: E402
from selkies.stream_server import _ipv6_loopback_redirect  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ipv6-loopback] {label}  {detail}", flush=True)


class _Transport:
    def __init__(self, family, sockname):
        self._info = {"socket": socket.socket(family, socket.SOCK_STREAM) if family else None,
                      "sockname": sockname}

    def get_extra_info(self, name):
        return self._info.get(name)

    def close(self):
        sock = self._info.get("socket")
        if sock is not None:
            sock.close()


class _Request:
    """Only what the helper reads: the transport, the URL and the headers."""

    def __init__(self, family, sockname, url, headers=None, secure=False):
        self.transport = _Transport(family, sockname)
        self.url = URL(url)
        self.headers = headers or {}
        self.secure = secure
        self.query_string = self.url.query_string


CASES = [
    ("a browser on ::1 asking for localhost moves",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://localhost:8080/", {},
     "http://127.0.0.1:8080/"),
    ("the IPv6 literal moves too",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://[::1]:8080/", {},
     "http://127.0.0.1:8080/"),
    ("the route's own path is used, and the query rides along",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://localhost:8080/sub/?a=1", {},
     "http://127.0.0.1:8080/?a=1"),
    ("a path a caller invents cannot reach the target",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://localhost:8080//evil.example/", {},
     "http://127.0.0.1:8080/"),
    ("a proxy forwarding its own name stays",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://example.com/", {}, None),
    ("a proxy that says so stays",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://localhost:8080/",
     {"X-Forwarded-For": "203.0.113.9"}, None),
    ("a forwarded header of any spelling stays",
     socket.AF_INET6, ("::1", 8080, 0, 0), "http://localhost:8080/",
     {"Forwarded": "for=203.0.113.9"}, None),
    ("a global IPv6 address stays",
     socket.AF_INET6, ("2001:db8::1", 8080, 0, 0), "http://localhost:8080/", {}, None),
    ("an IPv4 connection is already where it should be",
     socket.AF_INET, ("127.0.0.1", 8080), "http://localhost:8080/", {}, None),
    ("a v4-mapped address is an IPv4 connection",
     socket.AF_INET6, ("::ffff:127.0.0.1", 8080, 0, 0), "http://localhost:8080/", {}, None),
    ("a unix socket carries no address family to compare",
     socket.AF_UNIX, "/run/selkies.sock", "http://localhost:8080/", {}, None),
]


def main() -> int:
    for label, family, sockname, url, headers, want in CASES:
        request = _Request(family, sockname, url, headers)
        try:
            got = _ipv6_loopback_redirect(request, "/")
        finally:
            request.transport.close()
        check(label, got == want, got if got != want else "")
    print(f"[ipv6-loopback] {passed}/{passed + failed} passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
