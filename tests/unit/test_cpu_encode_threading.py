#!/usr/bin/env python3
"""cpu_encode_threading reaches pixelflux on both transports, and its switch shows only in software.

The setting defaults on and parses from SELKIES_CPU_ENCODE_THREADING, where
"false|locked" pins it off. The capture builder both transports share sets
pixelflux's CaptureSettings.cpu_encode_threading from it, a change rebuilds
the capture (it is structural on WebSockets, a restart on WebRTC), and the
dashboards' gate (`encodesInSoftware`) reads a session as software where it
is: a CPU-only encoder, software forced on a codec that has it, a codec the
host encodes in software alone, or a full-color stream the engine hands over.
"""
import json
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
SRC = os.path.join(REPO, "src", "selkies")
CONDITIONAL = os.path.join(REPO, "addons", "selkies-web-core", "lib", "conditional-settings.js")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [cpu-threading] {label}  {detail}", flush=True)


# The settings singleton reads argv and SELKIES_* variables at import: a fresh interpreter per case.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}


def probe(code: str, **env: str) -> str:
    out = subprocess.run(
        [sys.executable, "-c", f"import selkies.settings as s; {code}"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"), **env))
    return out.stdout.strip() or out.stderr.strip()[-300:]


got = probe("print(s.settings.cpu_encode_threading)")
check("defaults on, unlocked", got == "(True, False)", got)
got = probe("print(s.settings.cpu_encode_threading)", SELKIES_CPU_ENCODE_THREADING="false")
check("SELKIES_CPU_ENCODE_THREADING=false turns it off", got == "(False, False)", got)
got = probe("print(s.settings.cpu_encode_threading)", SELKIES_CPU_ENCODE_THREADING="false|locked")
check("false|locked pins it off", got == "(False, True)", got)
got = probe("print('cpu_encode_threading' in s.STREAM_SETTINGS)")
check("it is a stream setting a display carries", got == "True", got)

# The shared builder, as both transports call it.
code = (
    "from types import SimpleNamespace as N; import selkies.display_utils as d; "
    "out = []\n"
    "for on in (True, False):\n"
    "    cs = N()\n"
    "    d.apply_common_capture_settings(cs, s.settings, is_wayland=False, display_name='primary', scale=1.0,"
    " framerate=60, encoder='h264enc', use_cpu=True, cbr=True, bitrate_kbps=8000, crf=25, paintover_crf=18,"
    " paintover_burst=5, fullcolor=False, ten_bit=False, streaming=True, use_paint_over_quality=True,"
    " capture_cursor=False, cpu_encode_threading=on)\n"
    "    out.append(cs.cpu_encode_threading)\n"
    "cs = N()\n"
    "d.apply_common_capture_settings(cs, s.settings, is_wayland=False, display_name='primary', scale=1.0,"
    " framerate=60, encoder='h264enc', use_cpu=True, cbr=True, bitrate_kbps=8000, crf=25, paintover_crf=18,"
    " paintover_burst=5, fullcolor=False, ten_bit=False, streaming=True, use_paint_over_quality=True,"
    " capture_cursor=False)\n"
    "out.append(cs.cpu_encode_threading)\n"
    "print(out)")
got = probe(code)
check("the capture builder sets CaptureSettings.cpu_encode_threading, on unless asked off",
      got == "[True, False, True]", got)

ws = open(os.path.join(SRC, "websockets_mode.py")).read()
start = ws.index("STRUCTURAL_CAPTURE_SETTINGS = (")
check("a change rebuilds a WebSockets capture (structural)",
      "cpu_encode_threading" in ws[start:ws.index(")", start)])
check("and restarts its video on a settings change",
      "restart_video_params = ['encoder', 'use_cpu', 'cpu_encode_threading'" in ws)
rtc = open(os.path.join(SRC, "webrtc_mode.py")).read()
pipe = open(os.path.join(SRC, "webrtc_media_pipeline.py")).read()
check("WebRTC applies it per display, restarting the capture",
      '"cpu_encode_threading": lambda p, v: p.set_cpu_encode_threading(bool(v))' in rtc
      and "async def set_cpu_encode_threading" in pipe and "cpu_encode_threading=self.cpu_encode_threading" in pipe)

if not shutil.which("node"):
    check("node available for encodesInSoftware", False, "node not found")
else:
    cases = [
        ("jpeg", None, False, False, False, True),
        ("h264enc-striped", None, False, False, False, True),
        ("h264enc", "gpu", False, False, False, False),
        ("h264enc", "gpu", True, False, False, True),
        ("av1enc", "gpu", False, False, False, True),
        ("h264enc", "vaapi-no-444", False, True, False, True),
        ("h264enc", "vaapi-no-444", False, False, False, False),
        ("h264enc", "none", False, False, False, True),
    ]
    tables = {
        "gpu": {"h264": {"hardware": "nvenc", "software": "x264"}, "av1": {"hardware": None, "software": "svt-av1"}},
        "vaapi-no-444": {"h264": {"hardware": "vaapi", "software": "x264",
                                  "fullcolor": {"hardware": False, "software": True}}},
        "none": None,
    }
    driver = (
        f"import {{ encodesInSoftware }} from {json.dumps(CONDITIONAL)};\n"
        "import { readFileSync } from 'node:fs';\n"
        "const io = JSON.parse(readFileSync(0, 'utf8'));\n"
        "process.stdout.write(JSON.stringify(io.cases.map(([e, t, cpu, fc, tb]) =>"
        " encodesInSoftware(e, t === null ? undefined : io.tables[t] ?? undefined, cpu, fc, tb))));")
    r = subprocess.run(["node", "--input-type=module", "-e", driver], capture_output=True, timeout=60,
                       input=json.dumps({"cases": [c[:5] for c in cases], "tables": tables}).encode())
    got = json.loads(r.stdout or b"[]") if r.returncode == 0 else r.stderr.decode()[-300:]
    for case, answer in zip(cases, got if isinstance(got, list) else [None] * len(cases)):
        encoder, table, cpu, fullcolor, _ten, want = case
        check(f"encodesInSoftware({encoder}, {table}, use_cpu={cpu}, fullcolor={fullcolor}) is {want}",
              answer is want, answer)

print(f"[cpu-threading] {passed}/{passed + failed} passed", flush=True)
sys.exit(0 if failed == 0 else 1)
