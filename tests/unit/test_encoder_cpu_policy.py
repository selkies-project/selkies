#!/usr/bin/env python3
"""Software-encode policy across encoder switches.

Selecting JPEG (or another encoder with no hardware path) forces software
encoding. That must not outlive the choice: a display moved back to a
hardware-capable encoder has to encode on the GPU again, and a client that
explicitly asked for software encoding has to keep it. Both directions are
checked here because the failure is silent -- the stream keeps working, just on
the CPU.
"""
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.settings import CPU_ONLY_ENCODERS, effective_use_cpu  # noqa: E402


class Results:
    def __init__(self) -> None:
        self.failed = 0

    def check(self, name: str, ok, detail="") -> None:
        if not ok:
            self.failed += 1
        print(("PASS" if ok else "FAIL") + "  [encoder-cpu-policy] {}  {}".format(name, detail),
              flush=True)


def main() -> bool:
    res = Results()

    for enc in CPU_ONLY_ENCODERS:
        res.check("{} forces software encoding".format(enc),
                  effective_use_cpu(enc, False, False) is True)
    res.check("h264enc is not forced to software",
              effective_use_cpu("h264enc", None, False) is False)

    # The round trip that strands a display on the CPU if the jpeg-forced
    # software state latches instead of being re-derived per call.
    requested = None
    state = effective_use_cpu("h264enc", requested, False)
    res.check("starts on hardware", state is False)
    state = effective_use_cpu("jpeg", requested, False)
    res.check("jpeg switches to software", state is True)
    state = effective_use_cpu("h264enc", requested, False)
    res.check("returning to h264enc restores hardware", state is False,
              "a flag that sticks across the encoder change reads True")

    # An explicit client preference survives the same round trip.
    requested = True
    res.check("explicit software choice is kept on h264enc",
              effective_use_cpu("h264enc", requested, False) is True)
    res.check("explicit software choice survives a jpeg detour",
              effective_use_cpu("jpeg", requested, False) is True
              and effective_use_cpu("h264enc", requested, False) is True)

    requested = False
    res.check("explicit hardware choice is honoured over a software default",
              effective_use_cpu("h264enc", requested, True) is False)
    res.check("server default applies when the client never chose",
              effective_use_cpu("h264enc", None, True) is True)

    print("[encoder-cpu-policy] {} check(s) failed".format(res.failed), flush=True)
    return res.failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
