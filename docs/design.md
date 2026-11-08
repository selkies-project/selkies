---
title: What is Selkies?
description: The problem Selkies solves, where it came from, and the decisions behind its design.
---

**If you simply want to get this project running and do not like reading long text, head to [Getting Started](start.md).**

Have you ever wondered why Windows has Parsec, and Linux does not? Have you ever wanted to obtain full frame on interactive 3D OpenGL/Vulkan/Wine-Direct3D applications or Linux/Wine video games, without relying on proprietary installers or seated server licenses, from the convenience of your web browser that you enjoyed from noVNC or Apache Guacamole? Do you have a web server, reverse proxy, or load balancer in your infrastructure that a web application deployment must pass through?

Have you ever wondered if Parsec, Moonlight + Sunshine, or Steam Remote Play could be exposed over an HTML5 web browser interface without the need to open as many ports? Or ever wondered how GeForce Now or Xbox Cloud Gaming delivered fluid streams in Google Chrome with WebRTC?

None of these capabilities have to be behind proprietary walls, the community can build one!

**Moonlight, Google Stadia, or GeForce NOW in noVNC form factor for Linux X11 and Wayland, in any HTML5 web interface you wish to embed inside, with at least 60 frames per second on Full HD resolution.**

Selkies is an open-source low-latency high-performance Linux-native GPU/CPU-accelerated HTML5 remote desktop streaming platform, for self-hosting, containers, Kubernetes, or Cloud/HPC platforms, [started out first by Google engineers](https://web.archive.org/web/20210310083658/https://cloud.google.com/solutions/gpu-accelerated-streaming-using-webrtc), then open-sourced and developed by academic researchers, [LinuxServer.io](https://www.linuxserver.io), and community contributors. It streams over plain WebSockets by default, with WebRTC available as an opt-in transport.

Selkies is designed for researchers studying Agentic AI, Graphical AI, Robotics, Autonomous Driving, Drug Discovery technologies, SLURM supercomputer or HPC system administrators, Jupyter, Kubernetes, Docker®, Coder infrastructure administrators, and Linux cloud gaming enthusiasts.

While designed for clustered or unprivileged containerized environments, Selkies can also be deployed in desktop computers, and any performance or quality issue that would be problematic with cloud gaming platforms is also considered a bug.

## Motivation

The Linux operating system (OS) has now become more than a server or enthusiast operating system. It has become the operating system of choice for artificial intelligence (AI), machine learning (ML), and deep learning, together with the cloud computing paradigm. In this era, artificial intelligence, especially including large deep learning models, has rewarded massive scale from clustered computing and supercomputers.

The first step of a user working in a clustered computing environment always tends to be some type of gateway. Many use SSH into a login node, and others can use a web frontend interface such as Jupyter to start and manage their workload. But one sector The Linux operating system landscape lacked a proper successor to VNC. VNC allowed GUI applications from clustered environments to be directly from such clusters.

However, because RFB, the protocol for VNC, is based on a 30-year-old protocol, several pitfalls exist, like the Tight encoding format, combining "rectangle, palette and gradient filling with zlib and JPEG" being adequate for partial screen refresh, but not fullscreen refresh and 3D graphics. Several alternative solutions exist for Linux that support video codecs including H.264, and even software encoding on video delivers better performance and efficiency with recent hardware.

Although, such solutions are currently primarily proprietary, requiring license servers and license seats integrated into the infrastructure, and having major issues integrating into reproducible environments such as virtual machine snapshots or containers. Due to using closed-source installer packages for these proprietary solutions, the flexibility to deploy on various niche environments, such as for embedded devices or exotic CPU architectures, also suffers. Other open-source solutions available such as RustDesk are meant for TeamViewer-like remote user support, not for large-scale client-server deployments in the cloud or in clusters.

## Design

Selkies streams a Linux desktop or Docker®/Kubernetes container to a recent web browser with GPU hardware or CPU software acceleration from the server and the client. By default it delivers the stream over plain WebSockets to a WebCodecs-based web client; WebRTC is available as an opt-in transport (`--mode=webrtc`). The default capture backend attaches to an X.Org X11 display; a native Wayland backend (running a headless Wayland compositor, or capturing from a wlroots-based host compositor) is available through the `pixelflux` capture library (toggled with `--wayland` / `SELKIES_WAYLAND`), while X11 remains the default. Mac and Windows server support is planned, but community contribution will always accelerate new features.

This project is adequate as a high-performance replacement to most Linux remote desktop solutions, especially VNC, delivering 60 frames per second at 1080p resolution with software encoding on less than 100% CPU consumption or better on an NVIDIA or Intel/AMD GPU. Selkies, overall, achieves comparable performance to proprietary remote desktop platforms and surpasses those of similar open-source applications by incorporating GPU-accelerated screen encoding and latency-eliminating techniques utilized in web-based WebRTC game streaming platforms.

You may create a self-hosted version of your favorite cloud gaming platform, running on a Linux host with a web-based client from any operating system. Wine and Proton allow your `.exe` Windows application, as well as Windows games, to run with Linux, without the complicated Windows licensing.

There are several strengths of Selkies compared to other game streaming or remote desktop platforms.

**First, Selkies is much more flexible to be used across various types of environments compared to other services or projects.**

Its focus on a single web interface instead of multiple native client implementations allow any operating system with a recent web browser to work as a client.

Either the built-in HTTP basic authentication feature of Selkies or any HTTP web server/reverse proxy may provide protection to the web interface.

Compared to many remote desktop or game streaming applications requiring multiple ports open to stream your desktop across the internet, Selkies only requires one HTTP web server or reverse proxy which supports WebSocket, or a single TCP port from the server. This allows many existing infrastructure previously utilizing noVNC to switch to Selkies without changing other components of the infrastructure.

The default WebSocket transport needs only a single TCP port. If you opt into the WebRTC transport, a dedicated STUN/TURN server for NAT traversal and traffic relaying can be flexibly configured within any location at or between the server and the client.

**Second, Selkies can utilize H.264 hardware acceleration of GPUs, as well as falling back to software H.264 or JPEG encoding.**

Screen capture and video encoding are handled by `pixelflux`, a Rust library the server links, which encodes with hardware NVENC (NVIDIA) or VA-API (Intel/AMD) when available, and otherwise falls back to software H.264 (`x264`, or the BSD-licensed OpenH264 when `pixelflux` is built without GPL components) or JPEG. Audio is captured from PulseAudio and encoded to Opus by `pcmflux`, a companion Rust library. Check the [Components](components/index.md#encoders) section for the current list of encoders and interfaces.

Both the default WebSocket transport and the opt-in WebRTC transport are engineered for minimum latency from the server to the HTML5 web client. NVIDIA GPUs are supported with NVENC, and Intel and AMD GPUs with VA-API, with progress on supporting other GPU hardware. H.264, H.265, VP8, VP9, and AV1 are all encoded in the capture path, and a frame reaches the encoder without a copy wherever the hardware allows: from the compositor's dmabuf on Wayland, and through NvFBC on X11 with an NVIDIA GPU. Additional encoders, interfaces, or protocols may be contributed from the community easily.

**Third, Selkies was designed not only for desktops and bare metal servers, but also for unprivileged Docker® and Kubernetes containers.**

Unlike other similar Linux solutions, there are no dependencies that require access to special devices not available inside containers by default, and is also not dependent on `systemd`.

This enables virtual desktop infrastructure (VDI) using containers instead of virtual machines (VMs) which tend to have high overhead.

Root permissions are also not required at all, and all components can be installed completely to the userspace in a portable way. This indicates that no administrator intervention is typically required for a user to deploy Selkies in any shared infrastructure.

**Fourth, Selkies is modularized, easy to use, and expand to various usage cases, attracting users and developers from diverse backgrounds.**

The runtime is a single [Rust](https://www.rust-lang.org) executable, `selkies`, built around an [`axum`](https://github.com/tokio-rs/axum) server on the [`tokio`](https://tokio.rs) runtime. Media capture and encoding are delegated to two small, self-contained Rust libraries it links directly — `pixelflux` for screen capture and H.264/JPEG encoding, and `pcmflux` for PulseAudio capture and Opus encoding — so the performance-critical code is isolated and reusable, and no interpreter runs in the process. This clean separation lets developers and researchers customize to their own needs, as long as the MPL-2.0 license terms are met.

For the opt-in WebRTC transport, Selkies carries its own WebRTC stack, the `selkies-rtc` crate under `crates/selkies-rtc`, built over the sans-IO `rtc-*` crates of [webrtc-rs](https://github.com/webrtc-rs/rtc), rather than depending on any native WebRTC multimedia-framework stack. Input injection into the X11 display uses XTEST and XFixes through [`x11rb`](https://github.com/psychon/x11rb). The web client is a WebCodecs-based application under [`web/selkies-web-core`](https://github.com/selkies-project/selkies/tree/main/web/selkies-web-core).

Therefore, Selkies is meant from the start to be a community-built project, where developers from all backgrounds can easily contribute to or expand upon.

### HTTP and media stack choices

Axum fits the server's Tower-native architecture: authentication, file routes, and both streaming signaling paths compose as one service, while direct Hyper connections preserve custom TLS, Unix-socket, listener, and WebSocket-upgrade behavior. Encoded buffers reach Axum's `Message::Binary` through shared `Bytes` handles. [Salvo](https://github.com/salvo-rs/salvo) also supports asynchronous `Bytes` bodies and WebSocket streams, so zero-copy is not exclusive to Axum; its integrated HTTP/3 and WebTransport support is a future advantage. Retaining Axum avoids translating well-tested middleware and listener contracts with no demonstrated latency benefit, and preserves the workspace's Rust-toolchain floor. A later WebTransport implementation can reassess the serving architecture independently.

`selkies-rtc` is currently the lower-risk WebRTC implementation for Selkies, not a claim that a custom stack is inherently faster or more mature. Selkies needs interleaved FEC, per-peer reference invalidation and resume, dependency descriptors, absolute capture time, cleanup-frame pacing, shared ICE muxes, and encoded Opus/RED and camera/microphone delivery without transcoding. Keeping those semantics takes a small Rust send path plus a bounded vendored SCTP patch; it does not require new modifications to the much larger native libwebrtc engine.

[LiveKit's libwebrtc binding](https://github.com/livekit/rust-sdks/tree/main/webrtc-sys/libwebrtc) does support pre-encoded H.264, H.265, VP8, VP9, and AV1. Codec passthrough is therefore not the blocker. The open questions are encoded audio, arbitrary per-peer dependency recovery, exact FEC/pacer controls, shared ICE topology, and whether bounded public APIs can expose them without a permanent C++ fork. Musl, C++ toolchain, glibc-floor, binary-size, and every-platform release artifacts also need proof. After parity is stable, a narrow upstream feasibility prototype should answer those questions and compare wire behavior, artifacts, latency, CPU, and frame rate. Replacing `selkies-rtc` is justified only if every existing behavior survives and the measured maintenance or performance tradeoff improves.

**Head to [Getting Started](start.md) to deploy your own instance.**
