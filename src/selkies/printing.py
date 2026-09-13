# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Documents printed in the session, on their way to a browser.

The session's print queue, Selkies, is a CUPS scheduler run as this user
whose backend writes each job as a PDF into the print spool. The spool is
watched, every document that lands whole is announced to the controller
pages of the active transport, and a page fetches it once through
`/api/print/<name>`, which takes it out of the spool. The spool therefore
holds what no page has taken yet, and a page connecting later is told about
that too.
"""
import asyncio
import logging
import os
import shutil
import tempfile
from importlib.resources import files
from typing import Any, Awaitable, Callable, Dict, List, Optional, Tuple

from watchdog.events import FileSystemEventHandler
from watchdog.observers import Observer

logger = logging.getLogger("printing")

# How long a file that merely appeared in the spool is left alone before it
# counts as whole: one moved in from elsewhere is complete at once, one being
# written in place is opened or modified well within this.
SETTLE_SECONDS = 0.5


def document_name(name: str) -> Optional[str]:
    """`name` when it names a document in the spool, else None: a path, a
    hidden or partial file, or anything that is not a PDF is refused."""
    if name != os.path.basename(name) or name.startswith(".") or "\x00" in name:
        return None
    return name if name.lower().endswith(".pdf") else None


def pending(spool: str) -> List[Tuple[str, int]]:
    """Every document still in the spool with its size, oldest first."""
    try:
        entries = [e for e in os.scandir(spool) if e.is_file() and document_name(e.name)]
    except OSError:
        return []
    entries.sort(key=lambda e: e.stat().st_mtime)
    return [(e.name, e.stat().st_size) for e in entries]


class SpoolWatcher(FileSystemEventHandler):
    """Announces each document that lands whole in the spool.

    A document renamed into place or closed after being written there is
    whole at that event. One that only appears, moved in from another
    directory, is announced once it has sat unopened for `SETTLE_SECONDS`,
    which is what tells it apart from a file still being written.
    """

    def __init__(self, spool: str, loop: asyncio.AbstractEventLoop,
                 on_document: Callable[[str, int], Awaitable[None]]) -> None:
        self.spool = spool
        self.loop = loop
        self.on_document = on_document
        self.observer = Observer()
        self._appeared: Dict[str, asyncio.TimerHandle] = {}

    def start(self) -> None:
        os.makedirs(self.spool, exist_ok=True)
        self.observer.schedule(self, self.spool)
        self.observer.start()

    def stop(self) -> None:
        self.observer.stop()
        self.observer.join(timeout=2)

    # The observer thread reports here; the loop does the rest.
    def on_moved(self, event: Any) -> None:
        if not event.is_directory and os.path.dirname(event.dest_path) == self.spool:
            self.loop.call_soon_threadsafe(self._announce, os.path.basename(event.dest_path))

    def on_closed(self, event: Any) -> None:
        if not event.is_directory:
            self.loop.call_soon_threadsafe(self._announce, os.path.basename(event.src_path))

    def on_created(self, event: Any) -> None:
        if not event.is_directory:
            self.loop.call_soon_threadsafe(self._appeared_whole, os.path.basename(event.src_path))

    def on_opened(self, event: Any) -> None:
        if not event.is_directory:
            self.loop.call_soon_threadsafe(self._being_written, os.path.basename(event.src_path))

    on_modified = on_opened

    def _appeared_whole(self, name: str) -> None:
        self._being_written(name)
        self._appeared[name] = self.loop.call_later(SETTLE_SECONDS, self._announce, name)

    def _being_written(self, name: str) -> None:
        timer = self._appeared.pop(name, None)
        if timer is not None:
            timer.cancel()

    def _announce(self, name: str) -> None:
        self._being_written(name)
        if document_name(name) is None:
            return
        try:
            size = os.path.getsize(os.path.join(self.spool, name))
        except OSError:
            return
        self.loop.create_task(self.on_document(name, size))


class PrintQueue:
    """The session's print queue: a CUPS scheduler run as this user under the
    runtime directory, with one queue, Selkies, whose backend writes each job
    into the spool. The scheduler's programs are the installed ones, reached
    through a private tree that adds the backend, and its whole state lives
    under `root`, so nothing under /etc/cups is touched and no privilege is
    needed. Applications reach it with CUPS_SERVER set to `socket`.
    """

    def __init__(self, spool: str) -> None:
        runtime = os.environ.get("XDG_RUNTIME_DIR")
        self.root = os.path.join(runtime, "selkies-cups") if runtime else \
            os.path.join(tempfile.gettempdir(), f"selkies-cups-{os.getuid()}")
        self.socket = os.path.join(self.root, "cups.sock")
        self.spool = spool
        self.process: Optional[asyncio.subprocess.Process] = None

    @staticmethod
    def programs() -> Optional[Tuple[str, str, str]]:
        """`(cupsd, server bin, data dir)` of the installed CUPS, or None."""
        path = os.environ.get("PATH", "") + ":/usr/sbin:/usr/local/sbin"
        cupsd = shutil.which("cupsd", path=path)
        if not cupsd:
            return None
        prefix = os.path.dirname(os.path.dirname(os.path.realpath(cupsd)))
        for lib in ("lib", "lib64", "libexec"):
            server_bin = os.path.join(prefix, lib, "cups")
            if os.path.isfile(os.path.join(server_bin, "daemon", "cups-exec")) \
                    and os.path.isdir(os.path.join(server_bin, "filter")):
                return cupsd, server_bin, os.path.join(prefix, "share", "cups")
        return None

    def _prepare(self, server_bin: str, data_dir: str) -> None:
        for sub in ("ppd", "state", "cache", "spool", "tmp", "bin/backend"):
            os.makedirs(os.path.join(self.root, sub), exist_ok=True)
        os.makedirs(self.spool, exist_ok=True)
        for sub in ("filter", "daemon"):
            link = os.path.join(self.root, "bin", sub)
            if os.path.islink(link):
                os.unlink(link)
            os.symlink(os.path.join(server_bin, sub), link)
        package = files("selkies") / "cups"
        backend = os.path.join(self.root, "bin", "backend", "selkies")
        with open(backend, "wb") as out:
            out.write((package / "backend").read_bytes())
        os.chmod(backend, 0o755)
        with open(os.path.join(self.root, "ppd", "Selkies.ppd"), "wb") as out:
            out.write((package / "selkies.ppd").read_bytes())
        confs = {
            "cups-files.conf": f"""ServerRoot {self.root}
ServerBin {os.path.join(self.root, "bin")}
DataDir {data_dir}
StateDir {os.path.join(self.root, "state")}
CacheDir {os.path.join(self.root, "cache")}
RequestRoot {os.path.join(self.root, "spool")}
TempDir {os.path.join(self.root, "tmp")}
LogFileGroup {os.getgid()}
AccessLog {os.path.join(self.root, "access.log")}
PageLog {os.path.join(self.root, "page.log")}
ErrorLog {os.path.join(self.root, "error.log")}
""",
            "cupsd.conf": f"""Listen {self.socket}
LogLevel warn
Browsing Off
WebInterface No
""",
            "printers.conf": f"""<DefaultPrinter Selkies>
Info Selkies printer
Location In the browser
DeviceURI selkies:{self.spool}
State Idle
Accepting Yes
Shared No
JobSheets none none
ErrorPolicy retry-job
</DefaultPrinter>
""",
        }
        for name, text in confs.items():
            with open(os.path.join(self.root, name), "w") as out:
                out.write(text)
        if os.path.exists(self.socket):
            os.unlink(self.socket)

    async def start(self) -> bool:
        """Start the scheduler; False where CUPS is not installed."""
        found = self.programs()
        if found is None:
            logger.info("No CUPS scheduler on this host: documents printed into the spool are "
                        "still handed over, but there is no Selkies queue to print to "
                        "(cups-daemon and cups-filters provide one)")
            return False
        cupsd, server_bin, data_dir = found
        self._prepare(server_bin, data_dir)
        self.process = await asyncio.create_subprocess_exec(
            cupsd, "-f", "-c", os.path.join(self.root, "cupsd.conf"),
            "-s", os.path.join(self.root, "cups-files.conf"),
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
        for _ in range(100):
            if os.path.exists(self.socket) or self.process.returncode is not None:
                break
            await asyncio.sleep(0.1)
        if not os.path.exists(self.socket):
            logger.warning("The print queue did not come up; its log is %s",
                           os.path.join(self.root, "error.log"))
            await self.stop()
            return False
        logger.info("Print queue Selkies listening on %s, spooling to %s", self.socket, self.spool)
        return True

    async def stop(self) -> None:
        process, self.process = self.process, None
        if process is None or process.returncode is not None:
            return
        process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 5)
        except asyncio.TimeoutError:
            process.kill()
            await process.wait()
