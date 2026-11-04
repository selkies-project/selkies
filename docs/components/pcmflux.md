---
title: pcmflux
description: The audio capture and Opus encoding library behind every Selkies session, and where its own documentation lives.
---

Audio capture and encoding are performed by [`pcmflux`](https://github.com/selkies-project/pcmflux), a companion Rust library the `selkies` server links as a crate, pinned like `pixelflux` in the workspace's `Cargo.toml`. Its Rust reference is published at <https://pcmflux.selkies.io>.

## What it does

`pcmflux` captures from PulseAudio or PipeWire-Pulse — the `output.monitor` source by default, `--audio-device-name` names another — and encodes to Opus, the one full-band codec every browser plays by specification. `--audio-bitrate`, `--audio-channels`, and `--audio-frame-duration-ms` shape the stream, and `--audio-redundancy` adds Opus RED (RFC 2198) redundancy so a lost packet costs no dropout, at the bandwidth of the frames it repeats. The same stream is carried over the WebSocket or as the WebRTC audio track, with nothing configured differently between the two. A surround session (`--audio-channels` 6 or 8) plays into a capture sink created in that speaker layout; a WebSocket page decodes it itself, and a WebRTC page receives it where its browser decodes Chromium's `multiopus` and a stereo fold of it otherwise (Firefox).

The microphone uplink runs the other way through the same library: the browser's Opus arrives over the transport and is played into a PulseAudio source the session's applications record from, through the sound server the capture reads. A session recording (`--recording-socket`, the [Operator API](../usage.md#operator-api)) carries the sound `pcmflux` hands `pixelflux` over a socket, muxed as a second track beside the video.

Whatever starts off is not captured at all until it is turned on: no capture runs for a page that does not receive audio, and the microphone is only asked for once the client turns it on or, under `--microphone-on-start=demand`, once an application records from the virtual source ([What a Session Starts With](../usage.md#what-a-session-starts-with)).

## Building Against a Development Checkout

As for [pixelflux](pixelflux.md#building-against-a-development-checkout): the server builds the commit the `rev` in `Cargo.toml` names, and a local change is built into it through a `[patch]` of the `pcmflux` repository's URL in a Cargo configuration outside the repository.
