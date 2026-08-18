#!/usr/bin/env python3
"""A failed clipboard-monitor build must not strand an X connection.

The monitor opens a display, then interns atoms and selects XFixes events over
it. Any of those round trips can raise while the connection is open, and the
caller retries on a timer -- so one stranded connection per attempt exhausts the
server's client slots within minutes. After that every unrelated component fails
to reach the display, which reads as a capture or input fault rather than a leak.

Counts the server's clients across repeated failed builds instead of trusting
the code path, because the failure only shows up in aggregate.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

DISPLAY = os.environ.get("LEAK_DISPLAY", ":96")
ATTEMPTS = 40


def start_server() -> subprocess.Popen:
    """Start a throwaway Xvfb on DISPLAY and wait until it answers queries.

    Returns:
        The Xvfb process handle.

    Raises:
        RuntimeError: If the server does not come up within the poll window.
    """
    proc = subprocess.Popen(
        ["Xvfb", DISPLAY, "-screen", "0", "640x480x24", "-extension", "GLX",
         "-nolisten", "tcp", "-ac", "-noreset"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(30):
        if subprocess.run(["xdpyinfo", "-display", DISPLAY],
                          capture_output=True).returncode == 0:
            return proc
        time.sleep(0.5)
    proc.kill()
    raise RuntimeError("test X server did not start")


def open_fd_count() -> int:
    """Descriptors this process holds; one leaked connection is one socket."""
    return len(os.listdir("/proc/self/fd"))


def main() -> bool:
    """Fail every monitor build repeatedly and count the descriptors left behind."""
    res = H.Results("x-connection-leak")
    server = start_server()
    try:
        from selkies import input_handler as IH

        # Force every build to fail after the connection is open, which is what a
        # disrupted server does to the round trips that follow it.
        original = IH._X11ClipboardMonitor._build

        def failing_build(self):
            raise RuntimeError("simulated mid-construction failure")

        IH._X11ClipboardMonitor._build = failing_build
        try:
            before = open_fd_count()
            failures = 0
            for _ in range(ATTEMPTS):
                try:
                    IH._X11ClipboardMonitor(DISPLAY)
                except RuntimeError:
                    failures += 1
            res.check("every build failed as arranged", failures == ATTEMPTS, failures)

            # Counted as descriptors held by this process: the server's client
            # ceiling is in the hundreds, so a handful of failed builds would not
            # show up as a refusal even though each one stranded a connection.
            leaked = open_fd_count() - before
            res.check("no descriptor stranded per failed build", leaked <= 2,
                      "{} fds after {} failed builds".format(leaked, ATTEMPTS))
        finally:
            IH._X11ClipboardMonitor._build = original

        # And a successful monitor still closes cleanly, fds included.
        mon = IH._X11ClipboardMonitor(DISPLAY)
        cmd_fds = (mon._cmd_r, mon._cmd_w)
        mon.close()
        closed = []
        for fd in cmd_fds:
            try:
                os.fstat(fd)
                closed.append(False)
            except OSError:
                closed.append(True)
        res.check("close() releases the self-pipe descriptors", all(closed), cmd_fds)
    finally:
        server.kill()
        server.wait(timeout=10)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
