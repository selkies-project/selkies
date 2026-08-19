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
from typing import Iterator, Optional, Sequence

SUITES: list = [
    # --- unit -------------------------------------------------------------
    {"path": "unit/test_uinput_abi.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_uinput_policy.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_i18n_keys.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_client_typing.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_typing.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_ladder.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_app_compositor_socket.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_pointer_lock.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_wayland_gpu_fallback.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_turn_address.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_env_case.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_example_session_scripts.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_encoder_cpu_policy.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_rate_control_defaults.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_transfer_pacer.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_nvml_failfast.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_per_display_settings.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_release_version.py", "tier": "unit", "timeout": 120},

    # --- integration ------------------------------------------------------
    {"path": "integration/test_uinput_backend.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_gamepad_release.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_gamepad_switch.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_protocol.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_wayland_session_dpi.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_parity_checks.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_seed.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_framerate.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_rate_control.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_state_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_switch_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_webrtc_unix_socket.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_wayland_multi_output.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_gpu_probe.py", "tier": "integration", "timeout": 180},
    {"path": "integration/test_two_display_pixels.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_retype_case.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_x_connection_leak.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_keymap_parity.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_basic_auth_challenge.py", "tier": "integration", "timeout": 120},

    # --- e2e --------------------------------------------------------------
    {"path": "e2e/test_ime_composition.py", "tier": "e2e", "timeout": 300},
    {"path": "e2e/test_matrix.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_scroll.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_browsers.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["chromium-ws", "firefox-ws", "webkit-ws", "chromium-wr", "firefox-wr"]},
    {"path": "e2e/test_dashboards.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["classic", "wish", "gates"]},
    {"path": "e2e/test_clipboard_large.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc"]},
    {"path": "e2e/test_cross_transition.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_two_displays.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_gamepad_uinput.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_dpi_accuracy.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_regressions.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["matrix", "switch", "clipboard", "pacer"]},
    {"path": "e2e/test_auth_refresh.py", "tier": "e2e", "timeout": 300},
    {"path": "e2e/test_subfolder.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_software_decode.py", "tier": "e2e", "timeout": 900,
     "selectors": ["retry", "persisted", "ladder", "healthy", "striped"]},

    # --- on request -------------------------------------------------------
    {"path": "perf/test_pacer.py", "tier": "perf", "timeout": 3600},
    {"path": "perf/test_transfer_saturation.py", "tier": "perf", "timeout": 1800},
    {"path": "soak/test_capture_api.py", "tier": "soak", "timeout": 2400},
    {"path": "soak/test_capture_api_extra.py", "tier": "soak", "timeout": 2400},
]

TIERS: tuple = ("unit", "integration", "e2e", "perf", "soak")


def cases(tiers: Optional[Sequence[str]] = None) -> Iterator[tuple]:
    """Yield `(path, selector, tier, timeout)` for every runnable case.

    Args:
        tiers: Restrict to these tiers; None yields every suite. A suite with
            no selectors yields one case whose selector is None.
    """
    for suite in SUITES:
        if tiers and suite["tier"] not in tiers:
            continue
        for selector in suite.get("selectors", [None]):
            yield suite["path"], selector, suite["tier"], suite["timeout"]
