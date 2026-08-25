#!/usr/bin/env python3
"""The primary WebRTC capture survives a controller tab reload.

A page reload drops and re-adds its peer within a second or two, so the primary
capture stop is deferred by a reconnect grace and cancelled when a consumer
reclaims the display — a reconnecting controller reuses the still-warm capture,
and any viewers keep streaming throughout (websockets _teardown_if_unclaimed
parity). A secondary display's stop is immediate. Driven against the service's
start/stop_display_media with a fake pipeline; no browser, no capture backend.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import webrtc_mode


class FakePipeline:
    def __init__(self) -> None:
        self.started = 0
        self.stopped = 0
        self.running = True

    async def start_media_pipeline(self) -> None:
        self.started += 1
        self.running = True

    async def stop_media_pipeline(self) -> None:
        self.stopped += 1
        self.running = False

    def is_media_pipeline_running(self) -> bool:
        return self.running


def make_service(grace: float = 0.2) -> webrtc_mode.WebRTCService:
    svc = webrtc_mode.WebRTCService(SimpleNamespace(set_clients_present=lambda present: None))
    svc.RECONNECT_GRACE_S = grace
    svc.settings = SimpleNamespace(second_screen=(True, False), wayland_host_display="")
    svc.media_pipeline = FakePipeline()
    svc.rtc_app = SimpleNamespace(peer_connections={}, send_media_data_over_channel=lambda *a: None)
    return svc


async def scenario(res: H.Results) -> None:
    grace = 0.2

    # A reload: stop, then a consumer reclaims within the grace -> no stop.
    svc = make_service(grace)
    svc.rtc_app.peer_connections = {"c1": {"display_id": "primary"}}
    await svc.stop_display_media("primary")
    res.check("primary stop is deferred, not immediate",
              svc.media_pipeline.stopped == 0 and svc._primary_stop_grace_task is not None,
              svc.media_pipeline.stopped)
    await asyncio.sleep(grace / 2)
    await svc.start_display_media("primary")  # the reconnecting consumer
    res.check("a consumer reclaiming cancels the pending stop",
              svc._primary_stop_grace_task is None, svc._primary_stop_grace_task)
    await asyncio.sleep(grace)
    res.check("reload: capture kept warm (never stopped, restart is idempotent no-op)",
              svc.media_pipeline.stopped == 0 and svc.media_pipeline.running, svc.media_pipeline.stopped)

    # A real departure: nobody reclaims -> the capture stops after the grace.
    svc = make_service(grace)
    svc.rtc_app.peer_connections = {}
    await svc.stop_display_media("primary")
    res.check("unclaimed: still running right after stop (within grace)",
              svc.media_pipeline.running, svc.media_pipeline.running)
    await asyncio.sleep(grace * 1.5)
    res.check("unclaimed: capture stops once the grace elapses",
              svc.media_pipeline.stopped == 1 and not svc.media_pipeline.running,
              svc.media_pipeline.stopped)

    # A consumer present at grace-expiry keeps the capture even without a cancel.
    svc = make_service(grace)
    svc.rtc_app.peer_connections = {"c1": {"display_id": "primary"}}
    # Bypass start_display_media's cancel to prove the expiry re-check also guards.
    svc._schedule_primary_stop_grace()
    await asyncio.sleep(grace * 1.5)
    res.check("a consumer present at expiry keeps the capture",
              svc.media_pipeline.stopped == 0 and svc.media_pipeline.running, svc.media_pipeline.stopped)

    # A secondary display's stop is immediate (no grace).
    svc = make_service(grace)
    sec = FakePipeline()
    svc.display_pipelines["display2"] = sec
    svc.display_clients["display2"] = {"width": 1280, "height": 720}
    svc.display_layouts["display2"] = {"x": 1280, "y": 0, "w": 1280, "h": 720}
    svc._broadcast_display_config = lambda: None

    async def no_reconfigure():
        return None

    svc.reconfigure_displays = no_reconfigure
    await svc.stop_display_media("display2")
    res.check("secondary stop is immediate and unregisters the display",
              sec.stopped == 1 and "display2" not in svc.display_clients
              and "display2" not in svc.display_pipelines, (sec.stopped, sorted(svc.display_clients)))

    # Shutdown-style cancel leaves no pending grace task running.
    svc = make_service(grace)
    svc.rtc_app.peer_connections = {}
    await svc.stop_display_media("primary")
    svc._cancel_primary_stop_grace()
    await asyncio.sleep(grace * 1.5)
    res.check("cancel drops the pending stop (shutdown path)",
              svc.media_pipeline.stopped == 0 and svc._primary_stop_grace_task is None,
              svc.media_pipeline.stopped)


def main() -> bool:
    res = H.Results("webrtc-reconnect-grace")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
