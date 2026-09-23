---
title: pcmflux
description: The audio capture and Opus encoding extension behind every Selkies session, and where its own documentation lives.
---

Audio capture and encoding are performed by [`pcmflux`](https://github.com/selkies-project/pcmflux), a companion Rust (PyO3) extension the `selkies` wheel installs as a dependency. Its Rust reference is published at <https://pcmflux.selkies.io>, and its README carries the Python API.

## What it does

`pcmflux` captures from PulseAudio or PipeWire-Pulse — the `output.monitor` source by default, `--audio-device-name` names another — and encodes to Opus, the one full-band codec every browser plays by specification. `--audio-bitrate`, `--audio-channels` and `--audio-frame-duration-ms` shape the stream, and `--audio-redundancy` adds Opus RED (RFC 2198) redundancy so a lost packet costs no dropout, at the bandwidth of the frames it repeats. The same stream is carried over the WebSocket or as the WebRTC audio track, with nothing configured differently between the two.

The microphone uplink runs the other way through the same library: the browser's Opus arrives over the transport and is played into a PulseAudio source the session's applications record from, through the sound server the capture reads. A session recording (`--recording-socket`, the [Operator API](../usage.md#operator-api)) carries the sound `pcmflux` hands `pixelflux` over a socket, muxed as a second track beside the video.

Whatever starts off is not captured at all until it is turned on: no capture runs for a page that does not receive audio, and the microphone is only asked for once the client turns it on or, under `--microphone-on-start=demand`, once an application records from the virtual source ([What a Session Starts With](../usage.md#what-a-session-starts-with)).

## Installing a Development Build

As for [pixelflux](pixelflux.md#installing-a-development-build): every commit to `main` is attached to a pre-release as wheels, and a local change is a wheel built with `pip wheel . --no-deps` and installed beside `selkies`.
