#!/usr/bin/env python3
"""The encoder menu a client is offered lists what this host serves.

At startup the server resolves, once, which backend serves each codec: the
software encoder of the installed pixelflux build and the hardware encoder of
the encode node the capture settings resolve, and narrows the operator's menu
to the encoders whose codec one of the two serves, so a client is never offered
an encoder the selection ladder would demote. The resolved table reaches the
clients as `encoder_backends`, from which the dashboards show the software
encoding switch only where a codec has both backends. A host that never
encodes on hardware (gpu_id -1, software encoding locked on) has no hardware
side to probe, so that path runs on any machine; the probed path is held to
its shape wherever the installed pixelflux carries the probe.
"""
import os
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [encoder-backends] {label}  {detail}", flush=True)


# The settings singleton reads argv and SELKIES_* environment variables at
# import, so every scenario is a fresh interpreter with only its own variables.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}


def probe(code: str, **env: str) -> str:
    """Run `code` against a freshly imported settings module; stripped stdout."""
    out = subprocess.run(
        [sys.executable, "-c", f"import selkies.settings as s; {code}"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"), **env))
    if out.returncode != 0:
        return f"exit {out.returncode}: {out.stderr.strip().splitlines()[-1:]}"
    return out.stdout.strip()


MENU = ("','.join(next(d for d in s.settings._setting_definitions"
        " if d['name'] == 'encoder')['meta']['allowed'])")
SOFTWARE_ONLY_MENU = (
    "sw = s.software_encoders();"
    f" menu = [e for e in ({MENU}).split(',') if e in s.CPU_ONLY_ENCODERS or s.codec_for_encoder(e) in sw];"
    " print(','.join(menu))")

# The encode node the probe opens follows the capture settings' own resolution.
check("no explicit pick encodes on the first node",
      probe("print(s.settings.encode_node_index())") == "0")
check("gpu_id picks the node", probe("print(s.settings.encode_node_index())", SELKIES_GPU_ID="1") == "1")
check("gpu_id -1 has no node", probe("print(s.settings.encode_node_index())", SELKIES_GPU_ID="-1") == "None")
check("encode_dri names the node",
      probe("print(s.settings.encode_node_index())", SELKIES_ENCODE_DRI="/dev/dri/renderD130") == "2")
check("software encoding locked on has no node",
      probe("print(s.settings.encode_node_index())", SELKIES_USE_CPU="true|locked") == "None")

# Before the startup resolution nothing is known and nothing is narrowed.
check("unresolved: the table is unknown", probe("print(s.settings.encoder_backends())") == "None")
check("unresolved: the payload carries no table",
      probe("print('encoder_backends' in s.build_client_settings_payload())") == "False")
check("unresolved: every encoder is served",
      probe("print(all(s.settings.encoder_served(e) for e in s.ENCODER_CODECS))") == "True")

# A host that never encodes on hardware resolves without a probe, so this path
# runs on any machine: the table names the build's software encoders and no
# hardware, and the menu keeps only the encoders with a software path.
got = probe(
    "s.settings.resolve_encoder_backends(); t = s.settings.encoder_backends(); sw = s.software_encoders();"
    " print(sorted(t) == ['av1', 'h264', 'h265', 'vp8', 'vp9'],"
    " all(v['hardware'] is None for v in t.values()),"
    " all(t[c]['software'] == sw.get(c) for c in t))", SELKIES_GPU_ID="-1")
check("gpu_id -1: the table is the build's software side and no hardware", got == "True True True", got)
expected = probe(SOFTWARE_ONLY_MENU, SELKIES_GPU_ID="-1")
got = probe(f"s.settings.resolve_encoder_backends(); print({MENU})", SELKIES_GPU_ID="-1")
check("gpu_id -1: the menu keeps the encoders with a software path", got == expected, f"{got} vs {expected}")
got = probe("s.settings.resolve_encoder_backends(); p = s.build_client_settings_payload();"
            " print(p['encoder_backends']['value'] == s.settings.encoder_backends())", SELKIES_GPU_ID="-1")
check("gpu_id -1: the payload publishes the resolved table", got == "True", got)
got = probe(f"s.settings.resolve_encoder_backends(); print({MENU})", SELKIES_GPU_ID="-1", SELKIES_ENCODER="vp9enc,jpeg")
check("gpu_id -1: an operator menu is narrowed, not replaced",
      got == ",".join(e for e in ("vp9enc", "jpeg") if e in expected.split(",")), got)
check("gpu_id -1: the CPU-only encoders are always served",
      probe("s.settings.resolve_encoder_backends(); print(s.settings.encoder_served('jpeg'),"
            " s.settings.encoder_served('h264enc-striped'))", SELKIES_GPU_ID="-1") == "True True")

# WebRTC narrows the resolved menu to what it can carry, and a switch back to
# websockets restores the narrowed menu rather than the shipped one.
got = probe(f"s.settings.resolve_encoder_backends(); print({MENU})", SELKIES_GPU_ID="-1", SELKIES_MODE="webrtc")
check("gpu_id -1, webrtc: the menu is the served WebRTC encoders",
      got == ",".join(e for e in expected.split(",") if e in ("h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc")), got)
got = probe(
    "s.settings.resolve_encoder_backends(); s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
    f" s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter(); print({MENU})", SELKIES_GPU_ID="-1")
check("gpu_id -1: a transport round trip keeps the narrowed menu", got == expected, got)

# The probed path opens the encode node, which a CI runner has none of: the
# table is then unknown (a pixelflux without the probe) or well formed, with
# each hardware entry a backend name and the menu inside what is served.
got = probe(
    "s.settings.resolve_encoder_backends(); t = s.settings.encoder_backends();"
    " print('unknown' if t is None else ("
    " all(v['hardware'] in (None, 'nvenc', 'vaapi') for v in t.values())"
    f" and all(s.settings.encoder_served(e) for e in ({MENU}).split(','))))")
check("probed: the table is unknown or well formed and the menu is served", got in ("unknown", "True"), got)

print(f"[encoder-backends] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
