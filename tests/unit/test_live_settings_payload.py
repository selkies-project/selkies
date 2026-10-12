#!/usr/bin/env python3
"""The settings payload a page is greeted with carries what its display streams with.

`with_live_stream_settings` puts a display's current values in place of the
boot ones: a range as its `default` (its `value` is the range), a list as a
string, a bool as it is; keys the payload lacks and values not known yet are
left alone, and the boot payload's own entries are not changed in place. Both
transports greet with it: the WebSockets page payload of a display holding
picks, and WebRTC's of its primary.
"""
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from selkies.settings import STREAM_SETTINGS, build_client_settings_payload, with_live_stream_settings
from selkies import webrtc_mode, websockets_mode

res = H.Results("live-settings-payload")

boot = build_client_settings_payload()
before = {key: dict(boot[key]) for key in ("framerate", "rate_control_mode", "audio_bitrate", "use_cpu")}
live = with_live_stream_settings(build_client_settings_payload(), {
    "framerate": 30, "rate_control_mode": "crf", "audio_bitrate": 128000, "use_cpu": True,
    "video_crf": None, "not_a_setting": 1})
res.check("a range carries the live value as its default, its span unchanged",
          live["framerate"]["default"] == 30 and (live["framerate"]["min"], live["framerate"]["max"])
          == (boot["framerate"]["min"], boot["framerate"]["max"]), live["framerate"])
res.check("a list carries the live value", live["rate_control_mode"]["value"] == "crf", live["rate_control_mode"])
res.check("a list of strings carries a number as a string", live["audio_bitrate"]["value"] == "128000",
          live["audio_bitrate"])
res.check("a bool stays a bool", live["use_cpu"]["value"] is True, live["use_cpu"])
res.check("a value not known yet keeps the boot entry", live["video_crf"] == boot["video_crf"], live["video_crf"])
res.check("a key the payload lacks is not added", "not_a_setting" not in live, sorted(set(live) - set(boot)))
res.check("the boot payload's entries are not changed in place",
          all(boot[key] == before[key] for key in before), "")

# WebSockets: a display's picks, and the session encoder where it holds none.
ws = websockets_mode.DataStreamingServer.__new__(websockets_mode.DataStreamingServer)
ws.display_clients = {"primary": {"framerate": 24.0, "rate_control_mode": "crf", "video_crf": 33,
                                  "encoder": "jpeg"}, "display2": {}}
ws.app = SimpleNamespace(encoder="h264enc", audio_bitrate=320000)
ws._initial_use_cpu = False
ws._second_screen_availability = lambda: (True, "")
ws.input_handler = None
payload = ws._settings_payload_for_display("primary")
res.check("WebSockets: the page payload carries the display's picks",
          (payload["framerate"]["default"], payload["rate_control_mode"]["value"], payload["video_crf"]["default"],
           payload["encoder"]["value"]) == (24.0, "crf", 33, "jpeg"),
          {k: payload[k].get("default", payload[k].get("value")) for k in ("framerate", "rate_control_mode",
                                                                         "video_crf", "encoder")})
res.check("WebSockets: a display holding no picks gets the session encoder",
          ws._settings_payload_for_display("display2")["encoder"]["value"] == "h264enc", "")

# WebRTC: the primary's values are the service args.
wr = webrtc_mode.WebRTCService.__new__(webrtc_mode.WebRTCService)
wr.args = SimpleNamespace(**{key: None for key in STREAM_SETTINGS})
wr.args.framerate, wr.args.rate_control_mode, wr.args.video_bitrate = 30, "cbr", 2000
wr.display_clients = {}
wr._second_screen_availability = lambda: (True, "")
wr.input_handler = None
payload = wr._server_settings_payload("primary")["settings"]
res.check("WebRTC: the greeting carries the primary's values",
          (payload["framerate"]["default"], payload["rate_control_mode"]["value"],
           payload["video_bitrate"]["default"]) == (30, "cbr", 2000),
          {k: payload[k].get("default", payload[k].get("value")) for k in ("framerate", "rate_control_mode",
                                                                         "video_bitrate")})

sys.exit(0 if res.summary() else 1)
