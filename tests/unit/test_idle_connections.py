#!/usr/bin/env python3
"""A connection that carries no request is closed; one carrying a request is not.

Every open connection holds a file descriptor, so a peer that connects and
sends nothing, stops inside its request headers, or leaves a kept-alive
connection idle after a response must not keep one for long: the server closes
each after `IDLE_CONNECTION_TIMEOUT_SECONDS`. A request in progress is not
idle, so an upload whose body trickles in for longer than that still lands.
The suite shortens the timeout so it runs in seconds; the server reads it at
start. aiohttp before 3.14 (3.13.5, the floor on Python 3.9) arms its keepalive
timer only after a response, so a connection that has not finished a request
stays open there: those two checks are skipped on it, saying so.
"""
import asyncio
import importlib.metadata
import os
import socket
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]

from selkies import stream_server  # noqa: E402
from selkies.settings import SETTING_DEFINITIONS, AppSettings  # noqa: E402

TIMEOUT = 1.5
passed = failed = 0
AIOHTTP = importlib.metadata.version("aiohttp")
# Whether aiohttp closes a connection idle before its first request completes.
CLOSES_BEFORE_A_REQUEST = tuple(int(p) for p in AIOHTTP.split(".")[:2]) >= (3, 14)


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [idle-connections] {label}  {detail}", flush=True)


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def closed_within(sock: socket.socket, seconds: float) -> bool:
    """Whether the server closes `sock` within `seconds`."""
    sock.settimeout(seconds)
    try:
        while sock.recv(4096):
            pass
        return True
    except socket.timeout:
        return False
    except OSError:
        return True


def idle_cases(port: int) -> None:
    for label, sent in (("a connection that sends nothing", b""),
                        ("a connection stopped inside its request headers", b"GET /api/health HTTP/1.1\r\nHost: x\r\n"),
                        ("a kept-alive connection idle after a response", b"GET /api/health HTTP/1.1\r\nHost: x\r\n\r\n")):
        sock = socket.create_connection(("127.0.0.1", port))
        try:
            if sent:
                sock.sendall(sent)
            if sent.endswith(b"\r\n\r\n"):
                sock.settimeout(5)
                sock.recv(4096)
            check(f"{label} stays open before the timeout", not closed_within(sock, TIMEOUT / 3))
            if not sent.endswith(b"\r\n\r\n") and not CLOSES_BEFORE_A_REQUEST:
                print(f"SKIP  [idle-connections] {label} is closed after it: aiohttp {AIOHTTP} "
                      "arms its keepalive timer only after a response", flush=True)
                continue
            started = time.monotonic()
            check(f"{label} is closed after it", closed_within(sock, TIMEOUT + 3),
                  f"{time.monotonic() - started:.1f}s")
        finally:
            sock.close()


def slow_upload_case(port: int, root: str) -> None:
    body = b"x" * 8
    sock = socket.create_connection(("127.0.0.1", port))
    reply, took = b"", 0.0
    try:
        sock.sendall(b"POST /api/upload HTTP/1.1\r\nHost: x\r\nX-Upload-Path: slow.bin\r\n"
                     b"Content-Type: application/octet-stream\r\n"
                     + f"Content-Length: {len(body)}\r\n\r\n".encode())
        started = time.monotonic()
        for byte in body:
            time.sleep(TIMEOUT * 2 / len(body))
            sock.sendall(bytes([byte]))
        sock.settimeout(5)
        reply = sock.recv(4096)
        took = time.monotonic() - started
    except OSError as exc:
        reply = repr(exc).encode()
    finally:
        sock.close()
    landed = os.path.join(root, "slow.bin")
    check("an upload whose body takes longer than the timeout still lands",
          reply.startswith(b"HTTP/1.1 200") and os.path.exists(landed) and took > TIMEOUT,
          f"{took:.1f}s {reply[:12]!r}")


async def main_async() -> None:
    stream_server.IDLE_CONNECTION_TIMEOUT_SECONDS = TIMEOUT
    root = tempfile.mkdtemp(prefix="selkies-idle-")
    port = free_port()
    sys.argv = ["selkies", "--enable-basic-auth=false", "--enable-https=false", "--addr=127.0.0.1",
                f"--port={port}", f"--file-manager-path={root}", "--file-transfers=upload,download"]
    server = stream_server.CentralizedStreamServer(AppSettings(SETTING_DEFINITIONS))
    await server.start_server()
    try:
        await asyncio.to_thread(idle_cases, port)
        await asyncio.to_thread(slow_upload_case, port, root)
    finally:
        await server.stop_server()


def main() -> int:
    asyncio.run(main_async())
    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
