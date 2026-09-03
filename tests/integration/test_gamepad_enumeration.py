"""The joystick interposer must let an application that scans /dev/input find
the virtual pads, so apps that enumerate the directory instead of asking
libudev (SDL with udev disabled or built without it, GLFW, evtest) see them
without placeholder files. The interposer adds one evdev node per bound slot to
opendir/readdir and scandir; an unbound slot is never advertised, and the
directory read itself never fails because of the probe. Real host nodes pass
through untouched, so every check diffs against the directory's own baseline.
"""
import os
import subprocess
import sys
import tempfile
import textwrap
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

INTERPOSER_DIR = os.path.join(H.REPO, "addons", "js-interposer")


def build_interposer(workdir: str) -> str:
    """Compile the interposer into `workdir`, returning the .so path."""
    so = os.path.join(workdir, "selkies_joystick_interposer.so")
    subprocess.run(
        ["gcc", "-O2", "-shared", "-fPIC", "-o", so,
         os.path.join(INTERPOSER_DIR, "joystick_interposer.c"), "-ldl", "-pthread"],
        check=True, capture_output=True, text=True)
    return so


def serve_slots(sockdir: str, count: int) -> subprocess.Popen:
    """Start `count` gamepad slots, each with its js and evdev sockets bound."""
    script = textwrap.dedent(f"""
        import asyncio, os, signal, sys
        sys.path.insert(0, {H.SRC!r})
        from selkies.input_handler import SelkiesGamepad
        D = {sockdir!r}
        async def main():
            held = []
            for i in range({count}):
                gp = SelkiesGamepad(os.path.join(D, f"selkies_js{{i}}.sock"),
                                    os.path.join(D, f"selkies_event{{1000 + i}}.sock"),
                                    asyncio.get_running_loop())
                gp.set_config(f"Selkies Test Pad {{i}}", 16, 4)
                asyncio.create_task(gp.run_servers())
                held.append(gp)
            print("UP", flush=True)
            stop = asyncio.Event()
            asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, stop.set)
            await stop.wait()
            for gp in held:
                await gp.close()
        asyncio.run(main())
    """)
    proc = subprocess.Popen([H.PYTHON, "-c", script],
                            env={**os.environ, "SELKIES_JS_SOCKET_PATH": sockdir},
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    deadline = time.time() + 20
    while time.time() < deadline:
        line = proc.stdout.readline()
        if line.startswith("UP"):
            return proc
        if proc.poll() is not None:
            raise RuntimeError(f"gamepad server exited: {proc.stdout.read()}")
    raise RuntimeError("gamepad server did not come up")


def scan_dev_input(preload: str, sockdir: str) -> dict:
    """List /dev/input in a child under the interposer, three ways an app might:
    os.listdir (readdir), a filtered/sorted scandir via ctypes, and a stat of one
    node. Returns the parsed JSON the child prints.
    """
    child = textwrap.dedent("""
        import ctypes, ctypes.util, json, os, stat
        listdir = sorted(n for n in os.listdir("/dev/input") if n.startswith("event"))

        # scandir(3) with the SDL-style "event*" filter and alphasort, the path a
        # udev-less SDL or evtest takes; resolved from the global namespace (not
        # the libc handle) so the preloaded interposer's scandir wins, as it does
        # for a normally linked caller.
        libc = ctypes.CDLL(None, use_errno=True)
        class Dirent(ctypes.Structure):
            _fields_ = [("d_ino", ctypes.c_uint64), ("d_off", ctypes.c_int64),
                        ("d_reclen", ctypes.c_uint16), ("d_type", ctypes.c_uint8),
                        ("d_name", ctypes.c_char * 256)]
        FILTER = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.POINTER(Dirent))
        COMPAR = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
        libc.scandir.restype = ctypes.c_int
        libc.scandir.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.POINTER(ctypes.POINTER(Dirent))),
                                 FILTER, COMPAR]
        libc.alphasort.restype = ctypes.c_int
        keep = FILTER(lambda e: 1 if bytes(e.contents.d_name).startswith(b"event") else 0)
        alpha = ctypes.cast(libc.alphasort, COMPAR)
        namelist = ctypes.POINTER(ctypes.POINTER(Dirent))()
        n = libc.scandir(b"/dev/input", ctypes.byref(namelist), keep, alpha)
        scan = []
        for i in range(max(n, 0)):
            ent = namelist[i].contents
            scan.append([ent.d_name.decode(), ent.d_type])

        # A char-device stat is what a scanner does before opening the node.
        modes = {}
        for name in listdir:
            st = os.stat("/dev/input/" + name)
            modes[name] = [stat.S_ISCHR(st.st_mode), os.major(st.st_rdev)]
        print(json.dumps({"listdir": listdir, "scandir": scan, "modes": modes}))
    """)
    import json
    out = subprocess.run(
        [sys.executable, "-c", child],
        env={**os.environ, "LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": sockdir},
        capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"scan child failed: {out.stdout}\n{out.stderr}")
    return json.loads(out.stdout.strip().splitlines()[-1])


def main() -> bool:
    res = H.Results("gamepad-enum")
    if not os.path.isdir("/dev/input"):
        res.skip("dev-input", "/dev/input absent; the interposer augments an existing directory")
        return res.summary()

    work = tempfile.mkdtemp(prefix="js-enum-")
    try:
        try:
            preload = build_interposer(work)
        except subprocess.CalledProcessError as e:
            res.check("build", False, e.stderr[:150])
            return res.summary()
        res.check("build", os.path.exists(preload))

        # The host's own nodes (a desktop's real devices, a CI runner's) are
        # passed through by design; the interposer's contribution is the diff.
        baseline = sorted(n for n in os.listdir("/dev/input") if n.startswith("event"))

        # Two of four slots bound: exactly those two evdev nodes join the listing.
        sockdir = os.path.join(work, "sock")
        os.makedirs(sockdir)
        server = serve_slots(sockdir, 2)
        try:
            data = scan_dev_input(preload, sockdir)
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()

        listdir = data["listdir"]
        added = sorted(set(listdir) - set(baseline))
        res.check("readdir-bound-nodes",
                  added == ["event1000", "event1001"] and set(baseline) <= set(listdir),
                  f"listdir events={listdir} baseline={baseline}")
        res.check("readdir-hides-unbound", "event1002" not in listdir and "event1003" not in listdir,
                  f"unbound absent from {listdir}")
        scan_names = [n for n, _ in data["scandir"]]
        scan_added = sorted(set(scan_names) - set(baseline))
        res.check("scandir-bound-nodes",
                  scan_added == ["event1000", "event1001"] and set(baseline) <= set(scan_names),
                  f"scandir events={scan_names} baseline={baseline}")
        DT_CHR = 2
        added_types = [t for n, t in data["scandir"] if n not in baseline]
        res.check("scandir-d-type-chr", all(t == DT_CHR for t in added_types),
                  f"d_type values={added_types}")
        modes = {n: v for n, v in data["modes"].items() if n not in baseline}
        res.check("stat-char-device",
                  all(chr_ok and major == 13 for chr_ok, major in modes.values()),
                  f"modes={modes}")

        # No slots bound: the directory still reads cleanly, adding nothing.
        empty = os.path.join(work, "empty")
        os.makedirs(empty)
        try:
            none = scan_dev_input(preload, empty)
            res.check("no-nodes-without-sockets", none["listdir"] == baseline,
                      f"listdir events={none['listdir']} baseline={baseline}")
            res.check("readdir-errno-clean", True, "os.listdir did not raise on an empty slot set")
        except RuntimeError as e:
            res.check("readdir-errno-clean", False, str(e)[:150])
        hotplug_checks(res, preload, os.path.join(work, "hotplug"))
        sdl_hotplug_check(res, preload, os.path.join(work, "sdl"))
    finally:
        subprocess.run(["rm", "-rf", work], check=False)
    return res.summary()


IN_CREATE, IN_DELETE, IN_NONBLOCK, EAGAIN = 0x100, 0x200, 0o4000, 11

WATCHER = textwrap.dedent("""
    import ctypes, json, os, select, sys
    libc = ctypes.CDLL(None, use_errno=True)
    fd = libc.inotify_init1(0)
    wd = libc.inotify_add_watch(fd, b"/dev/input", 0x100 | 0x200)
    nb = libc.inotify_init1(0o4000)
    nbwd = libc.inotify_add_watch(nb, b"/dev/input", 0x100 | 0x200)
    print(json.dumps({"wd": wd, "nbwd": nbwd}), flush=True)
    buf = ctypes.create_string_buffer(4096)
    for line in sys.stdin:
        cmd = line.strip()
        if cmd == "read":
            r, _, _ = select.select([fd], [], [], 10)
            if not r:
                print(json.dumps({"timeout": True}), flush=True)
                continue
            n = libc.read(fd, buf, 4096)
            recs, off = [], 0
            while off + 16 <= n:
                w, mask, cookie, ln = (int.from_bytes(buf.raw[off:off + 4], "little", signed=True),
                                       int.from_bytes(buf.raw[off + 4:off + 8], "little"),
                                       int.from_bytes(buf.raw[off + 8:off + 12], "little"),
                                       int.from_bytes(buf.raw[off + 12:off + 16], "little"))
                name = buf.raw[off + 16:off + 16 + ln].split(b"\\0", 1)[0].decode()
                recs.append({"wd": w, "mask": mask, "name": name, "len": ln})
                off += 16 + ln
            print(json.dumps({"n": n, "recs": recs}), flush=True)
        elif cmd == "nbread":
            r, _, _ = select.select([nb], [], [], 3)
            n = libc.read(nb, buf, 4096)
            print(json.dumps({"ready": bool(r), "n": n, "errno": ctypes.get_errno() if n < 0 else 0}), flush=True)
        elif cmd == "rm":
            print(json.dumps({"rm": libc.inotify_rm_watch(fd, wd)}), flush=True)
        elif cmd == "quit":
            break
""")


def hotplug_checks(res: "H.Results", preload: str, sockdir: str) -> None:
    """An inotify watch on /dev/input, held by a child under the interposer,
    reports a slot bound and withdrawn later as its evdev node coming and
    going: the application's own watch descriptor, the node's name, and only
    what the sockets stand for. A read that carried nothing for the
    application never reads as end of file."""
    import json
    os.makedirs(sockdir)
    child = subprocess.Popen(
        [sys.executable, "-u", "-c", WATCHER],
        env={**os.environ, "LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": sockdir},
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    def ask(cmd: str) -> dict:
        child.stdin.write(cmd + "\n")
        child.stdin.flush()
        return json.loads(child.stdout.readline())

    try:
        ids = json.loads(child.stdout.readline())
        res.check("inotify-watch-added", ids["wd"] >= 0 and ids["nbwd"] >= 0, f"wds={ids}")
        server = serve_slots(sockdir, 1)
        try:
            seen = ask("read")
            recs = seen.get("recs", [])
            res.check("inotify-bound-slot-appears",
                      any(r["wd"] == ids["wd"] and r["mask"] & IN_CREATE and r["name"] == "event1000"
                          for r in recs),
                      f"records={recs}")
            res.check("inotify-sockets-stay-hidden",
                      all(not r["name"].startswith("selkies_") and not r["name"].startswith("js")
                          for r in recs),
                      f"records={recs}")
            res.check("inotify-record-padding",
                      all(r["len"] % 16 == 0 and r["len"] > len(r["name"]) for r in recs),
                      f"records={recs}")
            # Nothing on the non-blocking fd but the same create records; a
            # stray file in the socket directory is not a node and must not
            # read as end of file either.
            open(os.path.join(sockdir, "not-a-slot"), "w").close()
            time.sleep(0.3)
            first = ask("nbread")
            second = ask("nbread")
            res.check("inotify-nonblocking-never-eof",
                      first["n"] > 0 and second["n"] == -1 and second["errno"] == EAGAIN,
                      f"first={first} second={second}")
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
        gone = ask("read")
        recs = gone.get("recs", [])
        res.check("inotify-withdrawn-slot-vanishes",
                  any(r["wd"] == ids["wd"] and r["mask"] & IN_DELETE and r["name"] == "event1000"
                      for r in recs),
                  f"records={recs}")
        res.check("inotify-rm-watch", ask("rm")["rm"] == 0)
        child.stdin.write("quit\n")
        child.stdin.flush()
    finally:
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()


def sdl_hotplug_check(res: "H.Results", preload: str, work: str) -> None:
    """SDL with udev discovery disabled, started before the pad is served, gets
    a device-added event when the slot binds and a removed one when it goes."""
    os.makedirs(work)
    tool = os.path.join(work, "sdlhotplug")
    src = os.path.join(H.REPO, "tests", "tools", "gamepad", "sdlhotplug.c")
    flags = subprocess.run(["pkg-config", "--cflags", "--libs", "sdl2"], capture_output=True, text=True)
    if flags.returncode != 0 or subprocess.run(["gcc", "-O2", "-o", tool, src] + flags.stdout.split(),
                                                capture_output=True).returncode != 0:
        res.skip("sdl-hotplug", "no SDL2 development files")
        return
    sockdir = os.path.join(work, "sock")
    os.makedirs(sockdir)
    env = {k: v for k, v in os.environ.items() if k not in ("LD_PRELOAD", "SDL_JOYSTICK_DEVICE")}
    env.update({"LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": sockdir,
                "SDL_JOYSTICK_DISABLE_UDEV": "1", "SDL_JOYSTICK_HIDAPI": "0"})
    watcher = subprocess.Popen([tool, "8"], env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
    time.sleep(1.0)
    server = serve_slots(sockdir, 1)
    time.sleep(3.0)
    server.terminate()
    try:
        server.wait(timeout=5)
    except subprocess.TimeoutExpired:
        server.kill()
    out, _ = watcher.communicate(timeout=20)
    result = next((ln for ln in out.splitlines() if ln.startswith("RESULT")), "")
    res.check("sdl-hotplug-add-and-remove",
              "added=1" in result and "removed=1" in result and "start joysticks=0" in out,
              out.strip().replace("\n", " | ")[:300])


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
