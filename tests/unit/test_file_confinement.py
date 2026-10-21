#!/usr/bin/env python3
"""The file manager's path confinement: the string check and the descriptor act.

`_resolve_file_path` is the check the static path analyzer follows: a
`realpath`-then-prefix comparison that refuses traversal, a leaf symlink out
of the tree, and a sibling whose name merely extends the root's. The
endpoints that act rather than list do not act on that string: `_open_dir_fd`
walks the components through directory descriptors, opening each relative to
the one before and verifying where it landed from the descriptor itself, and
the act is named through the held descriptor (`dir_fd`), so a symlink planted
between the check and the act cannot steer it outside the root — the one case
the string check cannot see. An in-tree symlink stays what it always was, a
link the manager follows the way the resolved path did.
"""
import asyncio
import os
import pathlib
import stat
import sys
import tempfile
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-confinement-")

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402
from selkies import audit, sessions  # noqa: E402
from selkies.stream_server import (  # noqa: E402
    CentralizedStreamServer, FileRootEscape)

CTRL, VIEW = "unit-ctrl-token", "unit-view-token"
passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    ok = bool(ok)
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [file-confinement] {label}  {detail}", flush=True)


def settings(**over) -> SimpleNamespace:
    base = dict(enable_basic_auth=(False,), basic_auth_user="user", basic_auth_password="secret",
                basic_auth_viewonly_password="", master_token="unit-master-token", subfolder="",
                allowed_origins="", wayland=(False, False), audio_enabled=(True, False),
                audio_device_name="output.monitor", audio_channels=2, audio_bitrate="96000",
                file_transfers=("upload", "download"))
    base.update(over)
    return SimpleNamespace(**base)


def make_server(root: str) -> CentralizedStreamServer:
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = settings(file_manager_path=root)
    server.upload_dir = pathlib.Path(root)
    server.transfer_cap = SimpleNamespace(active=False)
    server._chunked_uploads = {}
    return server


def bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


class Recorder:
    def __init__(self) -> None:
        self.events: list = []
        self._emit = audit.emit
        audit.emit = lambda event, **fields: self.events.append((event, fields))

    def close(self) -> None:
        audit.emit = self._emit


def primitive_cases(server: CentralizedStreamServer, root: str) -> None:
    resolve = server._resolve_file_path
    base = str(server.upload_dir)
    check("_file_parts: traversal is the escape", server._file_parts("../x") is None)
    check("_file_parts: a buried round trip that still escapes is the escape",
          server._file_parts("a/../../x") is None)
    check("_file_parts: dot components drop out", server._file_parts("./a/./b") == ["a", "b"])
    check("_file_parts: nothing left is the root", server._file_parts("a/..") == [])
    check("resolve: an empty path is the root itself", resolve("") == base)
    check("resolve: an entry under the root resolves", resolve("sub dir/plain.bin") == os.path.join(base, "sub dir", "plain.bin"))

    sibling = pathlib.Path(root).parent / "managerx"
    sibling.mkdir(exist_ok=True)
    (sibling / "secret.txt").write_text("outside")
    os.symlink(os.path.join("..", sibling.name), on_disk(server, "alias-dir"))
    os.symlink(os.path.join("..", sibling.name, "secret.txt"), on_disk(server, "alias-file"))
    check("resolve: a sibling whose name extends the root's is not read as inside it",
          resolve("alias-dir/secret.txt") is None and resolve("alias-dir") is None,
          (resolve("alias-dir/secret.txt"), resolve("alias-dir")))
    check("resolve: a leaf link to that sibling is refused",
          resolve("alias-file") is None, resolve("alias-file"))
    check("the descriptor walk refuses the sibling link like the check does",
          raises_escape(server, ["alias-dir"]), "FileRootEscape")
    os.unlink(on_disk(server, "alias-dir"))
    os.unlink(on_disk(server, "alias-file"))


def on_disk(server: CentralizedStreamServer, name: str) -> str:
    return os.path.join(str(server.upload_dir), name)


def raises_escape(server: CentralizedStreamServer, parts: list) -> bool:
    try:
        fd = server._open_dir_fd(parts)
    except FileRootEscape:
        return True
    if fd is not None:
        os.close(fd)
    return False


def walk_cases(server: CentralizedStreamServer, root: str) -> None:
    os.makedirs(on_disk(server, "real"), exist_ok=True)
    with open(on_disk(server, os.path.join("real", "plain.txt")), "wb") as f:
        f.write(b"payload")
    os.symlink("real", on_disk(server, "docs"))
    os.symlink(os.path.dirname(root), on_disk(server, "away"))

    fd = server._open_dir_fd([])
    check("the walk: an empty walk is the root", os.path.isdir(f"/proc/self/fd/{fd}"))
    os.close(fd)
    try:
        server._open_dir_fd(["missing"])
        check("the walk: a missing component raises", False)
    except FileNotFoundError:
        check("the walk: a missing component raises", True)
    fd = server._open_dir_fd(["missing", "deep"], create=True)
    created = os.path.isdir(on_disk(server, "missing/deep"))
    os.close(fd)
    check("the walk: create makes the missing intermediates in place", created)
    fd = server._open_dir_fd(["docs"])
    followed = os.path.basename(os.readlink(f"/proc/self/fd/{fd}")) == "real"
    os.close(fd)
    check("the walk: an in-tree link is followed", followed)
    check("the walk: a link out of the tree ends it", raises_escape(server, ["away"]))

    opened = server._open_resolved_fd(["docs"])
    check("the resolved open: an in-tree leaf link opens and verifies",
          opened is not None and opened[1] == os.path.realpath(on_disk(server, "real")),
          opened[1] if opened else None)
    if opened:
        os.close(opened[0])
    opened = server._open_resolved_fd(["away"])
    check("the resolved open: a leaf link out of the tree is refused", opened is None, opened)
    opened = server._open_resolved_fd(["real"])
    check("the resolved open: a plain directory opens to its own path",
          opened is not None and opened[1] == on_disk(server, "real"))
    if opened:
        os.close(opened[0])
    try:
        server._open_resolved_fd(["absent"])
        check("the resolved open: a missing leaf raises", False)
    except FileNotFoundError:
        check("the resolved open: a missing leaf raises", True)
    with open(on_disk(server, "plain.txt"), "wb") as f:
        f.write(b"payload")
    opened = server._open_resolved_fd(["plain.txt"])
    regular = opened is not None and stat.S_ISREG(os.fstat(opened[0]).st_mode)
    if opened:
        os.close(opened[0])
    check("the resolved open: a file opens as a file", regular)
    os.unlink(on_disk(server, "away"))


async def route_cases(server: CentralizedStreamServer, root: str) -> None:
    sessions.user_tokens.clear()
    sessions.user_tokens.update({CTRL: {"role": "controller", "slot": 1},
                                 VIEW: {"role": "viewer", "slot": None}})
    sessions.active_mk_token = None
    app = web.Application(middlewares=[server._auth_middleware])
    app["settings"] = server.settings
    app.add_routes([
        web.get("/api/files/_browse", server.handle_files_browse),
        web.post("/api/files/_mkdir", server.handle_files_mkdir),
        web.get("/api/files/_zip", server.handle_files_zip, allow_head=False),
        web.post("/api/upload", server.handle_upload),
    ])
    client = TestClient(TestServer(app))
    await client.start_server()
    recorder = Recorder()
    try:
        r = await client.get("/api/files/_browse")
        check("browse: no credential is challenged", r.status == 401, r.status)
        r = await client.get("/api/files/_browse", headers=bearer(VIEW))
        check("browse: a viewer is refused the listing", r.status == 403, r.status)
        r = await client.get("/api/files/_browse", headers=bearer(CTRL))
        body = await r.json()
        names = {e["name"] for e in body.get("entries", [])}
        check("browse: the root lists its entries as JSON",
              r.status == 200 and {"real", "docs", "plain.txt", "missing"} <= names, (r.status, sorted(names)))
        r = await client.get("/api/files/_browse?path=docs", headers=bearer(CTRL))
        body = await r.json()
        check("browse: an in-tree link lists the directory it names",
              r.status == 200 and [e["name"] for e in body.get("entries", [])] == ["plain.txt"], body)
        r = await client.get("/api/files/_browse?path=plain.txt", headers=bearer(CTRL))
        check("browse: a file is not a directory", r.status == 404, r.status)
        r = await client.get("/api/files/_browse?path=../..", headers=bearer(CTRL))
        check("browse: a traversal is refused", r.status == 400, r.status)

        r = await client.post("/api/files/_mkdir", headers=bearer(CTRL), json={"path": ""})
        check("mkdir: an empty path is refused", r.status == 400, r.status)
        r = await client.post("/api/files/_mkdir", headers=bearer(CTRL), json={"path": "sub dir/.."})
        check("mkdir: a root spelling is refused", r.status == 400, r.status)
        r = await client.post("/api/files/_mkdir", headers=bearer(VIEW), json={"path": "viewer-dir"})
        check("mkdir: a viewer is refused", r.status == 403, r.status)
        r = await client.post("/api/files/_mkdir", headers=bearer(CTRL), json={"path": "deep/nested/dir"})
        made = os.path.isdir(on_disk(server, "deep/nested/dir"))
        check("mkdir: the missing intermediates are made with the leaf",
              r.status == 200 and made, (r.status, made))
        r = await client.post("/api/files/_mkdir", headers=bearer(CTRL), json={"path": "deep/nested/dir"})
        check("mkdir: one that exists is a conflict", r.status == 409, r.status)
        os.symlink(os.path.dirname(root), on_disk(server, "away"))
        r = await client.post("/api/files/_mkdir", headers=bearer(CTRL), json={"path": "away/evil"})
        check("mkdir: a link out of the tree cannot take the creation with it",
              r.status == 400 and not os.path.exists(os.path.join(os.path.dirname(root), "evil")), r.status)
        os.unlink(on_disk(server, "away"))
        os.symlink(os.path.dirname(root), on_disk(server, "away"))

        payload = b"plain upload bytes"
        r = await client.post("/api/upload", headers={**bearer(CTRL), "X-Upload-Path": "up%20dir/file.bin"},
                              data=payload)
        landed = open(on_disk(server, "up dir/file.bin"), "rb").read() if os.path.exists(
            on_disk(server, "up dir/file.bin")) else b""
        check("upload: a plain body lands inside the root under its name",
              r.status == 200 and landed == payload, (r.status, len(landed)))
        r = await client.post("/api/upload", headers={**bearer(CTRL), "X-Upload-Path": "away/evil.bin"},
                              data=b"x")
        check("upload: a link out of the tree is refused where it always was",
              r.status == 400 and not os.path.exists(os.path.join(os.path.dirname(root), "evil.bin")), r.status)
        r = await client.post("/api/upload", headers={**bearer(VIEW), "X-Upload-Path": "viewer.bin"},
                              data=b"x")
        check("upload: a viewer is refused", r.status == 403, r.status)

        head, tail = b"hello ", b"world!!"
        r = await client.post("/api/upload", headers={
            **bearer(CTRL), "X-Upload-Path": "chunked.bin", "X-Upload-Id": "t1",
            "X-Upload-Offset": "0", "X-Upload-Total": str(len(head) + len(tail))}, data=head)
        check("upload: the first slice is banked", r.status == 200 and (await r.json()).get("complete") is False, r.status)
        r = await client.post("/api/upload", headers={
            **bearer(CTRL), "X-Upload-Path": "chunked.bin", "X-Upload-Id": "t1",
            "X-Upload-Offset": str(len(head)), "X-Upload-Total": str(len(head) + len(tail)),
            "X-Upload-Final": "1"}, data=tail)
        final = open(on_disk(server, "chunked.bin"), "rb").read() if os.path.exists(
            on_disk(server, "chunked.bin")) else b""
        check("upload: the final slice renames the staged file onto the destination",
              r.status == 200 and final == head + tail, (r.status, final))

        r = await client.get("/api/files/_zip?path=real", headers=bearer(CTRL))
        archived = await r.read()
        check("zip: a folder comes back as one archive of the pinned directory",
              r.status == 200 and archived[:2] == b"PK" and b"plain.txt" in archived, (r.status, archived[:8]))
        r = await client.get("/api/files/_zip?path=real", headers={**bearer(CTRL), "Range": "bytes=3-"})
        check("zip: a Range resume is refused before anything is packed",
              r.status == 405 and r.headers.get("Allow") == "GET", r.status)
        r = await client.get("/api/files/_zip", headers=bearer(CTRL))
        check("zip: the root itself is refused", r.status == 404, r.status)
        r = await client.get("/api/files/_zip?path=docs", headers=bearer(CTRL))
        check("zip: an in-tree link is followed like the listing follows it",
              r.status == 200 and (await r.read())[:2] == b"PK", r.status)
        r = await client.get("/api/files/_zip?path=real", headers=bearer(VIEW))
        check("zip: a viewer is refused the archive", r.status == 403, r.status)
        os.unlink(on_disk(server, "docs"))
    finally:
        recorder.close()
        await client.close()
        sessions.user_tokens.clear()


async def main() -> None:
    root = tempfile.mkdtemp(prefix="selkies-confinement-")
    server = make_server(root)
    os.makedirs(os.path.join(root, "sub dir"), exist_ok=True)
    with open(os.path.join(root, "sub dir", "plain.bin"), "wb") as f:
        f.write(b"0123456789")
    primitive_cases(server, root)
    walk_cases(server, root)
    await route_cases(server, root)
    print(f"[file-confinement] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
