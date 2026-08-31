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
    # unit
    {"path": "unit/test_uinput_abi.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_uinput_policy.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_i18n_keys.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_client_typing.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_keyboard_chords.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_track_transform.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_stripe_clock.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_encode_pace.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_typing.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_ladder.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_precedence.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_stream.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_clipboard_digest.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_app_session.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_rtc_peer_lifecycle.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webrtc_secondary_gate.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webrtc_audio_recovery.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webrtc_reconnect_grace.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_audio_health.py", "tier": "unit", "timeout": 300},
    {"path": "unit/test_audio_worklet.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_control_plane_guards.py", "tier": "unit", "timeout": 300},
    {"path": "unit/test_app_compositor_socket.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_pointer_lock.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_app_commands.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_relative_motion.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_pointer_tracking.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_wayland_gpu_fallback.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_turn_address.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_env_case.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_desktop_session_scripts.py", "tier": "unit", "timeout": 600},
    {"path": "unit/test_package_deps.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_docs_tables.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_https_selfsigned.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_encoder_cpu_policy.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_rate_control_defaults.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_transfer_pacer.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_webrtc_pacer_brake.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_nvml_failfast.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_per_display_settings.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_realized_layout.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_release_version.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_wayland_start_verdict.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_twcc_feedback.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webcam_abi.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webcam_format.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webcam_reformat.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_webcam_orientation.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_keymap_layout_hint.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_keymap_held_carry.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_wayland_reset_queue.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_app_clipboard_poll.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_input_housekeeping.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_session_xwayland.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_typed_keysyms.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_backend_verdict.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_wm_swap.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_secure_routes.py", "tier": "unit", "timeout": 180},
    {"path": "unit/test_clipboard_rearm.py", "tier": "unit", "timeout": 120},
    {"path": "unit/test_audio_control.py", "tier": "unit", "timeout": 120},

    # integration
    {"path": "integration/test_uinput_backend.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_gamepad_release.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_gamepad_switch.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_gamepad_enumeration.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_ack_latency.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_overlay_recycle.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_clipboard_incr.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_protocol.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_apps_gate.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_relative_injection.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_cursor_delivery.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_wayland_session_dpi.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_session_dpi_owner.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_parity_checks.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_seed.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_framerate.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_display_rate_control.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_state_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_switch_stress.py", "tier": "integration", "timeout": 900},
    {"path": "integration/test_webrtc_unix_socket.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_wayland_multi_output.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_wayland_seam.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_wayland_session_screens.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_gpu_probe.py", "tier": "integration", "timeout": 180},
    {"path": "integration/test_two_display_pixels.py", "tier": "integration", "timeout": 600},
    {"path": "integration/test_extended_monitor_outputs.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_retype_case.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_x_connection_leak.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_keymap_parity.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_xwayland_typed_text.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_x11_multigroup.py", "tier": "integration", "timeout": 300},
    {"path": "integration/test_basic_auth_challenge.py", "tier": "integration", "timeout": 120},
    {"path": "integration/test_webcam_device.py", "tier": "integration", "timeout": 600},
    {"path": "packaging/test_packaging.py", "tier": "integration", "timeout": 1800},

    # e2e
    {"path": "e2e/test_ime_composition.py", "tier": "e2e", "timeout": 300},
    {"path": "e2e/test_matrix.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_scroll.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_browsers.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["chromium-ws", "firefox-ws", "webkit-ws", "chromium-wr", "firefox-wr",
                   "striped", "sink"]},
    {"path": "e2e/test_full_color.py", "tier": "e2e", "timeout": 900,
     "selectors": ["ws-chromium", "ws-firefox", "ws-webkit", "wr-chromium", "wr-webkit",
                   "ws-locked", "ws-pinned"]},
    {"path": "e2e/test_dashboards.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["classic", "wish", "gates", "hidpi"]},
    {"path": "e2e/test_clipboard_large.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc"]},
    {"path": "e2e/test_clipboard_image.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc", "wayland"]},
    {"path": "e2e/test_clipboard_pacing.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "wayland", "bloat"]},
    {"path": "e2e/test_cross_transition.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_two_displays.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["websockets-x11", "webrtc-x11", "websockets-wl", "webrtc-wl"]},
    {"path": "e2e/test_constrained_root.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc"]},
    {"path": "e2e/test_gamepad_uinput.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_dpi_accuracy.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_regressions.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["matrix", "switch", "clipboard", "pacer"]},
    {"path": "e2e/test_auth_refresh.py", "tier": "e2e", "timeout": 300},
    {"path": "e2e/test_subfolder.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_pointer_motion.py", "tier": "e2e", "timeout": 1800},
    {"path": "e2e/test_software_decode.py", "tier": "e2e", "timeout": 900,
     "selectors": ["retry", "persisted", "ladder", "healthy", "striped", "nowebcodecs"]},
    {"path": "e2e/test_software_h264.py", "tier": "e2e", "timeout": 600,
     "selectors": ["x11", "wl"]},
    {"path": "e2e/test_webcam.py", "tier": "e2e", "timeout": 900,
     "selectors": ["websockets", "webrtc", "locked", "nowebcodecs", "rotation", "reformat", "detail", "encoderpref"]},
    {"path": "e2e/test_microphone.py", "tier": "e2e", "timeout": 600,
     "selectors": ["websockets", "webrtc"]},
    {"path": "e2e/test_core_parity.py", "tier": "e2e", "timeout": 900,
     "selectors": ["webrtc", "websockets"]},
    {"path": "e2e/test_ws_verdicts.py", "tier": "e2e", "timeout": 900,
     "selectors": ["mk-access", "no-resize"]},
    {"path": "e2e/test_wayland_primary_shrink.py", "tier": "e2e", "timeout": 600},
    {"path": "e2e/test_display_positions.py", "tier": "e2e", "timeout": 900},
    {"path": "e2e/test_cross_display_drag.py", "tier": "e2e", "timeout": 900,
     "selectors": ["x11", "wl"]},
    {"path": "e2e/test_secure_mode.py", "tier": "e2e", "timeout": 900,
     "selectors": ["routes", "websockets", "webrtc", "dashboards", "legacy"]},
    {"path": "e2e/test_file_transfer.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["websockets", "webrtc", "policy"]},
    {"path": "e2e/test_apps_panel.py", "tier": "e2e", "timeout": 600},
    {"path": "e2e/test_dashboard_matrix.py", "tier": "e2e", "timeout": 1800,
     "selectors": ["wish-webrtc-x11", "wish-ws-wl", "wish-webrtc-wl",
                   "gates-webrtc-x11", "gates-ws-wl", "gates-webrtc-wl"]},
    {"path": "e2e/test_browser_backends.py", "tier": "e2e", "timeout": 1200,
     "selectors": ["firefox-ws-wl", "webkit-ws-wl", "firefox-wr-wl", "webkit-wr-x11"]},
    {"path": "e2e/test_turn_relay.py", "tier": "e2e", "timeout": 600},
    {"path": "e2e/test_microphone_audio.py", "tier": "e2e", "timeout": 600,
     "selectors": ["websockets", "webrtc", "locked"]},
    {"path": "e2e/test_pointer_lock.py", "tier": "e2e", "timeout": 600,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_keyboard_layout.py", "tier": "e2e", "timeout": 600,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_encoders.py", "tier": "e2e", "timeout": 900,
     "selectors": ["ws-x11", "wr-x11", "ws-wl", "wr-wl"]},
    {"path": "e2e/test_ws_unix_socket.py", "tier": "e2e", "timeout": 600},

    # on request
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
