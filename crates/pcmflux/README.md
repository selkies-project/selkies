<h1 align="center">pcmflux</h1>

<p align="center">Part of <a href="https://www.linuxserver.io"><img src="https://raw.githubusercontent.com/selkies-project/selkies/main/crates/pcmflux/assets/linuxserver.svg" alt="LinuxServer.io" title="LinuxServer.io" width="182" align="middle"></a></p>

[![CI](https://github.com/selkies-project/selkies/actions/workflows/ci.yaml/badge.svg)](https://github.com/selkies-project/selkies/actions/workflows/ci.yaml) [![Release](https://img.shields.io/github/v/release/selkies-project/selkies?display_name=tag)](https://github.com/selkies-project/selkies/releases/latest) [![PyPI](https://img.shields.io/pypi/v/pcmflux)](https://pypi.org/project/pcmflux/) [![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0) [![Docs](https://img.shields.io/badge/docs-docs.selkies.io-blue)](https://docs.selkies.io/rustdoc/pcmflux/index.html) [![Discord](https://img.shields.io/badge/dynamic/json?logo=discord&label=Discord%20Members&query=approximate_member_count&url=https%3A%2F%2Fdiscordapp.com%2Fapi%2Finvites%2FwDNGDeSW5F%3Fwith_counts%3Dtrue)](https://discord.gg/wDNGDeSW5F) [![Ask DeepWiki](https://img.shields.io/badge/Ask-DeepWiki-blue)](https://deepwiki.com/selkies-project/selkies)

pcmflux is a high-performance audio capture and encoding library for Rust and Python.

It is designed to capture system audio using PulseAudio, encode it into the Opus format, and stream it with low latency. A key optimization is its ability to detect and discard silent audio chunks, significantly reducing network traffic and CPU usage during periods of no sound. This directory's Rust crate is the library; its `python` feature adds the `pcmflux` extension module, thin wrappers over the same API, which the wheels build.

## Installation

Every release on the [GitHub Releases page](https://github.com/selkies-project/selkies/releases) carries wheels (`manylinux_2_28` and `musllinux`, x86_64 and aarch64, CPython 3.9 and newer), the pre-releases cut per commit included; take the one for your interpreter and platform:
```bash
pip install ./pcmflux-<version>-cp312-cp312-manylinux_2_28_x86_64.whl
```

A wheel carries the PulseAudio client library it links, so all a host needs is a PulseAudio server, or PipeWire through `pipewire-pulse`, to capture from and play into.

### Building from source

This package builds a native Rust extension (via `setuptools-rust`/PyO3, the crate's `python` and `extension-module` features). It requires a Rust toolchain (`cargo`/`rustc` 1.88 or newer) plus the PulseAudio development headers on your system.

On Debian/Ubuntu, from the root of the repository:
```bash
sudo apt-get install libpulse-dev cmake build-essential
pip install ./crates/pcmflux
```

The Opus encoder is built from the copy of libopus that `opusic-sys` vendors and is linked statically, so `cmake` and a C compiler are always required and no system `libopus` is used.

A Rust program depends on the crate directly, pinned to the release it goes with, with no Python involved:
```toml
[dependencies]
pcmflux = { git = "https://github.com/selkies-project/selkies", tag = "<release>" }
```

## Core Features

- **PulseAudio Capture:** Captures system audio via PulseAudio using the asynchronous `Context`/`Stream` record API with a manually-pumped mainloop.
- **Opus Encoding:** Integrates the high-quality, low-latency Opus codec.
- **Silence Detection:** Intelligently skips encoding and sending silent audio chunks.
- **Native Audio Header:** With `omit_audio_header=False` (the default), the encoder prepends a 2-byte `[0x01, 0x00]` header to each chunk natively, so WebSocket transports avoid an extra Python copy. When the silence gate closes after sound, a single two-byte `[0x01, 0x80]` chunk with no Opus marks where the sound ends (as it does when the sound server drops the capture), so a player plays out what it holds instead of waiting for more and can tell the sender's silence from a late delivery; the first silent chunk follows it with the same bit set, since its Opus carries the end of the sound the encoder held back, and a player appends it to that sound. Below that bit, the second byte is the RED block count. Set it to `True` for raw Opus (WebRTC/RTP).
- **Optional RED redundancy (RFC 2198):** `red_distance` (0–4, default 0) prepends redundant copies of recent Opus payloads for lossy/unreliable transports; `0` disables it (the default for reliable WebSocket/TCP).
- **Zero-copy Frames:** Each callback receives a native `AudioFrame` that owns the encoded chunk and supports the buffer protocol — `bytes(frame)` / `memoryview(frame)` / `len(frame)` — on **every supported Python version (3.9 and newer)**. `memoryview(frame)` aliases the buffer with no copy, and the frame keeps it alive until every view is released, so the hand-off is memory-safe.
- **Tunable Capture:** Configurable `latency_ms`, validated `frame_duration_ms` (2.5/5/10/20/40/60 ms, default 20), VBR/CBR, an optional `opus_complexity` (0–10; unset leaves libopus's default of 9), and a toggleable silence gate.
- **Multichannel Opus:** Mono, stereo, and 5.1 / 7.1 surround (via the Opus multistream API with Chromium-compatible channel layouts); `channels` accepts 1, 2, 6, or 8. A surround capture asks the sound server for the speaker positions its encoder reads (front left, right, and center, LFE, the rear pair, and at 7.1 the side pair), so a source of any layout is remixed into them, and `set_stereo_companion(True)` has it also deliver every frame folded to stereo (ITU-R BS.775, LFE left out) for consumers that decode no multistream Opus: each companion frame follows its surround frame with the same `pts` and `frame.channels == 2`.
- **Mic-Uplink Playback:** An `AudioPlayback` class decodes an inbound Opus stream (with optional RED recovery via `write_red`) and plays it into a PulseAudio sink — the reverse of capture, for client microphone audio. Playback is mono/stereo, and `write` / `write_red` take any bytes-like object (`bytes`, `memoryview`, `bytearray`, ...). Queued audio that stays unplayed through a whole second (after a burst, a stalled client, a sink that resumed late, or a client clock running fast of the sink's) is cut from the head of the queue with a 2 ms crossfade, so the uplink's delay returns to the sink's own buffer (`latency_ms`) instead of standing up to `max_buffer_bytes`.
- **Live Bitrate Updates:** Thread-safe `update_audio_bitrate()` adjusts the Opus bitrate during an active session.
- **Observable Lifecycle:** `state` (`"idle"` / `"starting"` / `"running"` / `"failed"`) and `last_error` on both `AudioCapture` and `AudioPlayback`, so a capture that fails after `start_capture` returned (PulseAudio took longer than the start handshake, or dropped out mid-run and could not be reconnected) is visible to the caller. Invalid settings raise `ValueError` before a thread is spawned.
- **Two Bridges, One Engine:** The Rust crate (`AudioCapture`, `AudioPlayback`, the settings, `AudioFrame`, the `AudioSink` trait) is the library; the `python` feature adds the native `pcmflux` extension module (full CPython API, not Limited/abi3) as thin wrappers over it, and `setuptools-rust` builds and packages that module.

## Usage

`AudioCapture.start_capture(settings, callback)` spawns a capture thread and
invokes `callback(frame)` once per *encoded* chunk. When the silence gate is on
(`use_silence_gate=True`, the default), silent chunks are dropped before
encoding and the callback is simply **not** called for them — it never receives
an empty frame, so there's no silence to filter out. The first silent chunk after
sound is the exception when it carries the end of the sound the encoder held back
(its 2.5 ms lookahead): a capture that emits the audio header always delivers it,
right behind the two-byte `[0x01, 0x80]` frame that marks where the silence starts,
and a raw Opus capture delivers it only when the sound reached into that lookahead,
since a WebRTC receiver's jitter buffer fades the sound after a pause in from what it
concealed, and concealing from a chunk of silence faded more short sounds than
concealing from the sound itself. The `frame` is a zero-copy `AudioFrame` (buffer protocol, a `.pts`
presentation timestamp in samples, and the `.channels` its Opus carries).
Copy it out with `bytes(frame)` if it must outlive the callback, or pass
`memoryview(frame)` for a zero-copy hand-off (keep the frame referenced for the
duration of the send so its buffer stays alive).
With no callback the capture serves only its `output_socket` (below) and no
frame reaches Python.

```python
from pcmflux import AudioCapture, AudioCaptureSettings

def on_chunk(frame):
    # Silence-gated chunks are never delivered (the callback is skipped for
    # them), so there's no empty/"silence" frame to filter out here.
    data = bytes(frame)          # copy out (header+Opus, or raw Opus per settings)
    pts = frame.pts              # presentation timestamp, in samples
    # send `data` to your client...

settings = AudioCaptureSettings()
settings.device_name = None      # None / "" => system default source
settings.frame_duration_ms = 20  # one of 2.5/5/10/20/40/60
settings.opus_complexity = 5     # 0-10, lower spends less encoder CPU; None keeps the default

capture = AudioCapture()
capture.start_capture(settings, on_chunk)
# capture.update_audio_bitrate(96000)  # adjust the Opus bitrate while running
# ...
capture.stop_capture()
```

### API notes

- `start_capture()` raises `ValueError` for settings the encoder or PulseAudio
  could never accept (sample rate, channel count, frame duration, an
  `opus_complexity` outside 0–10, negative latency, a NUL in `device_name`)
  and `RuntimeError` when the capture thread fails within the ~2 s start
  handshake. A PulseAudio server (or the named source) that is still coming up
  is retried with backoff for longer than that — `start_capture()` then
  returns with `state == "starting"`, and the outcome is published
  asynchronously: `state` becomes `"running"`, or `"failed"` with the reason
  in `last_error`. The same pair reports a capture that drops out mid-run and
  exhausts its reconnect budget, so a long-lived caller should poll
  `last_error` (or `state`) and restart when it is set. `is_capturing` is True
  only in the `"running"` phase.
- `AudioPlayback.start()` validates `AudioPlaybackSettings` the same way
  (`latency_ms` and `max_buffer_bytes` must be positive) and exposes the same
  `is_running` / `state` / `last_error` trio; `write()` / `write_red()` raise
  `RuntimeError` once the playback thread is gone.

- `update_audio_bitrate(bps)` stores the new Opus bitrate
  atomically; the capture thread re-reads it on the next frame, so it only takes
  effect during an **active** capture session. Calling it while no capture is
  active is **not** an error — it is a silent no-op store, but the value does
  **not** persist into the next session: the next `start_capture(settings, ...)`
  snapshots the passed settings object and re-seeds the atomic bitrate mirror from
  `settings.opus_bitrate`. To change the bitrate for a new
  session, set `settings.opus_bitrate` before `start_capture()`; use
  `update_audio_bitrate()` only to adjust a session that is already running.

## Rust API

The same engine, without an interpreter: `AudioCapture::start(&settings, sink)` takes an
`AudioCaptureSettings` (its `Default` is the Python class's) and an `Option<Arc<dyn AudioSink>>`,
where any `Fn(AudioFrame) + Send + Sync` is a sink, called on the delivery thread once per
encoded frame exactly as the Python callback is. An `AudioFrame` exposes `as_bytes()`, `pts()`,
and `channels()`, and recycles its buffer to the capture when dropped, so copy it out to keep it.
Validation happens in one place, `AudioCaptureSettings::validate` (and the playback's), and a
refused call is an `Error`: `Error::Settings` for what Python raises as `ValueError`,
`Error::Start` and `Error::NotRunning` for its `RuntimeError`s. The lifecycle reads through
`state()` (`State::Idle` / `Starting` / `Running` / `Failed`, spelled as Python's `state` by
`as_str()`) and `last_error()`; `is_capturing()`, `update_audio_bitrate()`, and
`set_stereo_companion()` behave as their Python namesakes, and `stop_all()` is what the Python
module runs at interpreter exit. `AudioPlayback` mirrors `AudioCapture` with `start`, `write`,
`write_red`, `stop`, `is_running`, `state`, and `last_error`. A `stop` joins the capture
thread, which joins the delivery thread, so a sink must not make `stop` wait on a lock its own
in-flight call holds; the Python bridge releases the GIL around every join for that reason.

```rust
use std::sync::Arc;
use pcmflux::{AudioCapture, AudioCaptureSettings, AudioFrame, AudioSink};

let settings = AudioCaptureSettings {
    frame_duration_ms: 20.0,
    opus_complexity: Some(5),
    ..AudioCaptureSettings::default()
};
let sink: Arc<dyn AudioSink> = Arc::new(|frame: AudioFrame| {
    let data = frame.as_bytes().to_vec(); // header+Opus, or raw Opus per settings
    let pts = frame.pts();                // presentation timestamp, in samples
    // send `data` to your client...
});
let capture = AudioCapture::new();
capture.start(&settings, Some(sink))?;
// capture.update_audio_bitrate(96000);
// ...
capture.stop();
```

## Example Usage

The `example` directory contains a standalone demo that captures system audio, broadcasts it over a WebSocket, and plays it back in a web browser using the WebCodecs API.

To run the example:

1.  Install the module: `pip3 install .`, or a wheel from the [GitHub Releases page](https://github.com/selkies-project/selkies/releases)
2.  Run the server: `cd example && python3 audio_to_browser.py`
3.  Open `http://localhost:9001` in a modern web browser (Chrome, Edge, etc.).

The example client (`index.html`) strips the 2-byte header before decoding, and its `FRAME_DURATION_US` constant must match the server's `frame_duration_ms` (the value is not announced over the wire).

## Ogg Opus Output Socket

`output_socket` names a Unix socket the capture serves its packets on as a standard Ogg Opus stream, to every consumer that connects: the `OpusHead` and `OpusTags` pages first, then one page per packet with the 48 kHz granule position. A consumer that stops reading is dropped, never waited on. A path that cannot be bound (a missing directory, or a socket another account owns) fails the start: `start_capture` raises `RuntimeError` with the reason, which `last_error` keeps.

```python
settings.output_socket = "/run/user/1000/audio.sock"
AudioCapture().start_capture(settings)  # socket only: no callback, no Python per frame
```

```bash
ffmpeg -i unix:/run/user/1000/audio.sock -c:a copy capture.opus
```

## Development

`AGENTS.md` carries the conventions and the invariants of this tree, for contributors and coding agents alike. `cargo test --lib` covers the capture, encode, and assembly paths without an interpreter, `cargo test --lib --features python` adds the Python bridge, and `cargo test --release bench_emit_assembly -- --ignored --nocapture` prints the assembly measurement. `pip wheel . --no-deps` builds the extension the way the released wheels are built, and that wheel installed into a [selkies](https://github.com/selkies-project/selkies) checkout set up as its development documentation describes puts the change under the end-to-end audio suites.

Selkies' [Release](https://github.com/selkies-project/selkies/actions/workflows/release.yaml) workflow builds pcmflux from `crates/pcmflux` and attaches its CPython 3.9+ wheels beside the server artifacts. The tag stamps this crate's literal Cargo version, its standalone lock, and the root lock together. `release-published.yaml` uploads the wheels through PyPI trusted publishing. Before the standalone repository and its Pages site can be archived, a maintainer must register the pcmflux trusted publisher for Selkies' `release-published.yaml` and `pypi` environment and migrate the old documentation destination; neither external registration nor archival is performed by this source tree.

## License

This project is licensed under the **Mozilla Public License Version 2.0**.
A copy of the MPL 2.0 can be found at https://mozilla.org/MPL/2.0/.

[LICENSES.md](LICENSES.md) inventories the third-party components of a built `pcmflux` (crates, the linked libpulse and libopus, what the wheels bundle) with their licenses, and describes the cargo-deny check (`deny.toml`, the root license checks) that keeps the crate graph permissive.
