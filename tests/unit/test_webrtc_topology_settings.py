#!/usr/bin/env python3
"""The ICE topology settings, from the parser to the offer's SDP.

`webrtc_udp_mux_port`, `webrtc_tcp_mux_port` and `webrtc_ice_lite` parse in
both spellings, `RTCApp.open_ice_muxes` binds the named ports once on every
host address (failing loudly on one in use, warning that a port range yields
to the UDP mux) and `get_rtc_config` carries the muxes and the lite choice to
every peer, leaving STUN/TURN out of an ICE-lite agent's configuration. A
real RTCPeerConnection then offers an SDP whose host candidates sit on the
mux ports, with a passive TCP candidate and `a=ice-lite`.
"""
import asyncio
import logging
import os
import socket
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.settings import SETTING_DEFINITIONS, AppSettings, settings  # noqa: E402
from selkies.rtc import RTCApp  # noqa: E402
from selkies.ice import UdpMux  # noqa: E402
from selkies.ice.ice import get_host_addresses  # noqa: E402
from selkies.webrtc import RTCPeerConnection  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-topology] {label}  {detail}", flush=True)


def free_port(kind: int) -> int:
    with socket.socket(socket.AF_INET, kind) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def apply(udp: int = 0, tcp: int = 0, lite: bool = False, port_range: str = "") -> None:
    settings.webrtc_udp_mux_port = udp
    settings.webrtc_tcp_mux_port = tcp
    settings.webrtc_ice_lite = (lite, False)
    settings.webrtc_port_range = port_range


def definitions_and_parsing() -> None:
    by_name = {d["name"]: d for d in SETTING_DEFINITIONS}
    for name, kind, default in (("webrtc_udp_mux_port", "int", 0),
                                ("webrtc_tcp_mux_port", "int", 0),
                                ("webrtc_ice_lite", "bool", False)):
        spec = by_name.get(name)
        check(f"{name}: {kind} defaulting to {default}",
              spec is not None and spec["type"] == kind and spec["default"] == default, spec)
    argv, environ = sys.argv, dict(os.environ)
    try:
        sys.argv = ["selkies", "--webrtc-udp-mux-port=59000", "--webrtc_tcp_mux_port=443"]
        os.environ["SELKIES_WEBRTC_ICE_LITE"] = "true|locked"
        parsed = AppSettings(SETTING_DEFINITIONS)
    finally:
        sys.argv = argv
        os.environ.clear()
        os.environ.update(environ)
    check("the ports parse in both flag spellings",
          parsed.webrtc_udp_mux_port == 59000 and parsed.webrtc_tcp_mux_port == 443,
          (parsed.webrtc_udp_mux_port, parsed.webrtc_tcp_mux_port))
    check("ice lite parses from the environment, locked suffix included",
          parsed.webrtc_ice_lite == (True, True), parsed.webrtc_ice_lite)


async def offer_sdp(app: RTCApp) -> str:
    pc = RTCPeerConnection(app.get_rtc_config())
    pc.createDataChannel("input")
    try:
        await pc.setLocalDescription(await pc.createOffer())
        return pc.localDescription.sdp
    finally:
        await pc.close()


class Capture(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.records: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append(record)


async def app_and_offers() -> None:
    loop = asyncio.get_running_loop()
    app = RTCApp(async_event_loop=loop, encoder="h264enc",
                 stun_servers=["stun://stun.example.test:3478"], turn_servers=[])
    capture = Capture()
    logging.getLogger("rtc").addHandler(capture)
    try:
        apply()
        await app.open_ice_muxes()
        config = app.get_rtc_config()
        check("defaults: no mux, no lite, the STUN server configured",
              app.ice_udp_mux is None and app.ice_tcp_mux is None and config.iceUdpMux is None
              and config.iceTcpMux is None and config.iceLite is False and len(config.iceServers) == 1,
              config)
        sdp = await offer_sdp(app)
        check("defaults: the offer is a full agent's", "a=ice-lite" not in sdp)

        apply(lite=True)
        config = app.get_rtc_config()
        check("ice lite leaves STUN and TURN out of the server's configuration",
              config.iceLite is True and config.iceServers == [], config.iceServers)
        sdp = await offer_sdp(app)
        check("ice lite: the offer says a=ice-lite", "a=ice-lite" in sdp)

        udp, tcp = free_port(socket.SOCK_DGRAM), free_port(socket.SOCK_STREAM)
        apply(udp=udp, tcp=tcp, lite=True, port_range="50000-50100")
        await app.open_ice_muxes()
        addresses = get_host_addresses(use_ipv4=True, use_ipv6=True)
        check("the muxes bind every host address on their ports",
              sorted(app.ice_udp_mux.listen_addresses) == sorted((a, udp) for a in addresses)
              and sorted(app.ice_tcp_mux.listen_addresses) == sorted((a, tcp) for a in addresses),
              (addresses, app.ice_udp_mux.listen_addresses, app.ice_tcp_mux.listen_addresses))
        check("a port range beside the UDP mux is warned about",
              any("webrtc_port_range is unused" in r.getMessage() for r in capture.records))
        config = app.get_rtc_config()
        check("every new peer's configuration carries the muxes",
              config.iceUdpMux is app.ice_udp_mux and config.iceTcpMux is app.ice_tcp_mux)
        sdp = await offer_sdp(app)
        lines = [line for line in sdp.splitlines() if line.startswith("a=candidate:")]
        udp_hosts = [line for line in lines if " udp " in line and " typ host" in line]
        tcp_hosts = [line for line in lines if " tcp " in line]
        check("the offer's UDP host candidates all sit on the UDP mux port",
              udp_hosts and all(line.split()[5] == str(udp) for line in udp_hosts), udp_hosts)
        check("the offer carries one passive TCP candidate per address on the TCP mux port",
              len(tcp_hosts) == len(addresses)
              and all(line.split()[5] == str(tcp) and "tcptype passive" in line for line in tcp_hosts),
              tcp_hosts)
        check("the lite offer over the muxes still says a=ice-lite", "a=ice-lite" in sdp)
        ufrag = [line for line in sdp.splitlines() if line.startswith("a=ice-ufrag:")][0]
        check("the username fragment is long enough to key the shared sockets",
              len(ufrag.split(":", 1)[1]) >= 8, ufrag)
        check("closing the peer detached it from the muxes",
              app.ice_udp_mux.attached() == 0 and app.ice_tcp_mux.attached() == 0)

        await app.close_ice_muxes()
        check("close releases the ports", app.ice_udp_mux is None and app.ice_tcp_mux is None)
        again = UdpMux(udp)
        await again.open(addresses)
        await again.close()
        check("and a later bind of the UDP port succeeds", True)

        blocker = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        blocker.bind((addresses[0], 0))
        apply(udp=blocker.getsockname()[1])
        try:
            await app.open_ice_muxes()
            check("a UDP mux port in use fails startup", False, "no OSError")
        except OSError:
            check("a UDP mux port in use fails startup", app.ice_udp_mux is None)
        finally:
            blocker.close()
        blocker = socket.socket()
        blocker.bind((addresses[0], 0))
        blocker.listen(1)
        apply(udp=udp, tcp=blocker.getsockname()[1])
        try:
            await app.open_ice_muxes()
            check("a TCP mux port in use fails startup and releases the UDP mux", False, "no OSError")
        except OSError:
            check("a TCP mux port in use fails startup and releases the UDP mux",
                  app.ice_udp_mux is None and app.ice_tcp_mux is None)
        finally:
            blocker.close()
    finally:
        apply()
        await app.close_ice_muxes()
        logging.getLogger("rtc").removeHandler(capture)


def main() -> int:
    definitions_and_parsing()
    asyncio.run(app_and_offers())
    print(f"[ice-topology] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
