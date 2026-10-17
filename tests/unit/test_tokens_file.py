#!/usr/bin/env python3
"""The tokens file provisions the session token table without a POST.

A table in `tokens_file` is provisioned as the watcher starts and opens the
config gate; a file renamed over it replaces the table; a file another user can
write to is refused until a chmod makes it safe; a file that is no table leaves
the table as it was; a symlink is followed to the file it names; and without a
master token the file is ignored.
"""
import asyncio
import json
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from selkies import sessions  # noqa: E402
from selkies import websockets_mode  # noqa: E402
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [tokens-file] {label}  {detail}", flush=True)


def write(path: str, body, mode: int = 0o600) -> None:
    """Put `body` at `path` in one rename, with `mode` already set."""
    staged = path + ".new"
    with open(staged, "w") as handle:
        handle.write(body if isinstance(body, str) else json.dumps(body))
    os.chmod(staged, mode)
    os.replace(staged, path)


async def settle(predicate, timeout: float = 5.0) -> bool:
    """Wait for `predicate` to hold, polling faster than the watcher does."""
    deadline = asyncio.get_running_loop().time() + timeout
    while not predicate():
        if asyncio.get_running_loop().time() > deadline:
            return False
        await asyncio.sleep(0.02)
    return True


def tokens() -> set:
    return set(sessions.user_tokens)


def server() -> DataStreamingServer:
    ws = DataStreamingServer.__new__(DataStreamingServer)
    ws.config_gate = asyncio.Event()
    return ws


async def main() -> None:
    websockets_mode.TOKENS_FILE_POLL_S = 0.05
    settings = websockets_mode.settings
    saved = settings.tokens_file, settings.master_token
    root = tempfile.mkdtemp(prefix="selkies-tokens-file-")
    path = os.path.join(root, "tokens.json")
    sessions.user_tokens = {}
    settings.tokens_file, settings.master_token = path, "unit-master-token"
    try:
        write(path, {"ctrl": {"role": "controller", "slot": None}})
        ws = server()
        watcher = asyncio.create_task(ws.watch_tokens_file())
        try:
            check("the file's table is provisioned without a POST", await settle(lambda: tokens() == {"ctrl"}), tokens())
            check("the first table opens the config gate", ws.config_gate.is_set())

            write(path, {"view": {"role": "viewer", "slot": None}})
            check("a file renamed over it replaces the table", await settle(lambda: tokens() == {"view"}), tokens())

            write(path, {"other": {"role": "controller", "slot": None}}, mode=0o620)
            await asyncio.sleep(0.3)
            check("a file another user can write to is refused", tokens() == {"view"}, tokens())
            os.chmod(path, 0o600)
            check("a chmod that makes it safe provisions it", await settle(lambda: tokens() == {"other"}), tokens())

            write(path, "{not json")
            await asyncio.sleep(0.3)
            check("a file that is not JSON leaves the table as it was", tokens() == {"other"}, tokens())
            write(path, {"bad": "controller"})
            await asyncio.sleep(0.3)
            check("a file whose entries are not objects leaves it too", tokens() == {"other"}, tokens())

            target = os.path.join(root, "mounted.json")
            write(target, {"linked": {"role": "controller", "slot": None}})
            os.symlink(target, path + ".link")
            os.replace(path + ".link", path)
            check("a symlink is followed to the file it names", await settle(lambda: tokens() == {"linked"}), tokens())
        finally:
            watcher.cancel()
            try:
                await watcher
            except asyncio.CancelledError:
                pass

        sessions.user_tokens = {}
        settings.master_token = ""
        ws = server()
        await asyncio.wait_for(ws.watch_tokens_file(), timeout=1.0)
        check("without a master token the file is ignored", not tokens() and not ws.config_gate.is_set(), tokens())
    finally:
        settings.tokens_file, settings.master_token = saved
        sessions.user_tokens = {}
    print(f"[tokens-file] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
