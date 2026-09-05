#!/usr/bin/env python3
"""The listen address defaults to loopback and binds every address it names.

A server that listens on every interface by default is reachable from the
network before anyone chose to expose it, so the built-in address is
``localhost``, bound on both loopback families, and ``0.0.0.0`` is what a
container or a documented command passes on purpose. A name is bound on
every address it resolves to, a comma-separated list on all of them, and a
loopback family the host does not carry is skipped rather than failing the
start; a port already taken, or a name that does not resolve, still fails and
leaves nothing bound.
"""
import asyncio
import errno
import logging
import os
import socket
import subprocess
import sys
import urllib.request

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.settings import SETTING_DEFINITIONS, AppSettings  # noqa: E402
from selkies.stream_server import CentralizedStreamServer, _bind_listen_sockets  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [listen-address] {label}  {detail}", flush=True)


class _Capture(logging.Handler):
    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(self.format(record))


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def host_has(family: int, address: str) -> bool:
    """Whether a loopback address of this family can be bound on this host."""
    try:
        with socket.socket(family, socket.SOCK_STREAM) as probe:
            probe.bind((address, 0))
        return True
    except OSError:
        return False


HAS_V6 = host_has(socket.AF_INET6, "::1")


def lan_address() -> str:
    """A non-loopback address of this host, or "" when it has none."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect(("192.0.2.1", 9))
            address = probe.getsockname()[0]
    except OSError:
        return ""
    return "" if address.startswith("127.") else address


def names(socks) -> list:
    return sorted(f"{s.family.name} {s.getsockname()[0]}" for s in socks)


def close_all(socks) -> None:
    for sock in socks:
        sock.close()


async def binder_cases() -> None:
    port = free_port()
    socks = await _bind_listen_sockets("localhost", port)
    got = names(socks)
    close_all(socks)
    check("localhost binds the IPv4 loopback", "AF_INET 127.0.0.1" in got, got)
    check("localhost binds the IPv6 loopback where the host has one",
          ("AF_INET6 ::1" in got) == HAS_V6, got)
    check("localhost binds no wildcard address",
          all(not n.endswith((" 0.0.0.0", " ::")) for n in got), got)

    socks = await _bind_listen_sockets("0.0.0.0", port)
    got = names(socks)
    close_all(socks)
    check("0.0.0.0 binds the IPv4 wildcard alone", got == ["AF_INET 0.0.0.0"], got)

    if HAS_V6:
        socks = await _bind_listen_sockets("::", port)
        v6only = socks[0].getsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY)
        got = names(socks)
        close_all(socks)
        check(":: binds the IPv6 wildcard alone", got == ["AF_INET6 ::"], got)
        check("an IPv6 socket is IPv6-only, so :: and 0.0.0.0 can share a port", v6only == 1, v6only)

        socks = await _bind_listen_sockets("127.0.0.1, ::1", port)
        got = names(socks)
        close_all(socks)
        check("a comma-separated list binds every entry",
              got == ["AF_INET 127.0.0.1", "AF_INET6 ::1"], got)

        socks = await _bind_listen_sockets("0.0.0.0,::", port)
        got = names(socks)
        close_all(socks)
        check("both wildcards bind together", got == ["AF_INET 0.0.0.0", "AF_INET6 ::"], got)

    socks = await _bind_listen_sockets("127.0.0.1,127.0.0.1", port)
    got = names(socks)
    close_all(socks)
    check("a repeated address binds once", got == ["AF_INET 127.0.0.1"], got)

    log = logging.getLogger("stream_server")
    sink = _Capture()
    log.addHandler(sink)
    real_getaddrinfo = socket.getaddrinfo

    def with_unassignable(host, *args, **kwargs):
        infos = real_getaddrinfo(host, *args, **kwargs)
        # A TEST-NET address no interface carries: bind fails with EADDRNOTAVAIL.
        return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("192.0.2.1", port))] + infos

    socket.getaddrinfo = with_unassignable
    try:
        socks = await _bind_listen_sockets("127.0.0.1", port)
        got = names(socks)
        close_all(socks)
        check("an address the host lacks is skipped while another binds",
              got == ["AF_INET 127.0.0.1"], got)
        check("the skipped address is reported",
              any("Not listening on 192.0.2.1:%d" % port in ln for ln in sink.lines), sink.lines[-2:])
        socket.getaddrinfo = lambda host, *a, **k: [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("192.0.2.1", port))]
        try:
            await _bind_listen_sockets("192.0.2.1", port)
            check("nothing bindable is an error", False, "no exception")
        except OSError as exc:
            check("nothing bindable is an error", "192.0.2.1" in str(exc), exc)
    finally:
        socket.getaddrinfo = real_getaddrinfo
        log.removeHandler(sink)

    with socket.socket() as taken:
        taken.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        taken.bind(("127.0.0.1", port))
        taken.listen(1)
        try:
            await _bind_listen_sockets("localhost", port)
            check("a port already taken is an error", False, "no exception")
        except OSError as exc:
            check("a port already taken is an error", exc.errno == errno.EADDRINUSE, exc)
    if HAS_V6:
        check("a failed bind leaves nothing else bound", host_has(socket.AF_INET6, "::1"))

    try:
        await _bind_listen_sockets("selkies-listen-address.invalid", port)
        check("a name that does not resolve is an error", False, "no exception")
    except OSError as exc:
        check("a name that does not resolve is an error", "selkies-listen-address.invalid" in str(exc), exc)


def get_status(url: str) -> str:
    try:
        with urllib.request.urlopen(url, timeout=3) as response:
            return str(response.status)
    except Exception as exc:  # a refusal is a result here
        return type(exc).__name__


async def server_cases() -> None:
    lan = lan_address()
    log = logging.getLogger("stream_server")
    sink = _Capture()
    log.addHandler(sink)
    try:
        for addr, expect in (
            ("", {"127.0.0.1": "200", "::1": "200" if HAS_V6 else "URLError", "lan": "URLError"}),
            ("0.0.0.0", {"127.0.0.1": "200", "::1": "URLError", "lan": "200"}),
        ):
            port = free_port()
            sys.argv = ["selkies", "--enable-basic-auth=false", "--enable-https=false", f"--port={port}"]
            if addr:
                sys.argv.append(f"--addr={addr}")
            settings = AppSettings(SETTING_DEFINITIONS)
            if not addr:
                check("the built-in listen address is localhost", settings.addr == "localhost", settings.addr)
            server = CentralizedStreamServer(settings)
            sink.lines.clear()
            await server.start_server()
            try:
                running = [ln for ln in sink.lines if "Selkies server running on" in ln]
                got = {
                    "127.0.0.1": await asyncio.to_thread(get_status, f"http://127.0.0.1:{port}/api/health"),
                    "::1": await asyncio.to_thread(get_status, f"http://[::1]:{port}/api/health"),
                    "lan": await asyncio.to_thread(get_status, f"http://{lan}:{port}/api/health") if lan else expect["lan"],
                }
            finally:
                await server.stop_server()
            label = addr or "the default"
            check(f"{label}: the start line names every bound address",
                  running and f"{addr or '127.0.0.1'}:{port}" in running[-1]
                  and ((f"[::1]:{port}" in running[-1]) == (HAS_V6 and not addr)), running[-1:])
            check(f"{label}: reachable exactly as documented", got == expect, got)
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                try:
                    probe.bind(("127.0.0.1", port))
                    probe.listen(1)
                    released = True
                except OSError:
                    released = False
            check(f"{label}: the address is free again after stop", released)
    finally:
        log.removeHandler(sink)


def help_case() -> None:
    out = subprocess.run([sys.executable, "-m", "selkies", "--help"], capture_output=True, text=True,
                         cwd=REPO, env={**os.environ, "PYTHONPATH": os.path.join(REPO, "src")}).stdout
    check("--help documents the loopback default and 0.0.0.0",
          "--addr" in out and "loopback" in out and "0.0.0.0" in out, out[:0])


def main() -> int:
    logging.basicConfig(level=logging.INFO)
    asyncio.run(binder_cases())
    asyncio.run(server_cases())
    help_case()
    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
