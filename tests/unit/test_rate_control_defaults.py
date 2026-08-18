#!/usr/bin/env python3
"""The rate-control default follows the transport. Websockets streams resolve
per encoder (CRF everywhere but openh264enc), WebRTC streams resolve to CBR
regardless of encoder, and an operator-provided rate_control_mode or disabled
rate control always wins. The same rule must hold at startup for either mode
and across a live transport switch, which re-resolves through
resolve_rate_control_default() exactly as the stream server does.
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
    print(f"{'PASS' if ok else 'FAIL'}  [rc-default] {label}  {detail}", flush=True)


# The settings singleton reads argv and SELKIES_* environment variables at
# import, so every scenario is a fresh interpreter with only its own variables.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}


def probe(code: str, **env: str) -> str:
    """Run `code` against a freshly imported settings module; stripped stdout."""
    out = subprocess.run(
        [sys.executable, "-c", f"import selkies.settings as s; {code}"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"), **env))
    return out.stdout.strip()


def resolved(**env: str) -> str:
    return probe("print(s.settings.rate_control_mode)", **env)


for encoder, want in [("h264enc", "crf"), ("openh264enc", "cbr"),
                      ("h264enc-striped", "crf"), ("jpeg", "crf")]:
    got = resolved(SELKIES_MODE="websockets", SELKIES_ENCODER=encoder)
    check(f"websockets {encoder} defaults to {want}", got == want, got)

for env in ({}, {"SELKIES_ENCODER_RTC": "openh264enc"},
            {"SELKIES_ENCODER_RTC": "h264enc"}):
    got = resolved(SELKIES_MODE="webrtc", **env)
    check(f"webrtc defaults to cbr ({env or 'default encoder'})", got == "cbr", got)

# The mode comparison runs on the normalized transport name, so an operator's
# casing must not silently swap which default applies.
got = resolved(SELKIES_MODE="WebRTC")
check("mixed-case webrtc mode still defaults to cbr", got == "cbr", got)
got = resolved(SELKIES_MODE="WebSockets")
check("mixed-case websockets mode still defaults to crf", got == "crf", got)

got = resolved(SELKIES_MODE="webrtc", SELKIES_RATE_CONTROL_MODE="crf")
check("operator crf pin beats the webrtc cbr default", got == "crf", got)
got = resolved(SELKIES_MODE="websockets", SELKIES_RATE_CONTROL_MODE="cbr")
check("operator cbr pin beats the websockets crf default", got == "cbr", got)

got = resolved(SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false",
               SELKIES_RATE_CONTROL_MODE="cbr")
check("disabled rate control forces crf on webrtc too", got == "crf", got)
got = probe(
    "print(next(d for d in s.settings._setting_definitions"
    " if d['name'] == 'rate_control_mode')['meta']['allowed'])",
    SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false")
check("disabled rate control publishes a crf-only menu", got == "['crf']", got)

# The live transport switch: the stream server rewrites mode and re-resolves,
# so an unpinned rate control follows the transport actually streaming.
got = probe(
    "s.settings.mode = 'webrtc'; s.settings.resolve_rate_control_default();"
    " out = [s.settings.rate_control_mode];"
    " s.settings.mode = 'websockets'; s.settings.resolve_rate_control_default();"
    " out.append(s.settings.rate_control_mode); print(','.join(out))",
    SELKIES_MODE="websockets")
check("a live switch re-resolves cbr then back to crf", got == "cbr,crf", got)

got = probe(
    "s.settings.mode = 'webrtc'; s.settings.resolve_rate_control_default();"
    " print(s.settings.rate_control_mode)",
    SELKIES_MODE="websockets", SELKIES_RATE_CONTROL_MODE="crf")
check("a live switch never overwrites an operator pin", got == "crf", got)

print(f"[rc-default] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
