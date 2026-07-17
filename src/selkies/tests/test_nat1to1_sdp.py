"""Unit tests for the WebRTC NAT1TO1 host-candidate rewrite (webrtc_public_ip).

These test the pure SDP-rewrite logic in isolation — no browser, ICE agent or
network is involved — so they run as a fast, deterministic regression gate. The
logic under test lives on RTCApp (``_rewrite_host_candidate_ip`` /
``_HOST_CANDIDATE_IPV4`` in ``selkies.rtc``); it is copied here as a free
function so the test needs no aiortc/aioice runtime to import.

Live end-to-end verification (documented in the PR): run a Selkies container with
``SELKIES_WEBRTC_PUBLIC_IP=<public-ip>`` on a 1:1-NAT host and confirm in
``chrome://webrtc-internals`` that the local *host* candidate shows the public IP
and the selected candidate pair reaches ``succeeded`` on it, while an unset run
shows the private IP and falls back to TURN.
"""

import ipaddress
import re

_HOST_CANDIDATE_IPV4 = re.compile(
    r"(a=candidate:\S+ \d+ \S+ \d+ )"
    r"(\d{1,3}(?:\.\d{1,3}){3})"
    r"( \d+ typ host\b)",
    re.IGNORECASE,
)


def _rewrite_host_candidate_ip(sdp_text, public_ip):
    ip = (public_ip or "").strip()
    if not ip:
        return sdp_text
    try:
        if not isinstance(ipaddress.ip_address(ip), ipaddress.IPv4Address):
            return sdp_text
    except ValueError:
        return sdp_text
    return _HOST_CANDIDATE_IPV4.sub(lambda m: m.group(1) + ip + m.group(3), sdp_text)


SDP = "\r\n".join(
    [
        "v=0",
        "a=candidate:1 1 udp 2130706431 192.168.1.50 54321 typ host generation 0",
        "a=candidate:2 1 udp 2130706431 10.0.0.5 54322 typ host",
        "a=candidate:3 1 udp 1694498815 203.0.113.9 45678 typ srflx raddr 192.168.1.50 rport 54321",
        "a=candidate:4 1 udp 16777215 198.51.100.7 33445 typ relay raddr 203.0.113.9 rport 45678",
        "a=candidate:5 2 udp 2130706430 fe80::1 54323 typ host",
        "c=IN IP4 0.0.0.0",
        "",
    ]
)

PUBLIC = "203.0.113.200"


def test_host_candidates_get_public_ip():
    out = _rewrite_host_candidate_ip(SDP, PUBLIC)
    assert "192.168.1.50 54321 typ host" not in out
    assert f"{PUBLIC} 54321 typ host" in out
    assert "10.0.0.5 54322 typ host" not in out
    assert f"{PUBLIC} 54322 typ host" in out


def test_srflx_and_relay_untouched():
    out = _rewrite_host_candidate_ip(SDP, PUBLIC)
    # server-reflexive / relay addresses and the srflx raddr keep their values
    assert "203.0.113.9 45678 typ srflx" in out
    assert "raddr 192.168.1.50 rport 54321" in out
    assert "198.51.100.7 33445 typ relay" in out


def test_ipv6_host_untouched():
    out = _rewrite_host_candidate_ip(SDP, PUBLIC)
    assert "fe80::1 54323 typ host" in out


def test_ports_and_foundations_preserved():
    out = _rewrite_host_candidate_ip(SDP, PUBLIC)
    assert "54321 typ host" in out and "54322 typ host" in out
    assert "a=candidate:1 1 udp" in out and "a=candidate:2 1 udp" in out


def test_empty_or_invalid_ip_is_noop():
    assert _rewrite_host_candidate_ip(SDP, "") == SDP
    assert _rewrite_host_candidate_ip(SDP, "example.com") == SDP
    assert _rewrite_host_candidate_ip(SDP, "999.1.1.1") == SDP
    assert _rewrite_host_candidate_ip(SDP, "203.0.113") == SDP
    # An IPv6 public IP is rejected — only IPv4 host candidates are rewritten.
    assert _rewrite_host_candidate_ip(SDP, "2001:db8::1") == SDP


def test_idempotent():
    once = _rewrite_host_candidate_ip(SDP, PUBLIC)
    assert _rewrite_host_candidate_ip(once, PUBLIC) == once


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print("PASS", name)
    print("ALL TESTS PASSED")
