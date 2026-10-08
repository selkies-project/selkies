# Experimental lossless still refinement

This is a review fixture, not a feature enabled in a streaming core or dashboard.
It preserves the live video and temporarily places an RGB snapshot over it. A new
presented video frame, input, resize, or video-style change removes the snapshot.
Pending responses are rejected when the client observes such a change, when a
newer request supersedes them, or when their dimensions differ from the video.

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

## Limits that prevent integration

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

The integration proposal reuses the existing paint-over damage/idle information,
keeps lossless encoding off the interactive path, and bounds and cancels pending
work. It requires equivalent behavior across both transports, capture backends,
dashboards, and supported sinks, followed by input-latency and frame-pacing
measurements before any automatic mode becomes a default.
