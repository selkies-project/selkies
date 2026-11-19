/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a software encode costs a frame, timed for CodSpeed: one frame through the X11
//! host-frame pipeline a capture runs (`X11Pipeline::process_reported`, the path of a shared
//! framebuffer, handed the blocks the frame changed), for JPEG stripes, x264 at a UMH motion
//! search range of 16 and of 64 pixels, and libvpx's VP9.
//!
//! The screens are the cleanup measurements' (`pixelflux::fixtures`): a document scrolling 4,
//! 40, and 96 rows a frame under a still title bar, coded at a constant quality (CRF 25, JPEG
//! quality 40), and a moving picture over the whole screen, coded at 8 Mbit/s constant rate,
//! at 1280x720 and 60 fps. Every session codes on one thread (`cpu_encode_threading` off), so
//! a frame costs the same instructions on every run on one CPU (the codecs pick their SIMD code
//! by its features); a capture on a multi-core host spreads its session over threads instead.
//!
//! Each benchmark opens its pipeline and codes `WARM_FRAMES` frames before it is timed: the key
//! frame, the frames its rate control settles over, and the refinement of the still title bar
//! (`paint_over_trigger_frames`), each checked to come out coded by a software encoder. The
//! frames go on from there, drawn outside the timed call, so every timed frame is a new one
//! predicted from the frame before. x264's range is set for the thread before its session opens
//! (`set_x264_me_range`), and every benchmark sets its own.
//!
//! `cargo bench -p pixelflux --bench encode` runs them under Criterion; CI runs them under
//! CodSpeed's instruction counting (`cargo codspeed`), which keys its history on the names
//! below.

use std::hint::black_box;
use std::sync::LazyLock;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use pixelflux::CaptureSettings;
use pixelflux::encoders::Codec;
use pixelflux::encoders::software::set_x264_me_range;
use pixelflux::fixtures::{Canvas, changed_blocks, paint_video};
use pixelflux::pipeline::X11Pipeline;
use smithay::utils::{Physical, Rectangle};

const W: usize = 1280;
const H: usize = 720;

/// Frames a pipeline codes before it is timed: past the paint-over trigger and the cleanup
/// burst after it, so the timed frames are the steady state of a screen in motion.
const WARM_FRAMES: usize = 30;

/// What the screen shows: the document scrolling this many rows a frame, or the moving
/// picture over all of it.
#[derive(Clone, Copy)]
enum Content {
    Scroll(usize),
    Video,
}

impl Content {
    fn name(self) -> String {
        match self {
            Content::Scroll(rows) => format!("scroll_{rows}"),
            Content::Video => "video".into(),
        }
    }

    /// The settings a capture of this content runs: a scroll at a constant quality, the picture
    /// at a constant rate, as a session streaming text and one streaming video are set.
    fn settings(self, codec: Codec) -> CaptureSettings {
        CaptureSettings {
            width: W as i32,
            height: H as i32,
            target_fps: 60.0,
            codec,
            use_cpu: true,
            cpu_encode_threading: false,
            video_streaming_mode: true,
            video_cbr_mode: matches!(self, Content::Video),
            video_bitrate_kbps: 8000,
            video_crf: 25,
            jpeg_quality: 40,
            paint_over_jpeg_quality: 90,
            damage_block_duration: 20,
            ..Default::default()
        }
    }
}

/// A frame and the blocks it changed since the one before.
type Drawn = (Vec<u8>, Vec<Rectangle<i32, Physical>>);

/// The document the scrolls show, drawn once and only where a scroll is timed.
static CANVAS: LazyLock<Canvas> = LazyLock::new(|| Canvas::new(W, H * 4));

/// The frames of one content, one after another.
struct Scene {
    content: Content,
    t: usize,
    frame: Vec<u8>,
    previous: Vec<u8>,
}

impl Scene {
    fn new(content: Content) -> Self {
        Scene {
            content,
            t: 0,
            frame: vec![0; W * H * 4],
            previous: vec![0; W * H * 4],
        }
    }

    fn next(&mut self) -> Drawn {
        match self.content {
            Content::Scroll(rows) => CANVAS.frame(&mut self.frame, H, self.t * rows, false),
            Content::Video => paint_video(&mut self.frame, W, (0, 0, W, H), self.t),
        }
        self.t += 1;
        let changed = changed_blocks(&self.frame, &self.previous, W, H);
        self.previous.copy_from_slice(&self.frame);
        (self.frame.clone(), changed)
    }
}

/// A pipeline coding `content` at `codec`, x264 at `me_range` where it names one, and the
/// scene it codes, past `WARM_FRAMES` frames each checked to come out coded by a software
/// encoder.
fn warmed(
    name: &str,
    codec: Codec,
    content: Content,
    me_range: Option<i32>,
) -> (X11Pipeline, Scene) {
    if let Some(range) = me_range {
        set_x264_me_range(range);
    }
    let mut pipeline = X11Pipeline::new(content.settings(codec));
    assert!(
        !pipeline.is_hardware() && pipeline.encoder_name().starts_with("CPU"),
        "{name}: {} in place of a software encoder",
        pipeline.encoder_name()
    );
    assert_eq!(pipeline.codec(), codec, "{name}: the pipeline fell back");
    let mut scene = Scene::new(content);
    for i in 0..WARM_FRAMES {
        let (frame, changed) = scene.next();
        let out = pipeline.process_reported(&frame, W * 4, &changed);
        assert!(
            !out.is_empty() && out.iter().all(|s| s.codec == codec && !s.data.is_empty()),
            "{name}: frame {i} came out uncoded"
        );
    }
    (pipeline, scene)
}

/// Time one frame's encode of `content` at `codec`. The pipeline is opened and warmed the first
/// time the benchmark runs, so a process that runs another benchmark of the group (CodSpeed runs
/// each in a process of its own) codes none of this one's frames.
fn bench_encode(
    c: &mut Criterion,
    name: &str,
    codec: Codec,
    content: Content,
    me_range: Option<i32>,
) {
    let mut state = None;
    c.bench_function(name, |b| {
        let (pipeline, scene) = state.get_or_insert_with(|| warmed(name, codec, content, me_range));
        b.iter_batched(
            || scene.next(),
            |(frame, changed)| {
                let out = pipeline.process_reported(black_box(&frame), W * 4, &changed);
                (frame, out)
            },
            BatchSize::PerIteration,
        );
    });
}

fn jpeg(c: &mut Criterion) {
    for content in [Content::Scroll(4), Content::Video] {
        let name = format!("jpeg_stripes_720p_{}", content.name());
        bench_encode(c, &name, Codec::Jpeg, content, None);
    }
}

fn x264(c: &mut Criterion) {
    for range in [16, 64] {
        for content in [
            Content::Scroll(4),
            Content::Scroll(40),
            Content::Scroll(96),
            Content::Video,
        ] {
            let name = format!("x264_umh{range}_720p_{}", content.name());
            bench_encode(c, &name, Codec::H264, content, Some(range));
        }
    }
}

fn vp9(c: &mut Criterion) {
    for content in [Content::Scroll(4), Content::Video] {
        let name = format!("vp9_720p_{}", content.name());
        bench_encode(c, &name, Codec::Vp9, content, None);
    }
}

criterion_group!(benches, jpeg, x264, vp9);
criterion_main!(benches);
