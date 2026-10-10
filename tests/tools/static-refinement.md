# Experimental lossless still refinement

## Integrated opt-in

Both dashboards expose **Lossless static refinement** under the existing
paint-over/static-image improvement control. The child is off by default and
requires the parent to be enabled. Its preference is scoped to the display;
turning the parent off retains that preference but stops refinement. The server
setting is `lossless_static_refinement` (`SELKIES_LOSSLESS_STATIC_REFINEMENT`).

This experimental integration requires the scene-identity API in PixelFlux
commit `449cc49ba479f0fb6c564f5f681ca2c03d28dd6a`, on top of
[PixelFlux #46](https://github.com/selkies-project/pixelflux/pull/46), now included in
[PixelFlux #49](https://github.com/selkies-project/pixelflux/pull/49). A wheel built
from unmodified upstream PixelFlux does not provide that API. The Selkies change
must not merge before its native dependency is available; the normal dependency
version and CI wheel build have not been redirected to a private fork.

The supported route is a local Wayland compositor, a full-frame encoder with
verified sample association, WebSocket transport, and either a canvas that
Selkies owns or a page-owned MediaStreamTrackGenerator with compositor callbacks.
Availability follows the actual capture, encoder, and browser sink. The sidebar
shows why other routes are unavailable. Selecting this feature does not switch
capture backends, codecs, renderers, or transports. In particular, native WebRTC
video and worker-owned VideoTrackGenerator sinks remain unavailable, as do
striped video, XShm scene tracking, NvFBC, DRI3, and external Wayland hosts.

The page generator keeps feeding its original video element. A separate PNG
canvas appears only after requestVideoFrameCallback identifies the exact native
sample by its submitted VideoFrame timestamp and dimensions. The bounded ledger
holds 64 metadata entries and no frames. Unknown callbacks withdraw refinement;
they never use a nearest timestamp or the most recently written frame. Incoming
scene changes hide the PNG synchronously before their decoded video can be
submitted on the same page thread. Turning refinement off reveals the video
already running underneath, without a frame clone, video readback, or decoder
restart in the overlay withdrawal itself. The normal settings and pipeline
reset flows still apply and can recreate the decoder. CSS geometry follows the
same scaling and positioning as the video.
Video-only fullscreen and picture-in-picture retain normal video without this
separate canvas; refinement is suspended there. Fullscreen of the containing
Selkies page can include both surfaces.

After a scene settles, the canvas requests its native run/source/scene/sample
identity. The server waits for at least 500 ms of known scene quiet, shares one
capture across consumers, and delivers a bounded PNG behind video using the
existing bulk-transfer pacing. Metadata travels with the exact video payload
through relay selection and decoding. The canvas validates the current scene
again after asynchronous PNG decoding. A newer scene retires the refinement;
later video samples of the same scene do not undo it. This is ordering relative
to received and presented video, not knowledge of future network messages or
physical monitor presentation.

The feature preserves the captured desktop's RGB8 values. It does not recover
10-bit TIFF data or application detail lost before capture. One display caches
one PNG, and each client assembles at most one transfer (64 MiB maximum). While
an obsolete browser decode finishes, one newer compressed PNG may also wait;
this is not a single-PNG bound on total browser memory. A visible refinement
on a canvas video sink retains at most one decoded video-frame clone, or an
initial canvas backup, for restoring video when disabled. The page-generator
overlay retains neither and releases its surface on withdrawal. Already-running native compression and browser
PNG decoding cannot be interrupted; cancellation withdraws admission, transfer,
and presentation of their results. These limits are separate from PixelFlux's
native raw-pixel budgets.

Each requested refinement has an absolute 35-second deadline covering capture,
transfer, and browser decoding. A failed or expired request stops being pending
and cannot present a late PNG. Repeated video samples do not retry that scene;
a new scene or explicit reactivation can request again. The normal video keeps
running throughout, including when no PNG response arrives.

The focused tests exercise the controller, real transport methods, shared
settings, actual embedded worker parser, and renderer state machine:

```sh
python tests/unit/test_static_refinement.py
python tests/unit/test_lossless_transport.py
python tests/unit/test_lossless_settings.py
python tests/unit/test_lossless_static.py
```

The JavaScript wrappers require Node in `PATH`. Run the repository's full
preflight and private browser/GPU suites before publishing a changed source
tree; unit tests do not establish browser scheduling, latency, or visual quality.

### Reproduce the integrated UI path

Use a prepared Linux sandbox with the matching PixelFlux scene API, normal
Selkies dependencies, and the dashboard built from the same source tree. The
controller interpreter needs Playwright, Pillow, and NumPy; the server/fixture
interpreter also needs NumPy and pywayland. Playwright's selected browser must
be installed in that sandbox. No desktop display or existing session is used.

From the repository root, set `PF_REVISION` to the installed native source
revision and run:

```sh
probe_root=$(mktemp -d /tmp/selkies-refinement.XXXXXX)
python tests/tools/refinement_e2e.py --selkies-repo "$PWD" \
  --web-root "$PWD/addons/selkies-dashboard/dist" --dashboard default \
  --backend wayland --transport websockets --browser firefox \
  --revision "$PF_REVISION" \
  --runtime "$probe_root/runtime" --output "$probe_root/results" \
  --strict --require-supported
```

For Wish, select `addons/selkies-dashboard-wish/dist` and `--dashboard wish`.
`--server-python` can name a separate server interpreter, and the optional
`--browser-endpoint-file` connects to a private Playwright run-server with a
matching client version. The revision argument is a label; the report separately
records installed module hashes, source state, webroot hashes, and script hashes.
`--width 3840 --height 2160` exercises the same path at 4K. X11 negative-capability
checks require Xvfb. WebRTC and other unavailable sinks are reported explicitly;
`--require-supported` makes an unavailable route fail a requested positive check.

The probe clicks the actual sidebar controls and preserves normal codec and
renderer selection. It compares visible canvas RGBA8 to a deterministic SHM
producer across source changes, parent/child toggles, resize, and reload. A
Wayland producer follows the selected output's integer scale and allocates its
pattern at physical buffer dimensions, applying `set_buffer_scale` to preserve
the configured logical size. Its recorded commits include logical size,
physical size, and scale; they are producer observations, not capture or browser
presentation timestamps. Fractional-scale rendering is not established by this
integer-scale fixture. A
separate instrumented phase delays PNG decoding, then observes the original
bitmap draw/close calls to test cancellation and an old decode crossing a newer
presented scene. That phase includes a positive PNG-paint control. It is not a
latency benchmark, physical-display measurement, or proof over every possible
browser schedule.

Each invocation requires fresh runtime and output directories. The server uses
a private home and XDG directories, binds loopback only, and restricts file
manager access to its output directory. The script reaps its owned process
leaders; run it under the sandbox's supervisor when process-group cleanup is
required. Results and screenshots can contain local paths and the isolated
fixture desktop; inspect them before sharing.

## Historical standalone fixture

The earlier standalone review fixture remains available for reproducing the
historical studies below. It is separate from the integrated opt-in above.
It preserves the live video and temporarily places an RGB snapshot over it. A new
presented video frame, input, resize, or video-style change removes the snapshot.
Pending responses are rejected when the client observes such a change, when a
newer request supersedes them, or when their dimensions differ from the video.

The historical capture contract proposed in
[pixelflux's output-capture RFC](https://github.com/selkies-project/pixelflux/issues/45)
motivated the limited integrated route described above. It defines output
identity, scene and capture ordering, explicit precision, cancellation, and scheduling. Its scene
identity must distinguish content changes from repeated captures of unchanged
content; a later paint-over frame must not by itself suppress a valid refinement.

The default probe tests the actual ES module with a manually advanced browser
video stream and a deterministic RGB pattern. It checks exact canvas pixels,
fractional geometry at DPR 2, late responses, superseded requests, error paths,
and disposal. It does not require a Selkies server or scientific dataset:

```sh
python tests/tools/static_refinement_probe.py --output /tmp/refinement-checks
```

Use the repository's prepared test environment, including Playwright and
Chromium. To inspect the existing screenshot API, point the probe at an isolated
test session with one display and a visible HTML video sink:

```sh
python tests/tools/static_refinement_probe.py \
  --url http://localhost:8080/ --mode websockets --headed \
  --output /tmp/refinement-live
```

Use `--mode webrtc` for that transport. `--token-file` optionally reads a session
token from a private file; no credential is built into the fixture. The snapshot
request stays on the page's origin. `--api-path` specifies an endpoint relative
to the page for a deployment whose prefix differs. `--browser-endpoint-file`
optionally connects to a private Playwright run-server in the prepared sandbox.

The window has **Refine still** and **Show video** buttons and closes after 120
seconds, adjustable with `--seconds`. Turn off continuous streaming through the
test session's normal controls and let the image settle before requesting a
snapshot. `--capture-once` waits for client-observed quiet, clicks the same button,
saves the result, compares the overlay pixels with the returned PNG, and exits
with failure if the capture was rejected or differs. Each run
writes `results.json` and a browser screenshot to its output directory. These can
contain the test desktop and should be inspected before sharing.

Live results include decoded dimensions, the CSS box, DPR, fetch time, and decode
time. `readyMs` ends when the canvas has been drawn; it does not measure display
presentation or input-to-photon latency. Keep dimensions, scene, encoding policy,
network conditions, and concurrent load equal before comparing timings.

## What the prototype establishes

An RGB capture can finish a still image at the pixels that the application
rendered. This complements the existing `use_paint_over_quality` behavior:
paint-over already refreshes still regions at `video_paintover_crf` or
`paint_over_jpeg_quality`. The relevant comparison is with that cleanup enabled
at an appropriate quality, not only with a low-quality video stream.

In the preceding controlled experiment, both transports under comparison read
the same Fiji/X11 framebuffer, using one grayscale slice and a synthetic color
pattern. The anatomical mask contained 291,246 pixels. Native-scale mean absolute
RGB error on the 0–255 scale was:

| Path | Foreground MAE |
| --- | ---: |
| Selkies H.264 CRF 25 | 2.482925 |
| Selkies H.264 CRF 25 with paint-over CRF 5 | 0.350594 |
| Selkies H.264 CRF 5, new frames confirmed | 0.350594 |
| Selkies H.264 CRF 0, new frames confirmed | 0.166097 |
| Selkies JPEG 100 | 0.093378 |
| TurboVNC/noVNC JPEG quality 9 | 0.092726 |
| TurboVNC Tight without JPEG | 0 |
| PNG RGB refinement | 0 |

These are measurements of the earlier laboratory prototype on commit
`289e48edd0493b14f578961088063b2beea57e2c`, combining the geometry and antialiasing
fixes. They are not a benchmark of this newly packaged fixture. The same
Selkies PNG API matched all 810,000 pixels of the full Fiji region after moving
the cursor outside it. Its median request-to-ready time over six loopback samples
was 50.9 ms for 1,014,173 bytes; this excludes the idle wait and says nothing about
input latency, WAN behavior, or relative encoder efficiency.

## Precision and study protocol

The live check compares two decoded 8-bit canvas images. It establishes equality
at that boundary, not preservation of a higher-depth source: both readbacks can
lose the same information and still compare equal. The historical screenshot
fixture and the active snapshots used by this integration preserve eight bits.
The separate standalone X11 depth-30 screenshot path now emits RGB16 with ten
significant bits; it is not the source for this integrated refinement.
Encoding the video at 10 bits does not make an RGB8 source a native
10-bit capture, and an application may already map a higher-depth TIFF into an
8-bit display range before capture.

A precision study needs all 1,024 10-bit values, adjacent one-level differences,
an independent full-precision reference, and separate decoder/rendering checks.
PNG16 can transport 10 significant bits reversibly, but neither PNG16 nor a
successful 10-bit video decode guarantees high-precision canvas or monitor
presentation. Test the actual allocation and pixel values, rather than accepting
an API option as proof. SDR precision, HDR/color management, and physical display
precision are separate results.

Run the synthetic precision probe with the existing Playwright dependency:

```sh
python tests/tools/static_refinement_precision.py \
  --output /tmp/refinement-precision --require-high-precision
```

It generates an RGB PNG16 fixture with ten significant bits, checks its structure
and original integer codes, and compares multiple decoding routes against those
codes. It reports `preserved`, `measured-loss`, and `unsupported` separately,
including actual canvas backing/readback types. `--require-high-precision` fails
unless one complete decoding/rendering route recovers every ten-bit code; without
that flag a completed capability survey is not a ten-bit pass. The output always
states that native capture and physical display precision were not verified.
`--browser-endpoint-file` selects the prepared remote Chromium fixture using
SwiftShader; `--executable-path` selects a local isolated Chromium executable.

The two-canvas equality control deliberately demonstrates how an 8-bit equality
check can succeed after both images lose distinctions. High-precision recovery
means the original ten-bit codes are recovered by rounding the normalized
readback, not that all PNG16 or floating-point bit patterns remain identical.

Repeat the historical table on frozen current Selkies, pixelflux, and pcmflux
revisions before using it as a current product comparison. Record wheel hashes,
effective settings, browser/driver versions, and source equality. Compare against
properly configured paint-over and both JPEG and lossless TurboVNC. Confirm fresh
frames after changing quality; identical stale frames are not a quality result.

Expand the scene set beyond one anatomical slice: fine text and colored lines,
dark and bright tissue, gradients, and motion followed by stillness. Compare
native scale separately from HD/4K, DPR 1/2, and fractional scaling against a
declared reference filter. Cover two equal-size outputs, moved crops, reconnects,
out-of-order responses/video, and resumed interaction during refinement.

Measure the time from last damage to verified refinement, distinguishing the
idle policy, capture, compression, transfer, decode, and presentation stages.
Also measure input latency, frame pacing, unrestricted frame rate, CPU/GPU,
memory, and bytes under controlled bandwidth, RTT, and loss. Randomize repeated
paired runs and report uncertainty; frames from one cycle are not independent
repetitions. Draw completion is not physical presentation, and a loopback result
does not establish WAN behavior.

## Limits of the historical standalone fixture

- The screenshot API includes the cursor and captures the X11 root across
  outputs. The probe rejects a size mismatch; it cannot determine that a same-size
  capture belongs to the correct output. Cursor-free, output-scoped capture
  belongs in pixelflux before it can be consumed here.
- Client frame counters are not server scene generations. They cannot rule out a
  stale capture whose successor video has not arrived yet. Reconnects, display
  changes, and out-of-order captures need a server-side ordering contract.
- The fixture targets a visible HTML video sink. It does not integrate worker
  canvas sinks, either dashboard, automatic idle policy, or tile-based delivery.
- RGB equality is against the already-rendered desktop. It neither reverses an
  application's downsampling nor makes CSS smoothing an ideal scientific filter.
- Zero error matches a lossless reference. Beating TurboVNC's lossless mode on
  latency, bandwidth, CPU/GPU use, or refinement time requires a matched benchmark.

Broader integration remains future work beyond the opt-in route above. Making
automatic refinement a default requires coverage across transports, capture
backends, dashboards, and supported sinks, followed by input-latency and
frame-pacing measurements. The current limited route does not establish that
parity or justify changing the default.
