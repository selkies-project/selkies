#!/usr/bin/env python3
"""An audio start the sound server is slow to answer holds up the audio alone.

pcmflux's start_capture waits up to its handshake window while its connect
retries run, which is seconds when the sound server cannot be reached. The
audio pipeline's start, stop and re-gating serialize on `_audio_lock`, not on
the reconfigure lock, so a settings change or a reconfigure meanwhile goes
ahead (it waited inline in a socket's receive loop before, holding that page's
input behind it), while another audio operation still waits its turn: a stop
asked for during the start lands after it. Runs in a fresh interpreter (the
server module reads settings at import) with fakes for the pipeline start and
stop.
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
    print(f"{'PASS' if ok else 'FAIL'}  [audio-lock] {label}  {detail}", flush=True)


PROBE = r"""
import asyncio, json, time
import selkies.websockets_mode as S

HANDSHAKE_S = 0.5
out = {"order": []}


def server():
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv._reconfigure_lock = asyncio.Lock()
    srv._audio_lock = asyncio.Lock()
    srv._reconfigure_pending = False
    srv.is_pcmflux_capturing = False
    srv.clients = set()

    async def start():
        out["order"].append("start")
        await asyncio.sleep(HANDSHAKE_S)
        srv.is_pcmflux_capturing = True
        out["order"].append("started")
        return True

    async def stop():
        out["order"].append("stop")
        srv.is_pcmflux_capturing = False
        return True

    srv._start_pcmflux_pipeline = start
    srv._stop_pcmflux_pipeline = stop
    return srv


async def main():
    S.PCMFLUX_AVAILABLE = True
    srv = server()
    starting = asyncio.create_task(srv._apply_initial_audio_policy(None, "primary"))
    await asyncio.sleep(0.05)
    out["audio_lock_held"] = srv._audio_lock.locked()
    out["reconfigure_lock_held"] = srv._reconfigure_lock.locked()
    t = time.monotonic()
    async with srv._reconfigure_lock:
        out["reconfigure_wait_ms"] = round((time.monotonic() - t) * 1000, 1)
    t = time.monotonic()
    async with srv._audio_lock:
        out["audio_wait_ms"] = round((time.monotonic() - t) * 1000, 1)
        out["capturing_when_audio_op_ran"] = srv.is_pcmflux_capturing
        if srv.is_pcmflux_capturing:
            await srv._stop_pcmflux_pipeline()
    await starting
    out["capturing_after"] = srv.is_pcmflux_capturing
    print(json.dumps(out))


asyncio.run(main())
"""


def main() -> bool:
    base_env = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}
    with tempfile.TemporaryDirectory(prefix="selkies-audio-lock-") as home:
        proc = subprocess.run(
            [sys.executable, "-c", PROBE], capture_output=True, text=True, timeout=240,
            env=dict(base_env, PYTHONPATH=os.path.join(REPO, "src"),
                     SELKIES_FILE_MANAGER_PATH=home))
    lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("{")]
    if not lines:
        check("probe ran", False, (proc.stderr or proc.stdout)[-600:])
        return False
    got = json.loads(lines[-1])
    check("an audio start in progress holds the audio lock", got.get("audio_lock_held") is True, got)
    check("and not the reconfigure lock", got.get("reconfigure_lock_held") is False, got)
    check("so a reconfiguration meanwhile goes ahead at once",
          got.get("reconfigure_wait_ms", 1e9) < 50, got.get("reconfigure_wait_ms"))
    check("while another audio operation waits for the start to finish",
          got.get("audio_wait_ms", 0) >= 400 and got.get("capturing_when_audio_op_ran") is True,
          (got.get("audio_wait_ms"), got.get("capturing_when_audio_op_ran")))
    check("so a stop asked for during the start lands after it",
          got.get("order") == ["start", "started", "stop"] and got.get("capturing_after") is False,
          (got.get("order"), got.get("capturing_after")))
    print(f"[audio-lock] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
