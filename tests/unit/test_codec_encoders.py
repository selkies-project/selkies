#!/usr/bin/env python3
"""The encoder names and the pixelflux codecs behind them.

Every published encoder selects a pixelflux codec, an alias or an unknown name
lands on H.264 rather than taking a capture down, the striped encoder and JPEG
keep their framing, and the software-path rule follows the encoder: the striped
encoder is always software, a full-frame one only when software is forced.
"""
import os
import sys

# The settings singleton reads SELKIES_* at import; an operator pin in the
# shell running the tests would narrow the published menu under test.
for key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[key]

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.settings import (  # noqa: E402
    ENCODER_CODECS, SETTING_DEFINITIONS, WEBRTC_ENCODER_CHOICES, codec_for_encoder,
    software_video_path,
)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [codec-encoders] {label}  {detail}", flush=True)


allowed = next(d for d in SETTING_DEFINITIONS if d["name"] == "encoder")["meta"]["allowed"]
check("every published encoder selects a codec", all(e in ENCODER_CODECS for e in allowed), allowed)
check("every mapped encoder is published", set(ENCODER_CODECS) == set(allowed), sorted(ENCODER_CODECS))
for encoder, codec in [("jpeg", "jpeg"), ("h264enc", "h264"), ("h264enc-striped", "h264"),
                       ("h265enc", "h265"), ("vp8enc", "vp8"), ("vp9enc", "vp9"), ("av1enc", "av1")]:
    got = codec_for_encoder(encoder)
    check(f"{encoder} selects {codec}", got == codec, got)
check("an alias selects H.264", codec_for_encoder("openh264enc") == "h264", codec_for_encoder("openh264enc"))
check("an unknown encoder selects H.264", codec_for_encoder("nvh264enc") == "h264", codec_for_encoder("nvh264enc"))
check("webrtc offers only mapped encoders", all(e in ENCODER_CODECS for e in WEBRTC_ENCODER_CHOICES),
      WEBRTC_ENCODER_CHOICES)

check("jpeg is never a software video path", software_video_path("jpeg", True) is False)
check("the striped encoder is always software", software_video_path("h264enc-striped", False) is True)
for encoder in ("h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc"):
    check(f"{encoder} is software only when forced",
          software_video_path(encoder, False) is False and software_video_path(encoder, True) is True)

print(f"{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
