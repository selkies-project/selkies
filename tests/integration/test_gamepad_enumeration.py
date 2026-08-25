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
        import asyncio, os, sys
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
            await asyncio.sleep(3600)
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
    finally:
        subprocess.run(["rm", "-rf", work], check=False)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
