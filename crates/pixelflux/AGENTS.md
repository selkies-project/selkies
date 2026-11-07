# Working on this repository

pixelflux is the screen-capture and video-encode library in Selkies' `crates/pixelflux`, beside the server and
[pcmflux](../pcmflux) (audio capture and encode). Native changes and callers land in one commit.

Use web search, web fetch, and other available tools as necessary. Make sure that the comments or documentation are
not too verbose (do not add comments more fit for a PR summary than a comment). Do not leave arbitrary numbers (such
as issue or task numbers) in the code or documentation. Do not use inline comments. Do not use comments or
documentation that describe arbitrary code changes of previous states compared to the current code that do not need
explanation. The code commenting should reflect the current state of the codebase and be used to convey information
to an LLM bot or developer. Write American English -- color, behavior, center, initialize, canceled -- except
where a name belongs to something upstream, such as a Wayland `Cancelled` event or an NVENC `colourMatrix` field.

pixelflux is a Rust library first and a Python extension module second: `src/python.rs`, behind the `python`
feature, is the one place that names PyO3, and every class there wraps a public Rust type (`CaptureSettings`,
`ScreenCapture`, `Compositor`, `webcam::VirtualCamera`) or a public function, converting its arguments, calling
with the GIL released, and mapping `Error` to the exception Python raised before. Frames, cursor images, and
clipboard contents reach a consumer through the `sink` traits; the Python callbacks are sink implementations.
A capability a Rust consumer needs is added to the Rust API and then wrapped, never the other way around, and
nothing in Python-facing code is reachable only from Python. setup.py builds the wheel with `--features
python,extension-module`; `cargo build` alone links no interpreter.

Empirical testing is possible for everything here, including implementation, auditing, validation, and verification,
and every change is validated before it is reported. `cargo test --lib` in every feature configuration (the default,
`--features python`, and `--no-default-features --features openh264`) and
`pre-commit run --all-files` (rustfmt, clippy in both the default and `python` configurations, ruff, and codespell) are the floor;
the `#[ignore]`d `gpu_` tests need an NVIDIA GPU (`cargo test gpu_ -- --ignored --nocapture --test-threads=1`,
serially, since concurrent session builds fault in the driver), the `gpu_dmabuf_` ones a render node as well, and the
`gpu_bench_` ones print measurements to quote rather than assert. The VA-API surface probe (`vpp_sw_formats`)
runs only where a VA-API device opens, so the CI runners and NVIDIA hosts never test it; it is proven on Intel
hardware in the selkies sandbox. End to end, a change is a wheel
(`pip wheel . --no-deps`) installed into a selkies sandbox as the Agentic Development section of that repository's
`docs/development.md` describes, driven by its suites over both transports on X11 and Wayland with the installed
Firefox and Chrome and Playwright/Selenium/Puppeteer/Cypress WebKit in place of Safari; Selkies' root
`.devcontainer` builds the extension with its native dependencies. Ask before building an environment on a machine that was not set
up for one (Miniforge serves a host with a closed package manager; keep the system `libgbm.so` for GBM on NVIDIA and
other GPUs) and take the operator's directives on how it is constructed and constrained. Say which checks could not
run where the hardware for them was not available.

`../../.oss-scanner/` serves Anthropic's OSS Scanner. Its unified `threat_model.md` covers pixelflux, where untrusted bytes
enter from the weakest principal up, what matters most, how to exercise it, and how severity is rated; `Dockerfile`
builds the crate and runs its tests offline the way CI's cargo job does. The threat model is the rule the code is
held to, not a description of it: where the code lets an input reach what the model keeps from it, the code is the
defect even when its documentation and a test agree with it, and the fix corrects all three. Anyone, an agent
included, may change the rule to harden it (a principal reaching less, a boundary it did not state yet); loosening
it is a maintainer's call. Keep the model current in the commit that moves it: a new socket, server, file written,
or parser of bytes pixelflux does not write itself joins its entry points. Carry a new system package the build or
the tests need into `Dockerfile`.

`../../.clusterfuzzlite/` builds `crates/pixelflux/fuzz` (`cargo fuzz`) for ClusterFuzzLite
(`../../.github/workflows/fuzz.yaml`: changed code for five minutes, every target for an hour a day)
and OSS-Fuzz. Code that parses bytes pixelflux does not write itself (a bitstream an encoder emits, a webcam frame, a
request, a protocol message) gets a target in `fuzz/fuzz_targets` that compiles the modules it fuzzes through
`#[path]`, so it needs none of the crate's system libraries, with seeds in `fuzz/seeds/<target>/` and its source
among the workflow's `pull_request` paths. A crash the fuzzer finds is fixed at its cause, and its input joins the
seeds.

CodeQL's code scanning and the other checkers (clippy, rustfmt, ruff and codespell through pre-commit; cargo-deny's
advisories, licenses and bans; Dependabot's alerts; the OSS Scanner's and the fuzzers' findings) are at least
stylistic recommendations: a finding is fixed in the code and its alert closed by the next scan or update, never
dismissed or closed by hand, and a lint is not silenced (`#[allow]`, `noqa`) where the code can be written its way.
Code that is safe through a check a checker cannot follow (a validator in another function, a bound set elsewhere) is
rewritten so the check sits where it is recognized (CodeQL's `*Customizations.qll` in github/codeql lists the forms),
so the checker keeps watching that path. A protocol requirement alone is dismissed, with the standard that fixes it
named (a hash, port or file format the protocol or its tools prescribe).

Priority: End-to-End Processing Latency > Resource Usage or Unrestricted Frame Rate >= Quality (non-perceptible or
statistically insignificant latency fluctuations of up to 5% are acceptable in exchange for quality or bandwidth
consistency, and slightly more GPU or CPU usage is also acceptable if it has no latency impact and brings a
substantial quality improvement) >> Overall Bandwidth Efficiency (since encoded frames are only used once, unlike
.mp4/.mkv)

Note that parity between X11 and Wayland, as well as between WebSockets and WebRTC, between the default dashboard
and the wish dashboard, or across encoders and codecs (the hardware encoders, NVENC, VA-API, V4L2 M2M and Tegra, and
the software ones, x264, OpenH264, x265, kvazaar, libvpx, SVT-AV1 and libjpeg-turbo; H.264, H.265, AV1, VP8, VP9 and
JPEG), is considered a key focus (things that were not wired up correctly on either side, and similar discrepancies,
are subject to fixes or deduplication; a fix or a feature made for one encoder or codec is checked against every
other its mechanism applies to and carried there, or the reason one cannot take it is stated). I prefer
deduplicating code that performs similar purposes across different modes over keeping duplicate code for no reason
and more fragility. Refactor through deduplication if you are confident there will be no regressions (or able to
validate regressions). Screen coroutine usage in both
Python and JavaScript, as well as thread usage in all languages, so that everything is performant and does not lead to
hanging or lagging. Performance preservation or improvements such as zero-copy and latency-reducing measures are
always important, and the GIL is held no longer than the work needs. End-to-end latency and an unrestricted frame
rate are separate goals rather than two ends of one dial: neither is spent to buy the other. A change never drops a
capability or falls back to an older implementation to make itself simpler; where one seems to be in the way, say
what it is rather than removing it. Note that compatibility should be ensured for Python 3.9 to 3.15 or even higher, and CUDA/NVENC 11
to 13 or higher. Protocol clients form fallback ladders that bind the newest architecture first (ext- before
zwlr-data-control in dcclient) and exist to keep selkies' Wayland path subprocess-free — they replace wtype/wl-copy
style forks, so extend them in-process rather than shelling out. A nested KWin session forwards no delta from its host
seat, so relative pointer motion reaches it through `org_kde_kwin_fake_input` on the app compositor socket
(`wayland/ficlient.rs`); wlroots sessions take the seat's `zwp_relative_pointer_v1` as before. Update the translations as well (and write/update additional entries if necessary) as necessary.
A defect that predates the change you are making is still in scope: finding it does not make it someone else's,
and "pre-existing" is not a reason to leave it. Fix it, or say precisely what is broken, what you ruled out, and
what you would do next. The same applies to a failure you cannot reproduce yet -- narrow it until it is either
fixed or precisely described, and never let a test that fails for an unknown reason pass unremarked.

A change is ready when four questions have answers, and the commit or pull request gives them to the reviewer:
was the defect, or the missing behavior, reproduced on the code before the change (a failing check or a measurement
on the old tree, not an argument from the source); is it gone, or present, on the exact code being committed, through
the path a user takes rather than a switch a user would never flip (a developer toggle, a debug key, a knob of the
rig); can the change affect behavior it was not aimed at, and what was run to know; and is the change stripped to what
makes it work, since every line the first two answers do not need is noise the maintainers have to sift. A change in an
area a maintainer has said they are working on goes to a branch and a pull request carrying those answers, never
straight to `main`, whatever standing permission to push `main` exists. An issue is closed by a
maintainer, never by you. A pull request's `Closes` keyword is not you closing it; the maintainer's
merge is. An optional path another component may offer
(a protocol a compositor advertises, a driver feature, a device) is taken only when its presence is detected and never
as the default: that it is exposed is not proof it works, and a reviewer has to be able to tell what runs where.

The codec of a capture is `CaptureSettings.codec` (`jpeg`, `h264`, `h265`, `vp8`, `vp9`, `av1`), the
`encoders::Codec` enum in Rust: it carries the wire id (the high nibble of a `0x04` frame's type byte, the
low nibble being the frame kind), the per-codec quantizer domain the shared `video_crf` index maps onto, the
level ladders, and the bitstream reads that label frames. A session advertises its stream's level from the
shared ladder at the current geometry (`codec.rs`), the lowest a decoder is asked to accept, so a hardware
decoder that gates on the level — older Apple and Intel parts refuse a level above their ceiling even for a
picture they could hold — takes the stream; NVENC re-declares it with a forced IDR on each in-place resize, and
holds AV1 alone at the resize headroom's level, which NVENC validates its session against at init. JPEG
stripes (`encoders/software.rs`); every video codec streams whole frames. Every session that can name what a frame predicts from
does (`encoders/reference.rs`, `StripeFrame.reference_frame_id`), and `invalidate_reference`
leaves a frame a consumer lost out of the predictions so recovery costs no keyframe: libx264 and libvpx
always (VP9 in libvpx's flexible reference mode, VP8 across its three buffers on a schedule `reference.rs`
keeps), NVENC where the device reports reference-picture invalidation, VA-API where the driver takes the
session's own H.264 or HEVC slice headers or addresses the VP8 buffers and the VP9 and AV1 slots, Tegra for
H.265 through the vendor's external reference set (L4T R32 and R36), SVT-AV1 at a constant rate where the release
takes reference commands (4.2 on); and the stream declares the decoded picture buffer its level admits (H.264 and
H.265 no more than `video_reference_frames`, for a decoder that holds its whole buffer before it shows a picture, as
GStreamer's hardware decoders in WebKitGTK and WPE do), or for AV1
a fixed eight whatever the level, four under NVENC, as many as a frame predicts from (eight work there too, at 112
MiB more a session). NVENC keeps long-term
frames of that buffer as anchors where the device offers them (H.264 and AV1 one, H.265 two), and VA-API one in
H.264 where the driver codes from the long-term reference it is named (radeonsi's VCE, measured; the session writes the
marking and the list modification into its own slice headers, `keeps_h264_anchors`), so a loss older than
every recent frame, or than a buffer a resize left small (the driver lowers the buffer in place but never raises it),
is still predicted past. H.264 keeps one in every case: NVENC's H.264 fallback takes the first alone, and OpenH264's
decoder, Firefox's for WebRTC H.264 where no FFmpeg decodes it, finds a CABAC stream's long-term picture by a number
no marking sets, so it reaches the first index alone and decodes a frame naming a second against another picture
without an error. Where the consumer says which frames every client holds or was sent (`acknowledge_references`,
`acknowledge_reference`, `ReferenceWindow::acknowledge`), AV1 keeps two as well, and several clients share one session
with each recovering on its own: of two anchors, the newest all of them hold is pinned while the schedule marks the
other, keeping one all of them were sent until they hold it (for up to four anchor periods), and a lone anchor all
of them hold is not marked over while frames all of them were sent go unheld, as through a client's outage, nor
while no recent frame is one all of them were sent, since it would predict from the one it marks over (for up to
eight anchor periods); an anchor within an anchor period of the newest frame all of them were sent first forgets the
frames after it, so it predicts from that one (`settle`), and carries `FRAME_ANCHOR` in its kind; a frame predicting from an anchor
names it (NVENC's H.264 fallback takes the first otherwise); an AV1 anchor slot holding no frame, or a lost one,
counts among the recent frames, as NVENC lends it to them (`lend_free_anchors`), so the window names the frame the
device predicts from; and in H.264 the frame at the `frame_num` wrap is an anchor reaching back however far. libvpx and
SVT-AV1 keep their anchor buffers alike once told (`ReferenceSlots::acknowledge`): the one holding the newest frame all
of them hold is pinned, one they were all sent is kept until they hold it, a frame predicting past a loss from an
anchor refreshes another, and an anchor on the schedule predicting from a frame all of them were sent carries
`FRAME_ANCHOR`. A session that does not
(x265, kvazaar, SVT-AV1 at a constant quantizer or before 4.2, Tegra's H.264 and AV1, a stateful V4L2 device)
refuses, and the caller forces an IDR instead; an H.264 session answers a loss covering the frame at its `frame_num` wrap with a key frame itself,
since FFmpeg's decoder, which Chromium, Firefox, and WebKit decode with on Linux, derives the picture order past that gap a wrap short and drops about a `frame_num` range of pictures after it; libx264's sixteen values and NVENC's 256 go out a byte wider (`encoders::sps::WideFrameNum`), so that frame comes once in 4096 or 65536. Every full-frame session is chosen by one ladder,
`encoders::select_frame_encoder` (Tegra's vendor encoder where its library answers, then NVENC on the NVIDIA
driver, VA-API otherwise, then a stateful V4L2 memory-to-memory device, then the codec's software encoder, then
a demotion to H.264), shared by X11, Wayland zero-copy, and Wayland readback.
The V4L2 step comes after the render-node probes, since a machine carrying either backend never reaches it, and
the boards it serves -- the Raspberry Pi's `bcm2835-codec`, RK356x's hantro, i.MX8M's VPU -- publish no driver
for those probes to select. It is reached by asking for the interface (`V4L2_CAP_VIDEO_M2M` and the codec's
fourcc on the capture queue) rather than by naming a board, so a device nobody here has is served on the same
path, and the size is checked before the node is opened because a refusal afterwards leaves a session that
produces nothing. Both backends take the codec as
a parameter rather than carrying a second copy of the interface: the queues, controls, and surface formats are
the same whichever coded format the capture queue is set to, so a device that advertises H.265 serves it there.
Only the picture type is codec-specific, because an H.265 NAL header is two bytes where H.264's is one. `encoders/nvenc.rs`
is codec-parameterized (H.264, HEVC, AV1; a codec the GPU lacks is refused at open). `encoders/vaapi/` is
the VA-API session, driving libva directly (`va-sys` carries bindings of the libva headers it vendors and opens
the host's library at run time) for all five codecs: `mod.rs` holds the device, the surfaces, the video
processor's convert, and the reference bookkeeping, one arm per codec writes the parameter buffers and, where
the driver takes them packed, the H.264, HEVC, and AV1 headers themselves (`bits.rs`), which is how a session
names the frame a picture predicts from; a 4:4:4 session tries the surface formats the
driver allocates and its video processor renders, read through libva's `VAProfileNone`
configuration, until one survives the surface pool, the convert, and the codec open (Intel's iHD
allocates planar 444P but its VPP writes 4:4:4 only packed, as XYUV, and its HEVC 4:4:4 entry point
takes only what the VPP writes), and `video_bit_depth` is negotiated the same
way, by the profile the driver lists for the format (HEVC Main 10 and Main 4:4:4 10 on `p010` and `y410`
surfaces, VP9 profiles 2 and 3, AV1's main profile at 10 bits), a session staying at 8 bits where the driver
lists none and no software encoder codes them either. Where one does (`encoders::software_ten_bit`: x264,
x265 with its 10-bit encoder linked in, libvpx built with high bit depth, SVT-AV1), a 10-bit request an
engine cannot serve is the software encoder's, as a 4:4:4 it lacks is (`hardware_lacks_ten_bit`), converted
from the 8-bit source straight to 10-bit samples (`software::convert_to_yuv10_mt`); `pixelflux.SOFTWARE_FORMATS`
and `pixelflux.hardware_formats` reports what a node carries; `vaapi/mock.rs` stands in for a driver so the session's every buffer is checked
without hardware. The software HEVC (x265 with the `gpl` feature, else kvazaar; `encoders/hevc.rs`), VP8/VP9
(libvpx, `encoders/vpx.rs`), and AV1 (SVT-AV1, `encoders/svtav1.rs`) encoders are linked directly and bound at
build time from the headers of the copies that are linked (`codec-sys`), so which library serves a codec is the
build's (`encoders::software_encoder`, exported as `pixelflux.SOFTWARE_ENCODERS`). Software H.264 is
resolved at build time, never by a setting: the default `gpl` feature makes libx264 the encoder behind every
CPU H.264 session, and a build without it (`PIXELFLUX_ENABLE_GPL=0` →
`--no-default-features --features openh264`) puts Cisco OpenH264 behind the same CPU path
(`encoders/oh264.rs`) with the same wire framing; selkies reads the exported names to
name the encoder and to keep a WebRTC offer on OpenH264 at the 4:2:0 profile. Every session, on every backend,
converts with the BT.709 matrix — the sRGB
desktop's own primaries and transfer — at limited range for 4:2:0 and full range for the software
4:4:4 sessions, and declares it; VP8 is the exception its bitstream forces, one color-space bit
whose only defined value is BT.601 — told BT.709 out of band, Firefox reads neither the decoder
configuration nor the RTP color-space extension and inverts the other matrix. A device that
converts in fixed function is the other exception, and only where it is probed and found to
discard the colorimetry it is handed: that session declares the matrix and range the firmware
produced rather than the one asked for, and one that honors the fields converts BT.709 limited
like every other. What a session says of itself therefore comes from `session_full_range`, never
from whether its encoder is hardware, so no two descriptions of one session can disagree. Chroma sits at
the center of each 2x2 block, which NVENC's own conversion does not: it weights the two columns
of a block 3:1 (its matrix follows what the session declares, measured on Volta and Pascal, so
only the siting is at stake). A 4:2:0 session therefore converts with `ChromaConvert`, a PTX
kernel the driver JIT-compiles (`encoders/argb_to_nv12.cu`, `scripts/build-ptx.sh`) that reads
the packed surface, a pitch-linear dmabuf import, or a texture over an array-typed one and writes
the NV12 NVENC encodes; 4:4:4 subsamples nothing and keeps the hardware conversion, as does a
driver that refuses the kernel. A 10-bit NVENC session (HEVC and AV1 where the device reports
`NV_ENC_CAPS_SUPPORT_10BIT_ENCODE`, from API 12.2, which names a session's input depth; H.264
stays 8-bit and a 10-bit request for it is the software encoder's) converts through the same
module's 10-bit kernels, to P010 or planar 4:4:4 (`ConvertLayout`), at either chroma: NVENC
upconverts an 8-bit surface itself, but from samples already rounded to eight (a 4:4:4 HEVC
session coded that way came out under its 8-bit self, the luma half a level low), where the
kernels leave a flat or graded picture a quarter of the 8-bit conversion's error
(0.07 against 0.28 of an 8-bit level, RTX 3060). NVENC's quantizer runs 0 to 51 at either depth,
so on text at one quality index the two depths code alike.
`VideoDecoder::color_tags` reads what a stream declares, and the unit tests hold each
encoder to it; the sequence headers travel with every IDR so a client joining or resynchronizing on any key
frame can decode, which `encoders/v4l2m2m.rs` keeps true itself for the devices whose drivers will not
(`REPEAT_SEQ_HEADER` is asked for and the parameter sets are put back where it is refused), since neither
FFmpeg's nor GStreamer's M2M encoder guarantees it. Every H.264 stream bounds reordering at zero in its SPS, since
Chromium's hardware decoders hold a whole decoded picture buffer back where a stream declares no bound: the software
encoders and NVENC write the restriction, and the sessions whose device or driver writes its own SPS (VA-API, V4L2
M2M, Tegra) write it back where it is missing (`sps::bound_reorder`), each only where it proves its pictures leave
in display order; the unit tests hold each encoder to it. Every session takes the capture's frame rate as the fraction
it names (`encoders::frame_rate`: 59.94 is 60000/1001), never a whole number, so its rate control and the timing its
stream declares hold to the rate the capture is paced at. Encoder settings are chosen by measured latency first, frame rate second, quality third, and
bitrate last: every software encoder runs at the fastest setting its library offers in real time (x264
ultrafast, with the uneven multi-hexagon motion search in place of its diamond, which costs a tenth of a
millisecond and finds the scrolls and moved windows the diamond codes as new pixels, VP8 speed 16, VP9 speed 8 with screen tuning, SVT-AV1 preset 11 in its real-time mode, a frame
predicting from an anchor alone at preset 10, since preset 11 leaves some blocks the picture changed since the anchor
as they stood in it (`ANCHOR_PRESET`), x265 ultrafast with wavefront threads; each full-frame software session, and the conversion ahead of it, on the
threads `encoders::session::encode_threads` names, one less than the cores up to eight, four for H.264's slices,
or one alone where `cpu_encode_threading` is off, which also codes JPEG's stripes one after another;
a Turbo frame keeps its hash beside the encode either way), NVENC at preset P3 with two-pass quarter-resolution rate control
(`gpu_bench_tuning` measures the alternatives), and VA-API at the fastest quality level the driver takes, on the low-power entry point where the driver lists one, except H.264 on Skylake and Broxton, whose low-power encoder codes a picture as one slice whatever it is handed and reports nothing of it (told by the kernel's PCI id; the full entry point first, the low-power one in a single slice where the driver build carries no other); the VP8, VP9, and AV1 quantizer tables in `codec.rs`,
and the hardware ones, were measured at those settings and must be re-measured whenever they change
(`encoders::codec` documents the method; `cleanup_bench` with `PF_BENCH_SSIM=1` measures it). Intel's low-power
HEVC encoder codes no quantizer under 10 and codes a slice naming a finer one at 10 regardless, which decodes washed
out, so a constant-quantizer session there is floored at 10 (`vaapi::h265::INTEL_LOW_POWER_MIN_QP`); its H.264
codes such a slice at its own floor, header and all, and its rate control stays at or above it by itself, so neither
takes the floor. Intel's full HEVC encoder (Gen9, Skylake to Comet Lake, which lists no low-power one) gets the
headers FFmpeg codes there: where it reports no block sizes, blocks from 8x8 to 32x32 (Skylake's) on surfaces still
aligned to 16, and one quantizer delta per tree block. Its rate control codes its quantizer in every coding unit as a
delta from the slice's, so a constant-rate slice there names the middle of the quantizer range (26 at 8 bits,
`vaapi::h265::centered_qp`) and the rate control is bounded at 51: from 30, a unit under 4 needs a delta past the
syntax's -26, which decoders refuse or misdecode. VP9 carries 4:4:4 as profile 1 (libvpx 1.13 on) at the same limited range as its 4:2:0, so the decoder hint the
client sends for it stays true. The CBR
sessions of x264 and x265 cap the quantizer at 51: both libraries default to an out-of-spec range above
it that forces macroblock skips on a VBV underflow, which freezes rows of a screen for a few frames, so a
budget the content cannot meet overshoots instead, as NVENC and libvpx do. Test both configurations (`cargo test --lib` and
`cargo test --lib --no-default-features --features openh264`, the latter with kvazaar's headers installed);
the OpenH264 crates are dependencies of every build, since their decoder is the virtual camera's H.264 decoder,
so the OpenH264 encoder's tests run under the default build too. The
wheel recipe (`pyproject.toml`) builds libvpx, SVT-AV1, dav1d, libde265, and x264 and x265 for the GPL wheel or
kvazaar for the non-GPL one from source.
libx264 is built from the commit the recipe pins with the patches under `patches/x264` applied, since pixelflux reads
the API they add: every other build (CI, the devcontainer, the scanner image, Selkies' packaging) takes it from
`scripts/build-x264.sh`, never from a distribution. The patches make x264 the screen encoder it is used as: the
caller's per-macroblock quantizer offsets apply without adaptive quantization's variance pass (`rc.b_qp_offsets`),
the quantizer bounds move live and an offset codes a block below the floor (`H264EncoderWrapper::reconfigure_floor`,
which floors a moving screen at the paint-over quantizer and lifts the floor for the cleanup), and the macroblocks the caller's `mb_info` marks unchanged (`H264EncoderWrapper::constant_blocks`, from the block
ledger, given only where the source reports every change) are coded as skips without analysis, copied from the
input, filtered into the lowres picture again, or costed again by the lookahead, each where the full path would
code, copy, filter, or cost them the same: a stream comes out bit for bit as without them; and the lowres planes are
filtered on the idle threads of the synchronous lookahead, their border expanded again only where an edge block
changed. At 2560x1600 a 256x256 change took x264 2.0 ms of a frame where it took 4.1, and a frame that changed whole
the same as before.
An 8-bit 4:2:0 picture reaches x264 as NV12 laid out as its own frames (`Nv12Planes`, from
`x264_param_input_layout`), which it encodes where it lies instead of copying it (`X264_CSP_IN_PLACE`) and reads through the
next encode as the previous input, so the CPU path converts into two pictures in turn and the X11 capture keeps that
pool surface out of its pool until an encode no longer reads it (`x264_encoder_release_input` frees it for a resize):
a 2560x1600 scroll took 6.7 ms of a frame where it took 7.4.
libvpx is built the same way, from the release the recipe pins with the patches under `patches/libvpx`
(`scripts/build-libvpx.sh`; `infra/packaging/patched-codecs.sh` builds both for Selkies), never from a
distribution. The blocks the ledger marks still and clean reach every libvpx session as its active map
(`FrameEncoder::constant_blocks`, `VpxEncoder::constant_blocks`), on a frame of the rate control's that predicts
from the frame before: libvpx itself codes an inactive macroblock as a zero-motion skip without a search (VP8) or a
superblock of them as one skip (VP9), and the patches copy only the active blocks, and those changed since the
buffer last held the picture, into the lookahead buffer the frame is coded from, bit for bit the stream of a whole
copy. A still 1080p screen took VP8 0.9 ms a frame on eight threads where it took 1.6 without the map and VP9 0.6
where it took 1.9; a block the cleanup has not reached stays active, so the rate control refines it as before.
The crate's `Cargo.toml` is the one place the version lives: `setup.py` reads it, spelling a semver pre-release
the PEP 440 way (`2.1.0-rc.1` is `2.1.0rc1` to pip), and the release workflow stamps the tag into the manifest
and the lock, so a build ahead of a release carries the series version and a release the tag's.

A still screen is cleaned up by one policy (`pipeline::cleanup_due`, run per frame by
`decide_hw_fullframe` and per region by `encode_cpu`) that reads what changed from the content —
compositor damage, the X server's report, a content hash (NvFBC's on the GPU) — never from what was sent,
so Turbo (`video_streaming_mode`) sees the screen go still like any other mode: a refresh at the
paint-over quantizer once the region holds still for the trigger, a key frame at it once a large
change has held still for four times that where the block ledger below has not refined most of
the region, and the same cleanup for a region that keeps changing only a little. Within a region that keeps changing, a block ledger (`ledger.rs`) refines the blocks that
held still for the trigger and are not clean through the encoder's per-block quantizer, as many
steps finer than the paint-over quantizer as the encoder needs to come to what its own cleanup
reaches (six for x264 and x265, three for NVENC and VA-API, none for VP8 and kvazaar), a turn of them a frame sized to what the rate control has
room for, a block counting clean once its frame came out at that quantizer: a video playing in
one window no longer holds the text around it at motion quality. x264's sessions take it as
macroblock quantizer offsets applied as given (`rc.b_qp_offsets`, below), NVENC's H.264, HEVC, and AV1 through the QP delta map under the rate control, and libvpx's
VP8 through its region-of-interest map, x265 through its per-block quantizer offsets, kvazaar
through its map of a delta a tree block, and VA-API's H.264 through the driver's regions of
interest, a region a run of blocks along a row and at most ten steps finer than the frame, on X11 (XShm, the shared framebuffer, DRI3) and Wayland; a
constant rate whose frames already run over it refines nothing. NvFBC, whose driver says only
that a frame is new, VP9, which skips a block whose source has not changed whatever its
quantizer, VA-API's HEVC, whose regions Intel's iHD codes into a worse picture, SVT-AV1, whose
mode decision skips a still block whatever its segment's quantizer, and the
other encoders keep the region-wide cleanup alone. Under a constant quality the cleanup moves the session's quality to the
paint-over one until the region moves again (`pipeline::decide_constant_quality`, and x264's
rate factor in `encode_cpu`), as main's paint-over did, and no key frame follows on either path:
the frames held there refine the picture past what a key frame at that quality codes. Under a constant rate,
a session that holds a quantizer and whose rate control reports the quantizer and the bytes of
its frames (NVENC, and libx264) is cleaned up through that rate
control: the frames keep flowing, each within its budget, until it codes the region at the
paint-over quantizer or finer in a small frame (`pipeline::convergence`), and no key frame is
sent; only where NVENC's rate control stalls short of it (a low rate for the resolution) is the
picture refreshed at the paint-over quantizer, a band a frame through the session's QP delta map
where it takes one (H.264 and HEVC; `FrameEncoder::band_size`), each band sized to a frame's budget
and the rest of the frame held at the coarsest quantizer, which leaves a still region as it is, and
a step coarser a band while the rest would run the sweep past `REFINE_S`, else in one held frame.
A whole refresh held at it in one frame measured 470-730 kB at 1080p
and 8 Mbit/s, half a second of queue on a 12 Mbit/s link, where the rate control reaches the same
picture in about a second with every frame within its budget. A rate control pinned at its coarsest
quantizer, its frames of the still screen over two budgets for a trigger period and none finer
than the finest before (NVENC's at 0.1 Mbit/s at 1080p), refines nothing and would put that cost
on every band, so the screen is refreshed at once in one held frame coarsened to
`HELD_KEY_BUDGET_S` at those frames' size.
Elsewhere under a constant rate the frame is held at that quantizer through
`FrameEncoder::hold_quantizer`:
libvpx's VP8 pins its bounds, and SVT-AV1 raises the target for a key frame, bounded to
`HELD_KEY_BUDGET_S` of the target. There is no cleanup where the encoder's last quantizer
(`last_quality`) is already finer, since that rate control refines a still screen itself. Where a
constant-rate session cannot hold a quantizer (`holds_quantizer`: VA-API, x265, kvazaar, and
libvpx's VP9, whose held refresh was one frame of 80 budgets at 2 Mbit/s) but reports its frames'
bytes, its frames flow through the rate control until the picture is done and no key frame is
sent: VA-API until the reconstruction it reads back and measures against its source once a second
stops improving (`FrameEncoder::measure`, `pipeline::plateau`; iHD's reported H.264 quantizer
reads 26 through a refinement from 23 to 50 dB, and it keeps 10-bit HEVC and VP9 and packed 4:4:4
reconstructions in a layout that cannot be read, which fall back to the quantizer), the others after
a trigger period of frames at the paint-over quantizer or once their frames have kept the size of
the one before for `EMPTY_S` at a quantizer as coarse as the floor's, and any of them after
`REFINE_S`. That cleanup then counts as settled, and only more than a small change arms another, so
a caret on a clean screen costs its own frames. A session that reports no bytes (Tegra, a stateful
V4L2 device) gets a refresh and its burst at the rate control's own quality. A constant-rate x264
session is floored at the paint-over quantizer while it moves, where a generous rate would otherwise be spent
coding motion finer than the still picture it leaves (a 1440p scroll at 30 Mbit/s, 22.3 Mbit/s against 13.5),
the floor lifted for its cleanup, and is given a buffer of `CLEANUP_VBV_FRAMES` frames while it is cleaned up, since in a frame and a half
its row-level control coarsens the rows it names a fine quantizer for, and its cleanup ends once its
frames have been slice headers alone for `EMPTY_S`: x264 keeps its quantizer on a screen that does
not change. On Intel's iHD the VA-API rate control itself is bounded per codec
(`vaapi::ConstantRate`): under the session's
buffer of a frame and a half it skips every block and pads the frame to the target.

Host capture of an external Wayland compositor (`wayland/host.rs`, `wayland_host_display`) picks each
rung per capability from the host's registry, never by a setting: frames through
`ext-image-copy-capture` or `wlr-screencopy` into pixelflux-allocated dmabufs, else through an
xdg-desktop-portal RemoteDesktop/ScreenCast session whose PipeWire streams are imported where they lie
(`wayland/portal.rs` over the pure-Rust zbus client, `wayland/pwcapture.rs` over the run-time
libpipewire binding shared with the webcam sink in `pipewire.rs`); keyboard and pointer over libei
(`wayland/eiclient.rs`, the pure-Rust `reis` client) where the backend answers `ConnectToEIS`, since
it carries keyboard, pointer, and touch on one socket, else the virtual-keyboard and virtual-pointer
protocols where the host offers them, else kernel uinput devices where `/dev/uinput` is writable, and
the portal's own `Notify*` methods by keysym last. That order is measured rather than assumed: a
socket write to the compositor runs about 4 us, a uinput write reaching its evdev node about 12 us,
and a `Notify*` D-Bus call about 2 ms. A successful `ConnectToEIS` makes the session refuse `Notify*`, so
libei is committed to only once its handshake binds a device, and the compositor's own keymap
(delivered over EIS, read-only) resolves each key's base keysym with a raw-keycode fallback. A portal
that refuses those devices is asked again for capture alone. The Plasma 6.6 session the sandbox can run (`kwin_wayland --virtual` with
`xdg-desktop-portal-kde` on a private bus, as selkies' `test_wayland_host_portal` brings it up) is the real
non-wlroots target for the portal rung: KWin runs there with `KWIN_WAYLAND_NO_PERMISSION_CHECKS`, as the
KDE image sets it, the backend starts a remote-desktop session without asking only for an app the
permission store's `kde-authorized` table grants `remote-desktop`, and the stream's frames arrive as GPU
dmabufs, so the portal stream's dmabuf import is exercised there too.

X11 capture has three backends and picks between them itself (`x11::run_capture`): NvFBC
(`x11/nvfbc.rs`) where the NVIDIA driver composites the screen into video memory and the buffer is
registered with NVENC in place, which is zero-copy and the lower-latency path; DRI3
(`x11/dri3.rs`) on any server whose screen lives on the GPU (XLibre's Xvfb with glamor, an Xorg
on a DRM driver), where the server blits the root into dmabufs pixelflux allocated through GBM on
the server's own render node and wrapped as pixmaps, the blit is waited for with a one-pixel
`GetImage`, the XFixes cursor is composited by the server through Render, and the hardware session
imports each dmabuf in place through the same `encode_dmabuf` the Wayland zero-copy path uses; and
the general path otherwise, which reads the screen in place where the server shares its framebuffer
(`x11/framebuffer.rs`: the images' Xvfb holding the screen in system memory offers SELKIES-FRAMEBUFFER, a
read-only memfd of the screen, a watch of the tiles drawn into it, and an eventfd, so a frame copies only
the tiles its pool surface misses, or for an 8-bit 4:2:0 x264 session with no overlay drawn converts them
straight into the surface's NV12 planes, which x264 encodes in place (`FramebufferGrab::convert`), waits on no request, and is decided on those tiles and the overlays drawn onto it
with no content hash, `X11Pipeline::process_reported`) and through XShm and XDamage on every other
server. Each zero-copy backend is declined -- with one line saying why --
for a codec its engine does not serve, software encoding, a server or device that does not qualify
(DRI3 also asks that the server draw on the encode node's GPU and that the encoder read the first frame),
and NvFBC for a non-NVIDIA encode node, a watermark, a driver without it, a 10-bit request NVENC lacks
and the software encoder codes, or a libX11 bound to another
copy of libxcb than the host's (a wheel that bundles libxcb and loads libX11 first), whose connection the
driver would read at another structure layout; DRI3 composites a
watermark through Render as it does the cursor, so that one costs it no readback. There is no
setting either way: the server's and the driver's own answers decide, and a host without NvFBC
pays about 7 ms once per capture start to find that out.
All three publish a change as it lands rather than on the next tick: NvFBC because the driver
generates a frame on damage and the grab waits for it, DRI3 and XShm because the X server's
DAMAGE reports end the wait early, and a shared framebuffer because its watch's eventfd does. How far any of them may come early is the frame pacing the
Wayland backend also keeps (`pace.rs`), so the rate stays within a thirty-second of the configured one. NvFBC's driver says
only that a frame is new; how much of it changed comes from a hash of its bands on the GPU
(`band_hash`, in the convert's module, read back as one word a band), the XShm path's band damage
computed where the frame lies, so a caret reads as the small change it is and directly rendered GL,
which the X server's DAMAGE may never report, as motion. Under Turbo the cleanup reads the frame
before's, hashed after its delivery, as the XShm path's reads the hash run beside the encode: on the
device where NVENC uploads the frame (`hash_next_upload`, the same kernel), else on a thread beside
it (`hash_beside`). The NvFBC
structures are hand-written FFI checked against the SDK by the layout and version assertions
in that module, and `libnvidia-fbc.so.1` is
loaded at run time like NVENC's library. The hardware checks are `#[ignore]`d
(`cargo test gpu_nvfbc -- --ignored --nocapture --test-threads=1` with `DISPLAY` on an NVIDIA X
server; `cargo test gpu_dri3 -- --ignored --nocapture --test-threads=1` with `DISPLAY` on a
server whose screen lives on the GPU and a hardware encoder on the same device). They capture and
paint the root of whatever `DISPLAY` names, so on a host where that is a live desktop a `gpu_`
sweep runs with `--skip nvfbc --skip dri3` and those checks run only against an X server of their
own; `x11::gpu_test_support` holds the painting and decoding helpers they share.

The virtual camera (`src/webcam/`, Python class `VirtualCamera`) is the webcam counterpart of pcmflux's
`AudioPlayback`: selkies only gates and hands encoded frames over; decoding (OpenH264, libvpx, dav1d, libde265,
TurboJPEG), fitting into the
fixed device format, and publishing happen on the camera's own thread. `push` takes the frame's upright transform as
optional arguments (`rotation` in clockwise degrees, then a horizontal `flip`); `convert::orient_i420` bakes it
right after decode, ahead of the fit, and an upright MJPEG frame keeps its pass-through. Sinks are the shared-memory ring served to the
Selkies V4L2 interposer (`ring.rs`/`server.rs`; the layout is mirrored byte-for-byte by
`addons/v4l2-interposer/v4l2_interposer.c` and checked by selkies' `tests/unit/test_webcam_abi.py`), a
v4l2loopback output device (`v4l2out.rs`), and a PipeWire `Video/Source` node (`pipewire.rs`, `libpipewire-0.3`
loaded at run time, pods built by hand — never add a build-time PipeWire dependency). `cargo test --lib webcam`
covers the ring, decoders (including an OpenH264 encode-and-decode round trip), and pod layouts; the device-level and browser
checks live in Selkies (`tests/integration/test_webcam_device.py`, `tests/e2e/test_webcam.py` from the repository root).

Licensing is part of the build matrix: `LICENSES.md` inventories every crate and native library of the default
(`gpl`, libx264) and `PIXELFLUX_ENABLE_GPL=0` (`openh264`) builds, `scripts/check-licenses.py` and
`deny.toml` gate both (the root license workflow runs the workspace equivalents), and a new crate that links, loads, or vendors
native code has to be described in the script's `NATIVE` table and in `LICENSES.md` before the check passes.
Copyleft stays confined to the `gpl` feature.

Logging follows one rule on every backend, because the combined selkies log is what a remote user pastes
back: a plain `println!` tagged `[X11]`, `[Wayland]`, `[HostCapture]`, or `[pixelflux]` for what an operator
reads (the encoder chosen and the GPU it runs on, the zero-copy or readback decision and why, each fallback,
the `Stream settings active` line that names the mode), `eprintln!` for warnings and errors, and
`crate::log::debug!` (`src/log.rs`) for everything behind those lines — device enumeration, each dmabuf
import, the per-second rate counters, an in-place reconfigure — which prints only while the switch a
capture's `debug_logging` sets is on (or `PIXELFLUX_DEBUG=1`). A line says what was decided, not which
function ran, names the output and the size it acts on, and appears once per decision, never per frame or
per buffer; the `[X11]` and `[Wayland]` lines of the same decision read the same apart from the tag, and the
one `Stream settings active` builder (`log_stream_settings`) serves every path. The strings selkies' suites
wait on (`Stream settings active`, `Socket listening on:`, `Configuring Output`, `[Wayland] Output`) are
contracts; a change to one changes the test with it.

What those lines tell an operator, a caller reads as values (`src/report.rs`, `ScreenCapture.stream_info`, and
`stream_stats`), which is what selkies shows a user who will never see the log. Each decision is recorded where
it is made and logged, into the report bound to the deciding thread (`report::enter`), so a new capture path, a
new fallback, or a new reason a path is declined records itself beside its log line, and the counters stay one
tally per delivered frame, never a callback into Python.

Update this file when certain details change.
