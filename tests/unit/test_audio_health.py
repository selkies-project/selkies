#!/usr/bin/env python3
"""A pcmflux capture that dies after start is noticed and restarted.

pcmflux answers start_capture while its worker may still be starting; a run
that gives up later surfaces only through `last_error`. The websockets audio
broadcast loop asks for it whenever its queue stays silent, logs the failure
once, and restarts the pipeline through the same stop/start the audio toggles
use — no more often than the restart floor. A start whose run already failed
is reported as a failed start. Older pcmflux builds without the attributes are
left alone. Runs in a fresh interpreter (the server module reads settings at
import) with fakes for the pcmflux module and the pipeline stop/start.
"""
import json
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [audio-health] {label}  {detail}", flush=True)


PROBE = r"""
import asyncio, json, logging, time
import selkies.selkies as S

out = {}


class Fake:
    def __init__(self, state="failed", last_error="PulseAudio connection lost"):
        self.state = state
        self.last_error = last_error


class Legacy:
    pass


def server():
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv._pcmflux_reported_failure = None
    srv._pcmflux_last_restart = 0.0
    srv._reconfigure_lock = asyncio.Lock()
    srv._reconfigure_pending = False
    srv.pcmflux_send_task = None
    srv.pcmflux_audio_queue = None
    srv.is_pcmflux_capturing = True
    calls = []

    async def stop():
        calls.append("stop")
        srv.pcmflux_module = None
        srv.is_pcmflux_capturing = False
        return True

    async def start():
        calls.append("start")
        srv.pcmflux_module = Fake(state="running", last_error=None)
        srv.is_pcmflux_capturing = True
        return True

    srv._stop_pcmflux_pipeline = stop
    srv._start_pcmflux_pipeline = start
    return srv, calls


errors = []
class Sink(logging.Handler):
    def emit(self, record):
        if record.levelno >= logging.ERROR:
            errors.append(record.getMessage())
S.data_logger.addHandler(Sink())
S.data_logger.setLevel(logging.INFO)


async def direct():
    srv, calls = server()
    srv.pcmflux_module = Fake()
    srv._check_pcmflux_health()
    srv._check_pcmflux_health()
    await asyncio.sleep(0.2)
    out["restart_calls"] = list(calls)
    out["error_logs"] = [e for e in errors if "PulseAudio connection lost" in e]
    # A second failure inside the restart floor is logged but not restarted.
    errors.clear()
    srv.pcmflux_module = Fake(last_error="device vanished")
    srv._check_pcmflux_health()
    await asyncio.sleep(0.2)
    out["floor_calls"] = list(calls)
    out["floor_logs"] = [e for e in errors if "device vanished" in e]
    # Past the floor it restarts again.
    srv._pcmflux_last_restart -= S.PCMFLUX_RESTART_FLOOR_SECONDS
    srv._check_pcmflux_health()
    await asyncio.sleep(0.2)
    out["after_floor_calls"] = list(calls)

    # A healthy run, and a pcmflux without the attributes, trigger nothing.
    srv2, calls2 = server()
    srv2.pcmflux_module = Fake(state="running", last_error=None)
    srv2._check_pcmflux_health()
    srv2.pcmflux_module = Legacy()
    srv2._check_pcmflux_health()
    await asyncio.sleep(0.2)
    out["healthy_calls"] = list(calls2)


async def through_loop():
    S.PCMFLUX_HEALTH_INTERVAL_SECONDS = 0.2
    srv, calls = server()
    srv.pcmflux_module = Fake()
    srv.pcmflux_audio_queue = asyncio.Queue()
    srv.clients = set()
    srv.display_clients = {}
    task = asyncio.create_task(srv._pcmflux_send_audio_chunks())
    deadline = time.monotonic() + 3.0
    while time.monotonic() < deadline and "start" not in calls:
        await asyncio.sleep(0.05)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass
    out["loop_calls"] = list(calls)


async def main():
    await direct()
    await through_loop()

asyncio.run(main())
print(json.dumps(out))
"""


def main() -> bool:
    base_env = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}
    with tempfile.TemporaryDirectory(prefix="selkies-audio-health-") as home:
        proc = subprocess.run(
            [sys.executable, "-c", PROBE], capture_output=True, text=True, timeout=240,
            env=dict(base_env, PYTHONPATH=os.path.join(REPO, "src"),
                     SELKIES_FILE_MANAGER_PATH=home))
    lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("{")]
    if not lines:
        check("probe ran", False, (proc.stderr or proc.stdout)[-600:])
        return False
    got = json.loads(lines[-1])
    check("a failed run is restarted through stop then start",
          got.get("restart_calls") == ["stop", "start"], got.get("restart_calls"))
    check("the failure is logged once", len(got.get("error_logs", [])) == 1, got.get("error_logs"))
    check("a second failure inside the restart floor is logged but not restarted",
          got.get("floor_calls") == ["stop", "start"] and len(got.get("floor_logs", [])) == 1,
          (got.get("floor_calls"), got.get("floor_logs")))
    check("past the floor the next failure restarts again",
          got.get("after_floor_calls") == ["stop", "start", "stop", "start"], got.get("after_floor_calls"))
    check("a healthy run and a pcmflux without the attributes are left alone",
          got.get("healthy_calls") == [], got.get("healthy_calls"))
    check("the broadcast loop's silent queue drives the check",
          got.get("loop_calls") == ["stop", "start"], got.get("loop_calls"))
    print(f"[audio-health] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
