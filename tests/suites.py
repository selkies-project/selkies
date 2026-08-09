#!/usr/bin/env python3
"""Registry of the test suites.

Every suite is a standalone program that prints one PASS/FAIL line per check and
exits non-zero if any of them failed, so it can be run directly:

    python3 tests/e2e/test_matrix.py ws-x11

test_suites.py turns this table into pytest cases, one per selector, and CI
selects tiers from it. Tiers describe what a suite needs:

    unit         the source tree only
    integration  an X display or the Wayland backend, PulseAudio, and a
                 selkies install with pixelflux/pcmflux
    e2e          the above plus Playwright browsers and the built web client
    perf         a long constrained-link benchmark, run on request
    soak         the full pixelflux/pcmflux API surface, run on request
"""

SUITES = [
    # --- unit -------------------------------------------------------------
    {"path": "unit/test_uinput_abi.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_uinput_policy.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_i18n_keys.py", "tier": "unit", "timeout": 120},

    # --- integration ------------------------------------------------------
    {"path": "integration/test_uinput_backend.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_gamepad_release.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_gamepad_switch.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_protocol.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_parity_checks.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_seed.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_framerate.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_rate_control.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_state_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_switch_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_webrtc_unix_socket.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_wayland_multi_output.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_two_display_pixels.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_keymap_parity.py", "tier": "integration", "timeout": 300},

    # --- e2e --------------------------------------------------------------
    {"path": "e2e/test_matrix.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_scroll.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_browsers.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["chromium-ws", "firefox-ws", "webkit-ws", "chromium-wr", "firefox-wr"]},
    {"path": "e2e/test_dashboards.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["classic", "wish"]},
    {"path": "e2e/test_clipboard_large.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc"]},
    {"path": "e2e/test_cross_transition.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_two_displays.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_gamepad_uinput.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_regressions.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["matrix", "switch", "clipboard", "pacer"]},
    {"path": "e2e/test_software_decode.py", "tier": "e2e", "timeout": 900,
     "selectors": ["retry", "persisted", "ladder", "healthy", "striped"]},

    # --- on request -------------------------------------------------------
    {"path": "perf/test_pacer.py", "tier": "perf", "timeout": 3600},
    {"path": "soak/test_capture_api.py", "tier": "soak", "timeout": 2400},
    {"path": "soak/test_capture_api_extra.py", "tier": "soak", "timeout": 2400},
]

TIERS = ("unit", "integration", "e2e", "perf", "soak")


def cases(tiers=None):
    """(path, selector, tier, timeout) for every runnable case."""
    for suite in SUITES:
        if tiers and suite["tier"] not in tiers:
            continue
        for selector in suite.get("selectors", [None]):
            yield suite["path"], selector, suite["tier"], suite["timeout"]
