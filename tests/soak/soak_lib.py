#!/usr/bin/env python3
"""What the soak suites share, which stay Python (they drive pixelflux's and
pcmflux's Python bindings, #552): the check lines every suite prints, children
that die with the suite, a throwaway X server, and the sound server's sinks.

    E2E_DISPLAY  the display the other suites drive; a private server never takes it
    E2E_WORKDIR  scratch and logs (default $TMPDIR/selkies-tests)
"""
import ctypes
import os
import shutil
import signal
import subprocess
import tempfile
import time
from typing import Any, Iterable, Optional

TEST_DISPLAY = os.environ.get("E2E_DISPLAY", "")
WORKDIR = os.environ.get("E2E_WORKDIR", os.path.join(tempfile.gettempdir(), "selkies-tests"))
os.makedirs(WORKDIR, exist_ok=True)


def pulse_setup() -> None:
    """Create the 'output' null sink the audio capture reads through its monitor
    source, matching the deployed layout. Module ids must not collide across
    restarts, so an existing sink is left alone. Without pactl the sink has to
    exist already; only the audio checks depend on it.

    Every call is bounded, because a sound server that accepts a connection and
    then stops answering would otherwise hold the suite that asked until its own
    timeout killed it, with nothing said about where it stopped.
    """
    pactl = shutil.which("pactl")
    if not pactl:
        print("note: pactl not found, leaving the audio sink as it is", flush=True)
        return

    def ask(*args: str) -> Optional[subprocess.CompletedProcess]:
        try:
            return subprocess.run([pactl, *args], capture_output=True, text=True, timeout=10)
        except subprocess.TimeoutExpired:
            print(f"note: pactl {args[0]} did not answer in 10s; "
                  "leaving the audio sink as it is", flush=True)
            return None

    sources = ask("list", "short", "sources")
    if sources is None:
        return
    if "output.monitor" not in sources.stdout:
        ask("load-module", "module-null-sink",
            "sink_name=output", "rate=48000", "channels=2")
    # Reset on every setup: a suite that pointed the default elsewhere and died
    # would otherwise leave every later audio check reading silence.
    ask("set-default-sink", "output")

def pulse_null_sink(name: str, **opts: Any) -> Optional[str]:
    """Load a null sink named `name`, returning the module id to unload later.

    Nothing is loaded when a sink of that name already exists, and None comes
    back: pactl would otherwise happily create a second one under the same
    name, which is how a host ends up with six of them. Pass the id to
    pulse_unload() when finished — a sink left behind outlives the run that
    made it and every later run inherits it.
    """
    pactl = shutil.which("pactl")
    if not pactl:
        return None
    existing = subprocess.run([pactl, "list", "short", "sinks"],
                              capture_output=True, text=True).stdout
    if any(line.split("\t")[1:2] == [name] for line in existing.splitlines()):
        return None
    args = [f"sink_name={name}"] + [f"{k}={v}" for k, v in opts.items()]
    r = subprocess.run([pactl, "load-module", "module-null-sink"] + args,
                       capture_output=True, text=True, timeout=10)
    module_id = r.stdout.strip()
    return module_id if module_id.isdigit() else None

def pulse_unload(module_id: Optional[str]) -> None:
    """Unload a module pulse_null_sink() or pulse_sine() loaded; a no-op for None."""
    pactl = shutil.which("pactl")
    if pactl and module_id:
        subprocess.run([pactl, "unload-module", module_id],
                       capture_output=True, timeout=10)

PR_SET_PDEATHSIG = 1
_LIBC = ctypes.CDLL(None, use_errno=True)


def _die_with_parent() -> None:
    """preexec for spawned children: the kernel SIGKILLs the child the moment
    the test process dies, so an externally killed run (timeout or SIGKILL
    skips every ``finally``) cannot orphan servers or damage feeds into the
    shared display and port fixtures. The reparent check closes the
    fork-to-prctl race."""
    _LIBC.prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() == 1:
        os._exit(1)


def spawn(cmd: Iterable, **kwargs: Any) -> subprocess.Popen:
    """subprocess.Popen for a test child that must not outlive the suite."""
    kwargs.setdefault("preexec_fn", _die_with_parent)
    return subprocess.Popen(cmd, **kwargs)

class Results:
    """Collects PASS/FAIL/SKIP check lines for one suite block and prints each
    as it is recorded, in the line format the suites driver reads."""

    def __init__(self, block: str) -> None:
        self.block = block
        self.items: list = []
        self.skipped: list = []

    def check(self, name: str, ok: Any, detail: Any = "") -> None:
        """Record one check; any truthy `ok` passes."""
        self.items.append((name, bool(ok), str(detail)[:160]))
        print(("PASS" if ok else "FAIL") + f"  [{self.block}] {name}  {str(detail)[:110]}", flush=True)

    def skip(self, name: str, reason: Any = "") -> None:
        """Record a check the installed dependencies cannot observe. Not a
        failure: the behavior is unproven here rather than known broken."""
        self.skipped.append((name, str(reason)[:160]))
        print(f"SKIP  [{self.block}] {name}  {str(reason)[:110]}", flush=True)

    def failed(self) -> list:
        return [i for i in self.items if not i[1]]

    def summary(self) -> bool:
        """Print the block's pass count; True when every check passed."""
        f = self.failed()
        tail = f", {len(self.skipped)} skipped" if self.skipped else ""
        print(f"[{self.block}] {len(self.items) - len(f)}/{len(self.items)} passed{tail}", flush=True)
        return not f

def free_display(taken: Iterable[str] = ()) -> Iterable[str]:
    """Display numbers no X server on this host holds, most likely first.

    Both the lock file and the socket are consulted, since a server that died
    without cleaning up leaves one or the other behind. The numbers start well
    above those desktop sessions and container entrypoints take, so a throwaway
    server never lands on a display someone is working on. Yields candidates
    rather than one answer: only the server that wins the bind knows for sure.
    """
    skip = {d.lstrip(":").split(".")[0] for d in taken if d}
    for number in range(64, 256):
        if str(number) in skip:
            continue
        if os.path.exists(f"/tmp/.X{number}-lock"):
            continue
        if os.path.exists(f"/tmp/.X11-unix/X{number}"):
            continue
        yield f":{number}"


def x_lock_holder(display: str) -> Optional[int]:
    """The process id the display's lock file names, None without a readable one."""
    number = display.lstrip(":").split(".")[0]
    try:
        with open(f"/tmp/.X{number}-lock") as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None

def private_x_server(width: int = 1280, height: int = 720, depth: int = 24,
                     extra_args: Iterable[str] = (), xvfb: str = "Xvfb") -> tuple:
    """A throwaway Xvfb of this suite's own, on a display nothing else holds.

    GLX is off because it faults on some GPU hosts and no suite that wants a
    private server needs it. A number another server takes first is simply the
    next candidate, so suites running side by side do not collide; the attempts
    are bounded, because a host where no server can start at all must report
    that rather than work through every number in the range. A display that
    answers is taken only when its lock names this server: two suites starting
    together try the same number, and the one whose server loses the lock would
    otherwise take the winner's display in the moment before its own exits.

    Args:
        width: Screen width. Xvfb fixes its maximum screen size here, so a
            suite that wants a server which refuses to grow asks for a small one.
        height: Screen height.
        depth: Color depth.
        extra_args: Further Xvfb arguments, appended.
        xvfb: The server binary, for a suite that wants a particular one.

    Returns:
        `(process, display)`; the caller terminates the process.

    Raises:
        RuntimeError: Xvfb or xdpyinfo is missing, or no candidate came up.
    """
    for tool in (xvfb, "xdpyinfo"):
        if not shutil.which(tool):
            raise RuntimeError(f"{tool} is not installed; a private X server needs it")
    candidates = free_display(taken=(TEST_DISPLAY, os.environ.get("DISPLAY", "")))
    for _, display in zip(range(8), candidates):
        proc = spawn(
            [xvfb, display, "-screen", "0", f"{width}x{height}x{depth}",
             "-extension", "GLX", "-nolisten", "tcp", "-ac", "-noreset",
             *extra_args],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True)
        deadline = time.time() + 15
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            if subprocess.run(["xdpyinfo", "-display", display],
                              capture_output=True).returncode == 0:
                if x_lock_holder(display) == proc.pid:
                    return proc, display
                break
            time.sleep(0.25)
        proc.kill()
        proc.wait(timeout=5)
    raise RuntimeError("no display number yielded a working X server")

def stop_x_server(proc: subprocess.Popen, display: str) -> None:
    """Stop a private_x_server() and leave its display number reusable.

    Asked to quit rather than killed outright: a killed X server never removes
    its lock file or socket, and free_display() then treats that number as
    taken for as long as the machine is up. Anything the server did leave
    behind is cleaned here, but only when the lock names the process that has
    just died, so a number already reissued to someone else is untouched.
    """
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
    if x_lock_holder(display) != proc.pid:
        return
    number = display.lstrip(":").split(".")[0]
    for path in (f"/tmp/.X{number}-lock", f"/tmp/.X11-unix/X{number}"):
        try:
            os.unlink(path)
        except OSError:
            pass
