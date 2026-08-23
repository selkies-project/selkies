#!/usr/bin/env python3
"""The TURN relay path: a WebRTC client that is only allowed relay candidates
still gets the stream, through a TURN server the selkies settings named.

A coturn `turnserver` is started on loopback with a shared secret; the server
is configured with turn_host/turn_port/turn_shared_secret (and the same host
as its STUN server) so /api/turn hands the page HMAC credentials for it, and
the page is pinned to iceTransportPolicy "relay" through the client's own
turn_switch store key. The checks read RTCPeerConnection.getStats(): the
selected candidate pair's local candidate is a relay on the coturn relay
address, its byte count grows while video decodes, and input typed on the page
(carried by the data channel over that same pair) reaches the X keymap.

Without a turnserver binary the suite skips with the command that enables it:
install coturn (`apt install coturn`, which provides /usr/bin/turnserver) or
point E2E_TURNSERVER at a turnserver binary.

Usage: python3 tests/e2e/test_turn_relay.py
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

SECRET = "e2e-turn-shared-secret"
REALM = "selkies.test"
# Relay allocations come out of this port range; narrow so two runs on one
# host can only collide with each other, never with the servers under test.
RELAY_PORTS = (45000, 45199)

# Pins the client to relay candidates (the wr core reads turn_switch from its
# localStorage namespace) and keeps every RTCPeerConnection reachable.
PAGE_JS = """
  (() => {
    const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
    try { localStorage.setItem(app + '_turn_switch', 'true'); } catch (e) {}
    window.__pcs = [];
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  })();
"""

STATS_JS = """async () => {
  const out = [];
  for (const pc of window.__pcs) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    let selected = null;
    rep.forEach(r => { if (r.type === 'transport' && r.selectedCandidatePairId) selected = r.selectedCandidatePairId; });
    const pairs = [];
    rep.forEach(r => {
      if (r.type !== 'candidate-pair') return;
      if (r.id !== selected && !(r.nominated && r.state === 'succeeded')) return;
      const l = byId[r.localCandidateId] || {}, rm = byId[r.remoteCandidateId] || {};
      pairs.push({selected: r.id === selected, state: r.state, bytesReceived: r.bytesReceived || 0,
                  local: l.candidateType, localAddress: (l.address || l.ip) + ':' + l.port,
                  relayProtocol: l.relayProtocol, remote: rm.candidateType});
    });
    out.push({connection: pc.connectionState, policy: pc.getConfiguration().iceTransportPolicy, pairs});
  }
  return out;
}"""


def turnserver_binary() -> Optional[str]:
    """A runnable turnserver: E2E_TURNSERVER when it names an executable file,
    else one found on PATH. A non-runnable override is treated as absent so the
    suite skips with the install hint rather than crashing on exec."""
    override = os.environ.get("E2E_TURNSERVER")
    if override:
        return override if os.path.isfile(override) and os.access(override, os.X_OK) else None
    return shutil.which("turnserver")


def free_udp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def start_turnserver(binary: str, port: int) -> subprocess.Popen:
    """coturn on loopback with the shared-secret (HMAC) credential scheme.

    Loopback peers are allowed because both ends of this run live on
    127.0.0.1, which coturn refuses to relay to by default; TLS, the CLI port
    and the TCP relay are off so nothing but the one UDP listener binds.
    """
    log = open(os.path.join(H.WORKDIR, "turnserver.log"), "w")
    proc = H.spawn([
        binary, "-n", "-v", "--listening-ip=127.0.0.1", "--relay-ip=127.0.0.1",
        f"--listening-port={port}", f"--min-port={RELAY_PORTS[0]}", f"--max-port={RELAY_PORTS[1]}",
        f"--realm={REALM}", "--use-auth-secret", f"--static-auth-secret={SECRET}",
        "--no-tls", "--no-dtls", "--no-cli", "--no-tcp-relay", "--no-multicast-peers",
        "--allow-loopback-peers", "--fingerprint", "--relay-threads=1",
        "--log-file=stdout", "--simple-log",
        f"--pidfile={os.path.join(H.WORKDIR, 'turnserver.pid')}",
        f"--userdb={os.path.join(H.WORKDIR, 'turnserver.db')}",
    ], stdout=log, stderr=subprocess.STDOUT)
    # A STUN binding answered on the port is the readiness signal.
    deadline = time.time() + 15
    request = bytes.fromhex("000100002112a442") + os.urandom(12)
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"turnserver exited {proc.returncode}; see {log.name}")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.settimeout(0.5)
            try:
                probe.sendto(request, ("127.0.0.1", port))
                data, _ = probe.recvfrom(1024)
                if data[:2] == b"\x01\x01":
                    return proc
            except OSError:
                pass
        time.sleep(0.3)
    proc.terminate()
    raise RuntimeError(f"turnserver did not answer STUN on 127.0.0.1:{port}; see {log.name}")


def stop_turnserver(proc: subprocess.Popen) -> None:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def relay_pairs(stats: list) -> list:
    """The selected pairs across every connection (the nominated, succeeded
    ones where the engine reports no selection)."""
    pairs = [p for s in stats for p in s["pairs"]]
    return [p for p in pairs if p["selected"]] or pairs


def received_bytes(page: Any) -> int:
    return sum(p["bytesReceived"] for p in relay_pairs(page.evaluate(STATS_JS)))


def main() -> None:
    binary = turnserver_binary()
    if not binary:
        H.skip_suite("turn relay: no turnserver binary; install coturn (apt install coturn) "
                     "or set E2E_TURNSERVER=/path/to/turnserver")
    res = H.Results("turn-relay")
    port = free_udp_port()
    turn = start_turnserver(binary, port)
    try:
        H.server_start(mode="webrtc", wayland=False, extra_env={
            "SELKIES_TURN_HOST": "127.0.0.1",
            "SELKIES_TURN_PORT": str(port),
            "SELKIES_TURN_SHARED_SECRET": SECRET,
            "SELKIES_TURN_PROTOCOL": "udp",
            "SELKIES_TURN_TLS": "false",
            "SELKIES_STUN_HOST": "127.0.0.1",
            "SELKIES_STUN_PORT": str(port),
            # The JSON file and a REST endpoint outrank the shared secret when
            # present; neither may reach this run (E2E_TURN_REST_URI included).
            "SELKIES_RTC_CONFIG_JSON": os.path.join(H.WORKDIR, "no-rtc-config.json"),
            "SELKIES_TURN_REST_URI": "",
        })
        res.check("server: HMAC TURN credentials source selected",
                  C.wait_log("Using short-term shared secret HMAC for TURN credentials", timeout=5))
        status, body = H.curl("/api/turn")
        config = json.loads(body) if status == 200 else {}
        turn_entries = [s for s in config.get("iceServers", []) if any(u.startswith("turn:") for u in s.get("urls", []))]
        res.check("/api/turn names the loopback TURN server with HMAC credentials",
                  status == 200 and turn_entries
                  and turn_entries[0]["urls"] == [f"turn:127.0.0.1:{port}?transport=udp"]
                  and ":" in turn_entries[0].get("username", "") and turn_entries[0].get("credential"),
                  f"{status} {turn_entries}")

        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
            ctx.add_init_script(PAGE_JS)
            page = ctx.new_page()
            console_errors = []
            page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
            page.on("pageerror", lambda e: console_errors.append(str(e)))
            page.goto(H.BASE_URL + "/", wait_until="load")
            try:
                info = C.wait_wr_video(page, timeout=60)
                res.check("video: <video> receiving with the client pinned to relay", info is not None, info)
                stats = page.evaluate(STATS_JS)
                res.check("the page's connection runs iceTransportPolicy relay",
                          stats and all(s["policy"] == "relay" for s in stats),
                          [s.get("policy") for s in stats])
                selected = relay_pairs(stats)
                res.check("the selected candidate pair's local candidate is a relay",
                          selected and all(p["local"] == "relay" for p in selected), selected)
                res.check("the relay address is the coturn relay on loopback",
                          selected and all(p["localAddress"].startswith("127.0.0.1:")
                                           and RELAY_PORTS[0] <= int(p["localAddress"].rsplit(":", 1)[1]) <= RELAY_PORTS[1]
                                           for p in selected), [p["localAddress"] for p in selected])
                before = received_bytes(page)
                time.sleep(3.0)
                after = received_bytes(page)
                res.check("bytes keep arriving over the relayed pair", after > before, f"{before} -> {after}")
                # Input rides the data channel on the same transport.
                page.mouse.click(640, 360)
                time.sleep(0.4)
                page.keyboard.down("x")
                time.sleep(0.8)
                pressed = C.x11_keymap_pressed("x")
                page.keyboard.up("x")
                time.sleep(0.3)
                res.check("input: key press through the relay reached the X keymap", pressed is True, pressed)
                res.check("input: key release through the relay", C.x11_keymap_pressed("x") is False)
                turn_log = H.server_log(os.path.join(H.WORKDIR, "turnserver.log"))
                res.check("coturn logged the allocation under the shared-secret realm",
                          "ALLOCATE" in turn_log and REALM in turn_log, "")
                real_errors, _ = C.benign_console(console_errors, [])
                real_errors = [e for e in real_errors if "Wake Lock" not in e]
                res.check("no console errors (filtered)", not real_errors, "; ".join(real_errors)[:160])
            finally:
                browser.close()
    finally:
        H.server_stop()
        stop_turnserver(turn)
    ok = res.summary()
    print(f"\n=== TURN-RELAY: {len(res.items) - len(res.failed())}/{len(res.items)} passed ===")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
