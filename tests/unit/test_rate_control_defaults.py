#!/usr/bin/env python3
"""The rate-control default follows the transport and the software encoder.
Websockets streams resolve per encoder (CRF), except that a session known to
encode on OpenH264 — the software H.264 encoder of a GPL-free pixelflux build,
read from pixelflux.SOFTWARE_H264_ENCODER — resolves to CBR; WebRTC streams
resolve to CBR regardless of encoder, and an operator-provided rate_control_mode
or disabled rate control always wins. The same rule must hold at startup for
either mode and across a live transport switch, which re-resolves through
resolve_rate_control_default() exactly as the stream server does. The retired
openh264enc encoder name is accepted as an alias of h264enc wherever an encoder
name enters.
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
               f"SOFTWARE_H264_ENCODER={software_encoder!r}); ")
    out = subprocess.run(
        [sys.executable, "-c", f"{pre}import selkies.settings as s; {code}"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"), **env))
    return out.stdout.strip()


def resolved(software_encoder: str = "", **env: str) -> str:
    return probe("print(s.settings.rate_control_mode)", software_encoder, **env)


for encoder, want in [("h264enc", "crf"), ("h264enc-striped", "crf"), ("jpeg", "crf")]:
    got = resolved(SELKIES_MODE="websockets", SELKIES_ENCODER=encoder)
    check(f"websockets {encoder} defaults to {want}", got == want, got)

got = resolved(SELKIES_MODE="webrtc")
check("webrtc defaults to cbr", got == "cbr", got)

# The software H.264 encoder is a property of the pixelflux build, read from
# pixelflux.SOFTWARE_H264_ENCODER; settings must agree with the installed
# build, including its own fallback: a pixelflux that predates the attribute,
# or no pixelflux at all, reads as the default x264 build.
got = probe("import importlib.util as iu"
            "; enc = getattr(__import__('pixelflux'), 'SOFTWARE_H264_ENCODER', 'x264')"
            " if iu.find_spec('pixelflux') else 'x264'"
            "; print(s.software_h264_encoder() == enc,"
            " s.software_h264_encoder() in ('x264', 'openh264'))")
check("software_h264_encoder() reports the installed pixelflux build", got == "True True", got)

# x264 is quality-driven: the software path keeps the CRF default. OpenH264
# targets a bandwidth: a session known to be on the software path (the striped
# encoder, or h264enc with software encoding forced by use_cpu or gpu_id=-1)
# defaults to CBR, while a hardware-first h264enc — which may still land on the
# CPU, unknowably — keeps CRF.
for encoder, extra, want in [("h264enc", {}, "crf"),
                             ("h264enc", {"SELKIES_USE_CPU": "true"}, "crf"),
                             ("h264enc-striped", {}, "crf")]:
    got = resolved("x264", SELKIES_MODE="websockets", SELKIES_ENCODER=encoder, **extra)
    check(f"x264 build: websockets {encoder} {extra or ''} defaults to {want}", got == want, got)
for encoder, extra, want in [("h264enc", {}, "crf"),
                             ("h264enc", {"SELKIES_USE_CPU": "true"}, "cbr"),
                             ("h264enc", {"SELKIES_GPU_ID": "-1"}, "cbr"),
                             ("h264enc-striped", {}, "cbr"),
                             ("jpeg", {}, "crf")]:
    got = resolved("openh264", SELKIES_MODE="websockets", SELKIES_ENCODER=encoder, **extra)
    check(f"openh264 build: websockets {encoder} {extra or ''} defaults to {want}", got == want, got)
got = resolved("openh264", SELKIES_MODE="websockets", SELKIES_ENCODER="h264enc-striped",
               SELKIES_RATE_CONTROL_MODE="crf")
check("openh264 build: an operator crf pin beats the software-path cbr default", got == "crf", got)
got = probe("print(s.build_client_settings_payload()['software_h264_encoder']['value'])", "openh264")
check("the software encoder is published to clients", got == "openh264", got)

# The retired openh264enc name (a separate OpenH264 choice before software H.264
# became a property of the pixelflux build) still configures a session: it is an
# alias of h264enc for an operator's env/CLI and for a client's stored setting,
# like the historical x264enc.
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
check("openh264enc is no longer a published encoder", got == "h264enc,h264enc-striped,jpeg", got)

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
check("webrtc publishes only its producible encoders", got == "h264enc", got)
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


# Paint-over defaults off under resolved CBR and on under CRF; an operator pin
# of either setting always beats the derivation.
def paintover(**env: str) -> str:
    return probe("print(s.settings.use_paint_over_quality[0])", **env)


check("websockets crf defaults paint-over on", paintover(SELKIES_MODE="websockets") == "True", "")
check("webrtc cbr defaults paint-over off", paintover(SELKIES_MODE="webrtc") == "False", "")
check("operator cbr pin yields paint-over off",
      paintover(SELKIES_MODE="websockets", SELKIES_RATE_CONTROL_MODE="cbr") == "False", "")
check("operator crf pin on webrtc yields paint-over on",
      paintover(SELKIES_MODE="webrtc", SELKIES_RATE_CONTROL_MODE="crf") == "True", "")
check("operator paint-over pin beats the cbr default",
      paintover(SELKIES_MODE="webrtc", SELKIES_USE_PAINT_OVER_QUALITY="true") == "True", "")
check("operator paint-over-off pin survives a crf default",
      paintover(SELKIES_MODE="websockets", SELKIES_USE_PAINT_OVER_QUALITY="false") == "False", "")
check("disabled rate control (forced crf) defaults paint-over on",
      paintover(SELKIES_MODE="webrtc", SELKIES_ENABLE_RATE_CONTROL="false") == "True", "")

got = probe(
    "s.settings.mode = 'webrtc'; s.settings.resolve_rate_control_default();"
    " out = [s.settings.use_paint_over_quality[0]];"
    " s.settings.mode = 'websockets'; s.settings.resolve_rate_control_default();"
    " out.append(s.settings.use_paint_over_quality[0]); print(','.join(str(v) for v in out))",
    SELKIES_MODE="websockets")
check("the live switch flips the paint-over default with the transport", got == "False,True", got)

print(f"[rc-default] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
