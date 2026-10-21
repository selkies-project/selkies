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
import concurrent.futures
import os
import pathlib
import shutil
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
from selkies import stream_server  # noqa: E402
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
    check("_file_parts: a NUL byte is refused rather than raised on", server._file_parts("a\x00b") is None)
    check("resolve: a NUL byte is refused rather than raised on", resolve("a\x00b") is None)
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

    # A root of "/" is a prefix that already ends in the separator.
    slash = make_server("/")
    check("a root of /: an entry under it resolves", slash._resolve_file_path("tmp") == "/tmp",
          slash._resolve_file_path("tmp"))
    fd = slash._open_dir_fd(["tmp"])
    check("a root of /: the descriptor walk enters it", os.readlink(f"/proc/self/fd/{fd}") == "/tmp")
    os.close(fd)


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
    check("the resolved open: a file is not a directory",
          raises(NotADirectoryError, server._open_resolved_fd, ["plain.txt"]))
    os.mkfifo(on_disk(server, "pipe"))
    check("the resolved open: a FIFO is refused at once, not waited on for a writer",
          raises(NotADirectoryError, server._open_resolved_fd, ["pipe"]))
    os.unlink(on_disk(server, "pipe"))
    os.unlink(on_disk(server, "away"))


def raises(kind: type, call, *args) -> bool:
    """Whether `call(*args)` raises `kind` within five seconds."""
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        future = pool.submit(call, *args)
        try:
            result = future.result(timeout=5)
        except kind:
            return True
        except Exception:
            return False
    if isinstance(result, tuple):
        os.close(result[0])
    return False


async def route_cases(server: CentralizedStreamServer, root: str) -> None:
    sessions.user_tokens.clear()
    sessions.user_tokens.update({CTRL: {"role": "controller", "slot": 1},
                                 VIEW: {"role": "viewer", "slot": None}})
    sessions.active_mk_token = None
    app = web.Application(middlewares=[server._auth_middleware])
    app["settings"] = server.settings
    app.add_routes([
        web.get("/api/file-manager/browse", server.handle_files_browse),
        web.post("/api/file-manager/mkdir", server.handle_files_mkdir),
        web.get("/api/file-manager/zip", server.handle_files_zip, allow_head=False),
        web.post("/api/upload", server.handle_upload),
    ])
    client = TestClient(TestServer(app))
    await client.start_server()
    recorder = Recorder()
    try:
        r = await client.get("/api/file-manager/browse")
        check("browse: no credential is challenged", r.status == 401, r.status)
        r = await client.get("/api/file-manager/browse", headers=bearer(VIEW))
        check("browse: a viewer is refused the listing", r.status == 403, r.status)
        r = await client.get("/api/file-manager/browse", headers=bearer(CTRL))
        body = await r.json()
        names = {e["name"] for e in body.get("entries", [])}
        check("browse: the root lists its entries as JSON",
              r.status == 200 and {"real", "docs", "plain.txt", "missing"} <= names, (r.status, sorted(names)))
        r = await client.get("/api/file-manager/browse?path=docs", headers=bearer(CTRL))
        body = await r.json()
        check("browse: an in-tree link lists the directory it names",
              r.status == 200 and [e["name"] for e in body.get("entries", [])] == ["plain.txt"], body)
        r = await client.get("/api/file-manager/browse?path=plain.txt", headers=bearer(CTRL))
        check("browse: a file is not a directory", r.status == 404, r.status)
        r = await client.get("/api/file-manager/browse?path=../..", headers=bearer(CTRL))
        check("browse: a traversal is refused", r.status == 400, r.status)
        r = await client.get("/api/file-manager/browse?path=real%00x", headers=bearer(CTRL))
        check("browse: a NUL byte is refused, not a 500", r.status == 400, r.status)
        r = await client.get("/api/file-manager/browse?path=.//real/", headers=bearer(CTRL))
        body = await r.json()
        check("browse: entry paths are the normalized ones, not the request's spelling",
              r.status == 200 and body.get("path") == "real"
              and [e["path"] for e in body.get("entries", [])] == ["real/plain.txt"], body)

        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": ""})
        check("mkdir: an empty path is refused", r.status == 400, r.status)
        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "sub dir/.."})
        check("mkdir: a root spelling is refused", r.status == 400, r.status)
        r = await client.post("/api/file-manager/mkdir", headers=bearer(VIEW), json={"path": "viewer-dir"})
        check("mkdir: a viewer is refused", r.status == 403, r.status)
        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "deep/nested/dir"})
        made = os.path.isdir(on_disk(server, "deep/nested/dir"))
        check("mkdir: the missing intermediates are made with the leaf",
              r.status == 200 and made, (r.status, made))
        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "deep/nested/dir"})
        check("mkdir: one that exists is a conflict", r.status == 409, r.status)
        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "plain.txt/below"})
        check("mkdir: a file in the way is a conflict", r.status == 409, r.status)
        r = await client.post("/api/file-manager/mkdir", headers={**bearer(CTRL), "Sec-Fetch-Site": "cross-site"},
                              json={"path": "cross-site-dir"})
        check("mkdir: another site's page is refused",
              r.status == 403 and not os.path.exists(on_disk(server, "cross-site-dir")), r.status)
        os.symlink(os.path.dirname(root), on_disk(server, "away"))
        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "away/evil"})
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

        r = await client.get("/api/file-manager/zip?path=real", headers=bearer(CTRL))
        archived = await r.read()
        check("zip: a folder comes back as one archive of the pinned directory",
              r.status == 200 and archived[:2] == b"PK" and b"plain.txt" in archived, (r.status, archived[:8]))
        r = await client.get("/api/file-manager/zip?path=real", headers={**bearer(CTRL), "Range": "bytes=3-"})
        check("zip: a Range resume is refused before anything is packed",
              r.status == 405 and r.headers.get("Allow") == "GET", r.status)
        r = await client.get("/api/file-manager/zip", headers=bearer(CTRL))
        check("zip: the root itself is refused", r.status == 404, r.status)
        r = await client.get("/api/file-manager/zip?path=docs", headers=bearer(CTRL))
        check("zip: an in-tree link is followed like the listing follows it",
              r.status == 200 and (await r.read())[:2] == b"PK", r.status)
        r = await client.get("/api/file-manager/zip?path=real", headers=bearer(VIEW))
        check("zip: a viewer is refused the archive", r.status == 403, r.status)
        r = await client.get("/api/file-manager/zip?path=real", headers={**bearer(CTRL), "Sec-Fetch-Site": "cross-site"})
        check("zip: another site's page is refused before anything is packed", r.status == 403, r.status)
        r = await client.get("/api/file-manager/zip?path=plain.txt", headers=bearer(CTRL))
        check("zip: a file rather than a folder is not found", r.status == 404, r.status)

        # What the pack leaves out: a link to a file outside the tree, a link
        # to a file inside it, and a FIFO; what it keeps: a file from before
        # 1980, which a zip entry cannot date and the pack clamps.
        outside = os.path.join(os.path.dirname(root), "zip-outside.txt")
        with open(outside, "wb") as f:
            f.write(b"OUTSIDE-SECRET")
        os.symlink(outside, on_disk(server, "real/out-link"))
        os.symlink("plain.txt", on_disk(server, "real/in-link"))
        os.mkfifo(on_disk(server, "real/pipe"))
        with open(on_disk(server, "real/old.txt"), "wb") as f:
            f.write(b"old")
        os.utime(on_disk(server, "real/old.txt"), (0, 0))
        r = await client.get("/api/file-manager/zip?path=real", headers=bearer(CTRL))
        archived = await r.read()
        names = zip_names(archived)
        check("zip: links and a FIFO are left out, a pre-1980 file is packed",
              r.status == 200 and names == ["old.txt", "plain.txt"] and b"OUTSIDE-SECRET" not in archived,
              (r.status, names))
        for name in ("real/out-link", "real/in-link", "real/pipe", "real/old.txt"):
            os.unlink(on_disk(server, name))
        os.unlink(outside)

        # Audit, 2026-10-22: what a listing and an archive may give away.
        secret = os.path.join(os.path.dirname(root), "listing-outside.bin")
        with open(secret, "wb") as f:
            f.write(b"x" * 12345)
        os.symlink(secret, on_disk(server, "out-file"))
        os.symlink(os.path.dirname(root), on_disk(server, "out-dir"))
        r = await client.get("/api/file-manager/browse", headers=bearer(CTRL))
        listed = {e["name"]: e for e in (await r.json()).get("entries", [])}
        check("browse: a link out of the tree is listed as itself, not as what it points at",
              r.status == 200 and listed.get("out-file", {}).get("size") not in (None, 12345)
              and listed.get("out-dir", {}).get("is_dir") is False and listed.get("docs", {}).get("is_dir") is True,
              {k: listed.get(k) for k in ("out-file", "out-dir", "docs")})
        os.unlink(on_disk(server, "out-file"))
        os.unlink(on_disk(server, "out-dir"))
        os.unlink(secret)

        limit = stream_server.FILE_LISTING_MAX_ENTRIES
        stream_server.FILE_LISTING_MAX_ENTRIES = 1
        try:
            r = await client.get("/api/file-manager/browse", headers=bearer(CTRL))
        finally:
            stream_server.FILE_LISTING_MAX_ENTRIES = limit
        check("browse: a folder past the listing bound is refused with 413", r.status == 413, r.status)

        os.makedirs(on_disk(server, "line\nbreak"))
        setuid = on_disk(server, "line\nbreak/tool")
        with open(setuid, "wb") as f:
            f.write(b"#!/bin/sh\n")
        os.chmod(setuid, 0o4755)
        with open(os.fsencode(on_disk(server, "line\nbreak")) + b"/odd-\xff.bin", "wb") as f:
            f.write(b"odd")
        r = await client.get("/api/file-manager/zip?path=line%0Abreak", headers=bearer(CTRL))
        archived = await r.read()
        modes = zip_modes(archived)
        check("zip: no set-ID bit rides into the archive, and a name that is not UTF-8 is packed",
              r.status == 200 and modes.get("tool") == 0o755 and "odd-\ufffd.bin" in modes, (r.status, modes))
        disposition = r.headers.get("Content-Disposition", "")
        check("zip: a control character in the folder's name stays out of the header's quoted name",
              'filename="line_break.zip"' in disposition and "\n" not in disposition, disposition)
        shutil.rmtree(on_disk(server, "line\nbreak"))

        r = await client.post("/api/file-manager/mkdir", headers=bearer(CTRL), json={"path": "n" * 300})
        message = (await r.json()).get("message", "")
        check("mkdir: an error names the failure, not the server's paths",
              r.status == 500 and root not in message and message, (r.status, message))

        limit = stream_server.ARCHIVE_MAX_ENTRIES
        stream_server.ARCHIVE_MAX_ENTRIES = 0
        try:
            r = await client.get("/api/file-manager/zip?path=real", headers=bearer(CTRL))
        finally:
            stream_server.ARCHIVE_MAX_ENTRIES = limit
        check("zip: a folder past the bound is refused with 413", r.status == 413, r.status)
        os.unlink(on_disk(server, "docs"))
    finally:
        recorder.close()
        await client.close()
        sessions.user_tokens.clear()


def zip_modes(archived: bytes) -> dict:
    """Each archived file's permission bits, by name."""
    import io
    import zipfile
    try:
        with zipfile.ZipFile(io.BytesIO(archived)) as archive:
            return {i.filename: (i.external_attr >> 16) & 0o7777 for i in archive.infolist()}
    except zipfile.BadZipFile:
        return {}


def zip_names(archived: bytes) -> list:
    import io
    import zipfile
    try:
        with zipfile.ZipFile(io.BytesIO(archived)) as archive:
            return sorted(archive.namelist())
    except zipfile.BadZipFile:
        return []


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
