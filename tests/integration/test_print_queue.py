#!/usr/bin/env python3
"""The session's print queue, run the way the server runs it.

`PrintQueue` starts a CUPS scheduler owned by this user under a private
runtime directory, declares the Selkies queue from the shipped PPD, and the
shipped backend writes each job into the print spool: a PDF whatever the
application sent, named after the job's title, never on top of an earlier
document, and never seen half-written. Needs cupsd with the cups-filters
chain and the lp and lpstat clients; skips without them.
"""
import asyncio
import os
import re
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]
from selkies import printing  # noqa: E402

WORK = os.path.join(H.WORKDIR, "print-queue")

# A one-page PDF drawing one rectangle, built by hand so the suite needs no
# PDF library.
CONTENT = b"0 0 1 rg 100 100 200 300 re f"
OBJECTS = [b"<< /Type /Catalog /Pages 2 0 R >>",
           b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
           b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R >>",
           b"<< /Length %d >>stream\n" % len(CONTENT) + CONTENT + b"\nendstream"]


def sample_pdf() -> bytes:
    out, offsets = b"%PDF-1.4\n", []
    for i, obj in enumerate(OBJECTS, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + obj + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(OBJECTS) + 1)
    out += b"".join(b"%010d 00000 n \n" % o for o in offsets)
    return out + b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(OBJECTS) + 1, xref)


def pages(path: str) -> int:
    return len(re.findall(rb"/Type\s*/Page\b(?!s)", open(path, "rb").read()))


async def main() -> None:
    path = os.environ.get("PATH", "") + ":/usr/sbin"
    if printing.PrintQueue.programs() is None or not all(shutil.which(t, path=path) for t in ("lp", "lpstat")):
        H.skip_suite("cupsd with the cups-filters chain, lp and lpstat are needed (cups-daemon, cups-client, cups-filters)")
    res = H.Results("print-queue")
    shutil.rmtree(WORK, ignore_errors=True)
    runtime, spool = os.path.join(WORK, "runtime"), os.path.join(WORK, "spool")
    os.makedirs(runtime)
    os.environ["XDG_RUNTIME_DIR"] = runtime
    queue = printing.PrintQueue(spool)
    env = {"PATH": path, "CUPS_SERVER": queue.socket, "HOME": WORK}

    def run(*cmd: str) -> subprocess.CompletedProcess:
        return subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)

    def spooled(want: int, timeout: float = 30) -> list:
        deadline = time.time() + timeout
        while time.time() < deadline:
            names = sorted(n for n in os.listdir(spool) if not n.startswith("."))
            if len(names) >= want and not run("lpstat", "-o").stdout.strip():
                return names
            time.sleep(0.2)
        return sorted(os.listdir(spool))

    res.check("the queue starts under the runtime directory",
              await queue.start() and queue.socket.startswith(runtime), queue.socket)
    try:
        res.check("the scheduler answers on the session socket", "running" in run("lpstat", "-r").stdout)
        res.check("Selkies is the one queue, idle, and the default destination",
                  "Selkies is idle" in run("lpstat", "-p").stdout
                  and run("lpstat", "-d").stdout.strip().endswith("Selkies"),
                  run("lpstat", "-p", "-d").stdout.strip())
        res.check("the spool was created", os.path.isdir(spool))
        res.check("nothing under /etc/cups was needed",
                  all(os.path.exists(os.path.join(queue.root, f)) for f in ("cupsd.conf", "printers.conf", "ppd/Selkies.ppd")))

        text = os.path.join(WORK, "hello.txt")
        open(text, "w").write("Hello from a text job\nline two\n")
        pdf = os.path.join(WORK, "sample.pdf")
        open(pdf, "wb").write(sample_pdf())
        run("lp", "-d", "Selkies", "-t", "Quarterly report/../evil", text)
        names = spooled(1)
        res.check("a text job lands as a PDF named after its title, the path characters replaced",
                  names == ["Quarterly report_.._evil.pdf"], names)
        landed = os.path.join(spool, "Quarterly report_.._evil.pdf")
        res.check("the document is a whole one-page PDF",
                  os.path.isfile(landed) and open(landed, "rb").read(4) == b"%PDF" and pages(landed) == 1,
                  os.path.getsize(landed) if os.path.isfile(landed) else "missing")

        run("lp", "-d", "Selkies", "-t", "Report.pdf", pdf)
        run("lp", "-d", "Selkies", "-t", "Report", pdf)
        names = spooled(3)
        res.check("a second document of the same title lands beside the first, not on it",
                  "Report.pdf" in names and "Report (2).pdf" in names, names)
        res.check("a PDF job keeps its page through the filter chain",
                  pages(os.path.join(spool, "Report.pdf")) == 1)
        run("lp", "-d", "Selkies", "-t", "///", pdf)
        run("lp", "-d", "Selkies", "-t", ".. .secret ", pdf)
        names = spooled(5)
        res.check("a title with nothing usable becomes document.pdf", "document.pdf" in names, names)
        res.check("a title that would hide the document is trimmed into view", "secret.pdf" in names, names)
        res.check("no partial file is left behind",
                  not [n for n in os.listdir(spool) if n.startswith(".")], os.listdir(spool))
        res.check("every completed job is on record",
                  run("lpstat", "-W", "completed").stdout.count("Selkies-") == 5,
                  run("lpstat", "-W", "completed").stdout)
        pid = queue.process.pid
        await queue.stop()
        res.check("the scheduler stops with the queue", queue.process is None and not os.path.exists(f"/proc/{pid}"))
        res.check("a second start replaces the state under the same root",
                  await queue.start() and "running" in run("lpstat", "-r").stdout)
    finally:
        await queue.stop()
    if res.failed():
        print(H.tail(os.path.join(queue.root, "error.log"), 20))
    sys.exit(0 if res.summary() else 1)


if __name__ == "__main__":
    asyncio.run(main())
