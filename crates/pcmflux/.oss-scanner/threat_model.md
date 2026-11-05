# Threat model

## What this project does and where untrusted input enters
- pcmflux is the audio half of Selkies' media path, a Rust/PyO3 extension (`pcmflux/src`): it captures PulseAudio or
  PipeWire audio, encodes Opus (silence gating, RFC 2198 RED redundancy, multichannel), hands frames to Python and to
  an Ogg Opus Unix-socket fan-out (`ogg_sink.rs`), and its `AudioPlayback` decodes Opus, possibly RED-framed, into a
  PulseAudio sink.
- Untrusted input, weakest principal first:
  - a remote browser's microphone packets, which Selkies passes to `AudioPlayback.write` and `write_red`: the RED
    parser (`decode_red_into_queue`) and the Opus decode;
  - other local accounts, through the Ogg sink's socket, whose path Python supplies;
  - the PulseAudio or PipeWire daemon's replies, semi-trusted.
- The Python embedding (Selkies) has full authority by design. `example/` is a localhost demonstration.

## Components that matter most / least
- Most: memory safety in the `unsafe` code (the Opus FFI, the buffer protocol exports, pooled buffers written in
  place), the RED parser, and the bounds of the playback queue.
- In scope but lower: the Ogg sink's socket handling, values the audio daemon reports, the CI workflows.
- Out of scope: `example/`, and libopus itself (report a misuse of its API here, not its own bugs).

## How to exercise it
- `cargo test --manifest-path pcmflux/Cargo.toml` runs offline, including the RED framing round trips and the Ogg
  page builder. The RED parser and the decode path can be driven from unit tests without an audio daemon.

## How you rate severity
- Critical: memory corruption reachable from remote microphone packets.
- High: a remote packet that hangs or crashes the process (it is the streaming server's); memory corruption from
  another local account.
- Medium: another local account replacing or reading the Ogg socket; unbounded memory from a peer.
- Low: hardening without a demonstrated crossing.

## Anything to leave alone
- Python arguments and environment variables come from the operator and are trusted.
- Reports should name the file and line, give a reproducer (a unit test is ideal), and propose a patch.
