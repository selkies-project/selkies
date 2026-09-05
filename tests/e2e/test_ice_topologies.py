#!/usr/bin/env python3
"""The ICE topologies end to end: a browser connects through each of them.

Each block starts a WebRTC server with one topology setting and drives
Chromium, Firefox and WebKit through Playwright, reading what the browser's
own RTCPeerConnection.getStats() says about the nominated candidate pair.
The port range confines the server's candidate to its window; the UDP mux
puts every session, a controller and a shared viewer at once, on the one
port; the TCP mux carries a session whose UDP candidates were stripped from
the offer, as a network that blocks UDP would, while an unstripped session
still prefers UDP; ICE-lite makes the browser take the controlling role of an
`a=ice-lite` offer; and the combined block runs all of them together, on
the Wayland backend too since the transport owes the capture nothing. Over
every path video decodes (WebKit's decoder permitting) and, on X11, a key
typed on the page, carried by the data channel on the same pair, reaches the
X keymap.

    python3 tests/e2e/test_ice_topologies.py portrange|udpmux|tcpmux|icelite|combined|combined-wl|all
"""
import os
import socket
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_browsers as TB
import test_browser_backends as BB
from playwright.sync_api import sync_playwright

ENGINES = ("chromium", "firefox", "webkit")

# Keeps every RTCPeerConnection reachable, records the remote SDP, and with
# window.__stripUdp set withholds UDP candidates in both directions, the
# offer's from the browser and the browser's own from the trickle, the way a
# network that blocks UDP leaves both agents: with only the offer stripped,
# the server, as the controlling agent, still checks the browser's UDP
# candidates and Chromium nominates the peer-reflexive UDP pair it discovers.
PAGE_JS = """
  (() => {
    window.__pcs = [];
    window.__remoteSdp = '';
    const strip = !!window.__stripUdp;
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = function(desc) {
      let sdp = desc && desc.sdp ? desc.sdp : '';
      window.__remoteSdp = sdp;
      if (strip && sdp) {
        sdp = sdp.split('\\r\\n').filter(l => !(l.startsWith('a=candidate:') && / udp /i.test(l))).join('\\r\\n');
        desc = {type: desc.type, sdp};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = function(c) {
      if (strip && c && c.candidate && / udp /i.test(c.candidate)) return Promise.resolve();
      return addCandidate.call(this, c);
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) {
        handler.set.call(this, (e) => {
          if (strip && e.candidate && / udp /i.test(e.candidate.candidate || '')) return;
          return fn(e);
        });
      },
    });
  })();
"""

STATS_JS = """async () => {
  const out = [];
  for (const pc of window.__pcs) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    let selected = null, iceRole = null;
    rep.forEach(r => {
      if (r.type !== 'transport') return;
      if (r.selectedCandidatePairId) selected = r.selectedCandidatePairId;
      if (r.iceRole) iceRole = r.iceRole;
    });
    const pairs = [];
    rep.forEach(r => {
      if (r.type !== 'candidate-pair') return;
      if (r.id !== selected && !(r.nominated && r.state === 'succeeded')) return;
      const l = byId[r.localCandidateId] || {}, rm = byId[r.remoteCandidateId] || {};
      pairs.push({selected: r.id === selected, state: r.state, bytesReceived: r.bytesReceived || 0,
                  localType: l.candidateType, localProtocol: l.protocol,
                  remoteType: rm.candidateType, remoteProtocol: rm.protocol,
                  remoteAddress: rm.address || rm.ip || '', remotePort: rm.port, remoteTcpType: rm.tcpType});
    });
    out.push({connection: pc.connectionState, iceRole, pairs, remoteSdp: window.__remoteSdp});
  }
  return out;
}"""


def free_port(kind: int) -> int:
    with socket.socket(socket.AF_INET, kind) as probe:
        probe.bind(("0.0.0.0", 0))
        return probe.getsockname()[1]


def free_port_both() -> int:
    """A port free for UDP and TCP alike, as one shared mux number wants."""
    for _ in range(50):
        port = free_port(socket.SOCK_DGRAM)
        with socket.socket() as probe:
            try:
                probe.bind(("0.0.0.0", port))
            except OSError:
                continue
        return port
    raise RuntimeError("no port free for both UDP and TCP")


def open_engine(pw: Any, engine: str, strip_udp: bool = False, url_hash: str = "") -> tuple:
    """A page of `engine` on the server, instrumented; returns (browser, ctx, page, errors)."""
    browser, ctx = TB.engine_launch(pw, engine)
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
    if strip_udp:
        ctx.add_init_script("window.__stripUdp = true;")
    ctx.add_init_script(PAGE_JS)
    page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
    errors: list = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return browser, ctx, page, errors


def close_engine(browser: Any, ctx: Any) -> None:
    try:
        ctx.close()
    finally:
        if browser is not None:
            browser.close()


def nominated_pairs(page: Any, timeout: float = 30) -> tuple:
    """The nominated pairs of every peer connection on the page, once one exists."""
    deadline = time.time() + timeout
    stats: list = []
    while time.time() < deadline:
        stats = page.evaluate(STATS_JS) or []
        if any(s["pairs"] for s in stats):
            break
        time.sleep(0.5)
    return stats, [p for s in stats for p in s["pairs"]]


def video_check(res: H.Results, tag: str, page: Any, engine: str) -> None:
    info = C.wait_wr_video(page, timeout=45)
    if info is None and engine == "webkit":
        rtp = page.evaluate(BB.VIDEO_RTP_JS)
        if rtp["received"] > 0 and rtp["decoded"] == 0:
            res.skip(f"{tag}: video decodes",
                     f"WebKit received {rtp['received']} video packets ({rtp['codec']}) but decoded none: no H.264 decoder in this build")
            return
        res.check(f"{tag}: video decodes", False, rtp)
        return
    res.check(f"{tag}: video decodes", info is not None, info)


def input_check(res: H.Results, tag: str, page: Any) -> None:
    page.mouse.click(640, 360)
    time.sleep(0.4)
    pressed = None
    for _ in range(3):
        page.keyboard.down("x")
        time.sleep(0.8)
        pressed = C.x11_keymap_pressed("x")
        page.keyboard.up("x")
        time.sleep(0.4)
        if pressed:
            break
    res.check(f"{tag}: a typed key reaches the X keymap over the pair", pressed is True, pressed)
    res.check(f"{tag}: and its release", C.x11_keymap_pressed("x") is False)


def pair_checks(res: H.Results, tag: str, page: Any, protocol: str,
                port: Optional[int] = None, window: Optional[tuple] = None,
                lite: Optional[bool] = None, engine: str = "chromium") -> list:
    stats, pairs = nominated_pairs(page)
    res.check(f"{tag}: a nominated pair exists", bool(pairs), stats)
    if pairs:
        pair = pairs[0]
        res.check(f"{tag}: the server's candidate is {protocol}",
                  (pair["remoteProtocol"] or "").lower() == protocol, pair)
        if port is not None:
            res.check(f"{tag}: on port {port}", int(pair["remotePort"] or 0) == port, pair)
        if window is not None:
            got = int(pair["remotePort"] or 0)
            res.check(f"{tag}: inside the window {window[0]}-{window[1]}",
                      window[0] <= got <= window[1], pair)
    if lite is not None and stats:
        sdp = stats[0]["remoteSdp"]
        res.check(f"{tag}: the offer {'says' if lite else 'does not say'} a=ice-lite",
                  ("a=ice-lite" in sdp) is lite)
        if lite and engine == "chromium":
            res.check(f"{tag}: the browser took the controlling role",
                      stats[0]["iceRole"] == "controlling", stats[0]["iceRole"])
    return pairs


def session_checks(res: H.Results, tag: str, page: Any, engine: str, protocol: str,
                   port: Optional[int] = None, window: Optional[tuple] = None,
                   lite: Optional[bool] = None, wayland: bool = False) -> list:
    video_check(res, tag, page, engine)
    pairs = pair_checks(res, tag, page, protocol, port, window, lite, engine)
    if not wayland:
        input_check(res, tag, page)
    return pairs


def portrange_block() -> H.Results:
    res = H.Results("ice-portrange")
    low = free_port(socket.SOCK_DGRAM)
    low = min(low, 65535 - 40)
    window = (low, low + 40)
    H.server_start(mode="webrtc", extra_env={"SELKIES_WEBRTC_PORT_RANGE": f"{window[0]}-{window[1]}"})
    try:
        with sync_playwright() as pw:
            for engine in ENGINES:
                browser, ctx, page, errors = open_engine(pw, engine)
                try:
                    session_checks(res, engine, page, engine, "udp", window=window)
                    res.check(f"{engine}: no page errors", not errors, errors)
                finally:
                    close_engine(browser, ctx)
    finally:
        H.server_stop()
    return res


def udpmux_block() -> H.Results:
    res = H.Results("ice-udpmux")
    port = free_port(socket.SOCK_DGRAM)
    H.server_start(mode="webrtc", extra_env={"SELKIES_WEBRTC_UDP_MUX_PORT": str(port)})
    try:
        res.check("the server announced the shared UDP port",
                  C.wait_log(f"WebRTC UDP mux: every session's host candidates share UDP port {port}", 5))
        with sync_playwright() as pw:
            for engine in ENGINES:
                browser, ctx, page, errors = open_engine(pw, engine)
                try:
                    session_checks(res, engine, page, engine, "udp", port=port)
                    if engine == "chromium":
                        viewer = ctx.new_page()
                        viewer.goto(H.BASE_URL + "/#shared", wait_until="load")
                        video_check(res, "chromium viewer", viewer, engine)
                        pair_checks(res, "chromium viewer", viewer, "udp", port=port)
                        again, _ = nominated_pairs(page, 5)
                        res.check("the controller's session stays up beside the viewer's on the one port",
                                  again and again[0]["connection"] == "connected", again)
                        viewer.close()
                    res.check(f"{engine}: no page errors", not errors, errors)
                finally:
                    close_engine(browser, ctx)
    finally:
        H.server_stop()
    return res


def tcpmux_block() -> H.Results:
    res = H.Results("ice-tcpmux")
    port = free_port(socket.SOCK_STREAM)
    H.server_start(mode="webrtc", extra_env={"SELKIES_WEBRTC_TCP_MUX_PORT": str(port)})
    try:
        res.check("the server announced the ICE-TCP port",
                  C.wait_log(f"WebRTC TCP mux: ICE-TCP accepted on TCP port {port}", 5))
        with sync_playwright() as pw:
            browser, ctx, page, errors = open_engine(pw, "chromium")
            try:
                stats, _ = nominated_pairs(page)
                sdp = stats[0]["remoteSdp"] if stats else ""
                passive = [line for line in sdp.splitlines()
                           if line.startswith("a=candidate:") and " tcp " in line and "tcptype passive" in line]
                res.check("the offer carries a passive TCP candidate on the port beside the UDP ones",
                          passive and all(line.split()[5] == str(port) for line in passive)
                          and any(" udp " in line for line in sdp.splitlines() if line.startswith("a=candidate:")),
                          passive or sdp)
                session_checks(res, "chromium with UDP available", page, "chromium", "udp")
            finally:
                close_engine(browser, ctx)
            for engine in ENGINES:
                tag = f"{engine} with UDP stripped"
                browser, ctx, page, errors = open_engine(pw, engine, strip_udp=True)
                try:
                    pairs = session_checks(res, tag, page, engine, "tcp", port=port)
                    if pairs and engine == "chromium":
                        res.check(f"{tag}: the server's candidate is the passive one",
                                  pairs[0]["remoteTcpType"] == "passive", pairs[0])
                    res.check(f"{tag}: no page errors", not errors, errors)
                finally:
                    close_engine(browser, ctx)
    finally:
        H.server_stop()
    return res


def icelite_block() -> H.Results:
    res = H.Results("ice-lite")
    H.server_start(mode="webrtc", extra_env={"SELKIES_WEBRTC_ICE_LITE": "true"})
    try:
        res.check("the server announced ICE-lite", C.wait_log("WebRTC ICE-lite", 5))
        with sync_playwright() as pw:
            for engine in ENGINES:
                browser, ctx, page, errors = open_engine(pw, engine)
                try:
                    session_checks(res, engine, page, engine, "udp", lite=True)
                    res.check(f"{engine}: no page errors", not errors, errors)
                finally:
                    close_engine(browser, ctx)
    finally:
        H.server_stop()
    return res


def combined_block(wayland: bool = False) -> H.Results:
    res = H.Results("ice-combined-wl" if wayland else "ice-combined")
    port = free_port_both()
    H.server_start(mode="webrtc", wayland=wayland, extra_env={
        "SELKIES_WEBRTC_UDP_MUX_PORT": str(port),
        "SELKIES_WEBRTC_TCP_MUX_PORT": str(port),
        "SELKIES_WEBRTC_ICE_LITE": "true",
        "SELKIES_WEBRTC_PORT_RANGE": "50000-50100",
    })
    try:
        res.check("a port range beside the UDP mux is warned about at startup",
                  C.wait_log("webrtc_port_range is unused while webrtc_udp_mux_port is set", 5))
        with sync_playwright() as pw:
            for engine, strip in (("chromium", False), ("chromium", True), ("firefox", True), ("webkit", False)):
                tag = f"{engine} with UDP {'stripped' if strip else 'available'}"
                browser, ctx, page, errors = open_engine(pw, engine, strip_udp=strip)
                try:
                    session_checks(res, tag, page, engine, "tcp" if strip else "udp", port=port,
                                   lite=True, wayland=wayland)
                    res.check(f"{tag}: no page errors", not errors, errors)
                finally:
                    close_engine(browser, ctx)
    finally:
        H.server_stop()
    return res


BLOCKS = {
    "portrange": portrange_block,
    "udpmux": udpmux_block,
    "tcpmux": tcpmux_block,
    "icelite": icelite_block,
    "combined": combined_block,
    "combined-wl": lambda: combined_block(wayland=True),
}


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    selectors = list(BLOCKS) if which == "all" else [which]
    results = [BLOCKS[selector]() for selector in selectors]
    summaries = [r.summary() for r in results]
    return 0 if all(summaries) else 1


if __name__ == "__main__":
    sys.exit(main())
