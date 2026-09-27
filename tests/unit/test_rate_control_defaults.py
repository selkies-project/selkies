#!/usr/bin/env python3
"""The rate-control default is CBR on both transports, for every encoder and
either software H.264 encoder of a pixelflux build (read from
pixelflux.SOFTWARE_ENCODERS), and paint-over is on whatever Turbo, the rate
control, and the encoder; an operator-provided rate_control_mode or
use_paint_over_quality, or disabled rate control, always wins. The same holds
at startup for either mode and across a live transport switch, which rewrites
the mode and refilters the encoder exactly as the stream server does. The
retired openh264enc encoder name is accepted as an alias of h264enc wherever an
encoder name enters.
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


def probe(code: str, software_encoder: str = "", **env: str) -> str:
    """Run `code` against a freshly imported settings module; stripped stdout.

    `software_encoder` stands a stub pixelflux module in the interpreter before
    settings imports, so the build-resolved encoder can be tried both ways here
    (a GPL-free pixelflux build is not something this machine has installed).
    """
    pre = ""
    if software_encoder:
        pre = ("import sys, types; sys.modules['pixelflux'] = types.SimpleNamespace("
               f"SOFTWARE_ENCODERS={{'h264': {software_encoder!r}, 'vp8': 'libvpx', 'av1': 'svt-av1'}}); ")
    out = subprocess.run(
        [sys.executable, "-c", f"{pre}import selkies.settings as s; {code}"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"), **env))
    return out.stdout.strip()


def resolved(software_encoder: str = "", **env: str) -> str:
    return probe("print(s.settings.rate_control_mode)", software_encoder, **env)


for build in ("x264", "openh264"):
    for encoder in ("h264enc", "h264enc-striped", "jpeg", "h265enc", "vp8enc", "vp9enc", "av1enc"):
        got = resolved(build, SELKIES_MODE="websockets", SELKIES_ENCODER=encoder)
        check(f"{build} build: websockets {encoder} defaults to cbr", got == "cbr", got)
    for extra in ({"SELKIES_USE_CPU": "true"}, {"SELKIES_GPU_ID": "-1"}):
        got = resolved(build, SELKIES_MODE="websockets", SELKIES_ENCODER="h264enc", **extra)
        check(f"{build} build: websockets h264enc {extra} defaults to cbr", got == "cbr", got)

got = resolved(SELKIES_MODE="webrtc")
check("webrtc defaults to cbr", got == "cbr", got)

# The software encoders are a property of the pixelflux build, read from
# pixelflux.SOFTWARE_ENCODERS; settings must agree with the installed build,
# and with the x264 default it falls back to where there is no extension to
# read.
got = probe("import importlib.util as iu"
            "; enc = __import__('pixelflux').SOFTWARE_ENCODERS['h264']"
            " if iu.find_spec('pixelflux') else 'x264'"
            "; print(s.software_encoders()['h264'] == enc,"
            " s.software_encoders()['h264'] in ('x264', 'openh264'))")
check("software_encoders() reports the installed pixelflux build", got == "True True", got)

got = resolved("openh264", SELKIES_MODE="websockets", SELKIES_ENCODER="h264enc-striped",
               SELKIES_RATE_CONTROL_MODE="crf")
check("openh264 build: an operator crf pin beats the cbr default", got == "crf", got)

# openh264enc and x264enc are aliases of h264enc for an operator's env/CLI and
# for a client's stored setting; neither is a published encoder.
got = probe("print(s.settings.encoder)", SELKIES_MODE="websockets", SELKIES_ENCODER="openh264enc")
check("an operator's openh264enc becomes h264enc", got == "h264enc", got)
got = probe("print(s.settings.encoder)", SELKIES_MODE="websockets", SELKIES_ENCODER="x264enc")
check("an operator's x264enc becomes h264enc", got == "h264enc", got)
got = probe(
    "import logging;"
    " print(s.sanitize_client_setting('encoder', 'openh264enc', s.settings, logging),"
    " s.sanitize_client_setting('encoder', 'X264ENC', s.settings, logging))",
    SELKIES_MODE="websockets")
check("a client's stored openh264enc/x264enc sanitize to h264enc", got == "h264enc h264enc", got)
got = probe(
    "print(','.join(next(d for d in s.settings._setting_definitions"
    " if d['name'] == 'encoder')['meta']['allowed']))",
    SELKIES_MODE="websockets")
check("openh264enc is not a published encoder",
      got == "h264enc,h265enc,vp8enc,vp9enc,av1enc,h264enc-striped,jpeg", got)

# One encoder knob, both transports: in webrtc mode a websockets-only choice
# falls back to the default and the published menu is filtered; switching back
# restores the operator's menu and value (neither a websockets capability nor
# an operator narrowing/pin is lost to a round trip).
ENCODER_MENU = (
    "','.join(next(d for d in s.settings._setting_definitions"
    " if d['name'] == 'encoder')['meta']['allowed'])"
)
got = probe(
    "print(s.settings.encoder)", SELKIES_MODE="webrtc", SELKIES_ENCODER="jpeg")
check("webrtc boot falls a websockets-only encoder back to the default", got == "h264enc", got)
got = probe(
    "print(s.settings.encoder)", SELKIES_MODE="webrtc", SELKIES_ENCODER="h264enc")
check("webrtc keeps a valid operator encoder", got == "h264enc", got)
got = probe(
    f"print({ENCODER_MENU})",
    SELKIES_MODE="webrtc")
check("webrtc publishes only its producible encoders", got == "h264enc,h265enc,vp8enc,vp9enc,av1enc", got)
got = probe(
    f"print({ENCODER_MENU})",
    SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg")
check("an operator encoder pin narrows the published menu", got == "jpeg", got)
got = probe(
    f"print({ENCODER_MENU})",
    SELKIES_MODE="webrtc", SELKIES_ENCODER="h264enc")
check("an operator pin producible on webrtc stays locked there", got == "h264enc", got)
got = probe(
    "import logging;"
    " print(s.sanitize_client_setting('encoder', 'h264enc', s.settings, logging))",
    SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg")
check("a client cannot escape an operator encoder pin", got == "jpeg", got)
got = probe(
    "s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
    " out = [s.settings.encoder];"
    " s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter();"
    f" out.append(s.settings.encoder); out.append({ENCODER_MENU});"
    " print('|'.join(out))",
    SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg")
check("a live switch clamps, and switching back restores the pin and menu",
      got == "h264enc|jpeg|jpeg", got)

# Client picks write through to the singleton (the transports re-seed from it
# on a mode switch): a websockets-only pick clamped by the webrtc leg comes
# back on the switch back, unless the client asserted something newer during
# that leg — a fresh pick always outranks the stash.
got = probe(
    "s.settings.encoder = 'jpeg'; s.settings._encoder_client_set = True;"
    " s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
    " out = [s.settings.encoder];"
    " s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter();"
    " out.append(s.settings.encoder); print('|'.join(out))",
    SELKIES_MODE="websockets")
check("a client's websockets-only pick survives a webrtc round trip",
      got == "h264enc|jpeg", got)
got = probe(
    "s.settings.encoder = 'jpeg'; s.settings._encoder_client_set = True;"
    " s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
    " s.settings.encoder = 'h264enc'; s.settings._encoder_client_set = True;"
    " s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter();"
    " out = [s.settings.encoder];"
    " s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
    " out.append(s.settings.encoder);"
    " s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter();"
    " out.append(s.settings.encoder); print('|'.join(out))",
    SELKIES_MODE="websockets")
check("a fresh pick during the webrtc leg wins and never resurrects the stash",
      got == "h264enc|h264enc|h264enc", got)

got = resolved(SELKIES_MODE="webrtc", SELKIES_RATE_CONTROL_MODE="crf")
check("operator crf pin beats the webrtc cbr default", got == "crf", got)
got = resolved(SELKIES_MODE="websockets", SELKIES_RATE_CONTROL_MODE="crf")
check("operator crf pin beats the websockets cbr default", got == "crf", got)

got = resolved(SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false",
               SELKIES_RATE_CONTROL_MODE="cbr")
check("disabled rate control forces crf on webrtc too", got == "crf", got)
got = probe(
    "print(next(d for d in s.settings._setting_definitions"
    " if d['name'] == 'rate_control_mode')['meta']['allowed'])",
    SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false")
check("disabled rate control publishes a crf-only menu", got == "['crf']", got)

# The live transport switch, as the stream server makes it: the mode is
# rewritten and the encoder refiltered, and the rate control stays what it was.
SWITCH = ("s.settings.mode = 'webrtc'; s.settings.apply_webrtc_encoder_filter();"
          " out = [s.settings.rate_control_mode];"
          " s.settings.mode = 'websockets'; s.settings.apply_webrtc_encoder_filter();"
          " out.append(s.settings.rate_control_mode); print(','.join(out))")
got = probe(SWITCH, SELKIES_MODE="websockets")
check("a live switch keeps cbr both ways", got == "cbr,cbr", got)
got = probe(SWITCH, SELKIES_MODE="websockets", SELKIES_RATE_CONTROL_MODE="crf")
check("and never overwrites an operator pin", got == "crf,crf", got)


# Paint-over cleans up a still screen whatever Turbo sends and whatever the rate
# control, so it is on by default everywhere; an operator's choice stands.
def paintover(**env: str) -> str:
    return probe("print(s.settings.use_paint_over_quality[0])", **env)


for mode in ("websockets", "webrtc"):
    check(f"{mode} defaults paint-over on under Turbo", paintover(SELKIES_MODE=mode) == "True", "")
    check(f"{mode} defaults it on without Turbo",
          paintover(SELKIES_MODE=mode, SELKIES_VIDEO_STREAMING_MODE="false") == "True", "")
    check(f"{mode} defaults it on under crf",
          paintover(SELKIES_MODE=mode, SELKIES_RATE_CONTROL_MODE="crf") == "True", "")
    check(f"{mode} keeps an operator's paint-over off under Turbo",
          paintover(SELKIES_MODE=mode, SELKIES_USE_PAINT_OVER_QUALITY="false") == "False", "")
    check(f"{mode} keeps an operator's paint-over off without Turbo",
          paintover(SELKIES_MODE=mode, SELKIES_VIDEO_STREAMING_MODE="false",
                    SELKIES_USE_PAINT_OVER_QUALITY="false") == "False", "")
check("jpeg defaults paint-over on",
      paintover(SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg") == "True", "")
check("disabled rate control (forced crf) leaves paint-over on",
      paintover(SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false") == "True", "")

# A switch to webrtc clamps jpeg to a video encoder and the switch back restores
# jpeg; paint-over stays what it was across both.
PAINT_SWITCH = ("out = []"
                "\nfor mode in ('webrtc', 'websockets'):"
                "\n    s.settings.mode = mode; s.settings.apply_webrtc_encoder_filter()"
                "\n    out.append(f'{s.settings.encoder}:{s.settings.use_paint_over_quality[0]}')"
                "\nprint(','.join(out))")
got = probe(PAINT_SWITCH, SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg")
check("a live switch keeps paint-over on both ways", got == "h264enc:True,jpeg:True", got)
got = probe(PAINT_SWITCH, SELKIES_MODE="websockets", SELKIES_ENCODER="jpeg",
            SELKIES_USE_PAINT_OVER_QUALITY="false")
check("and never overwrites an operator's paint-over", got == "h264enc:False,jpeg:False", got)

print(f"[rc-default] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
