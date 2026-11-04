/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a still screen gets after motion stops, through the X11 host-frame pipeline
//! (`X11Pipeline::process`, the policy and the encoder a capture runs): each frame's kind and
//! size and the luma PSNR of the decoded picture against the source, for a scene that scrolls
//! text and then holds still, optionally with a caret blinking in one place. Measurements to
//! quote rather than assert; configured through `PF_BENCH_*`:
//!
//! `CODEC` (h264, h265, vp8, vp9, av1, jpeg), `CPU` (1 forces software), `FULLFRAME` (1),
//! `FULLCOLOR` (1 for 4:4:4), `DEPTH` (bits per sample),
//! `TURBO` (1), `CBR` (1), `KBPS`, `CRF`, `PAINT_CRF`, `PAINT` (use_paint_over_quality, 1),
//! `KEYFRAME_S` (the scheduled key-frame interval), `W`, `H`, `FPS`, `MOTION` and `STILL` (frames), `STEP` (rows the scroll moves a frame, 4), `CARET`
//! (frames per caret toggle, 0 none), `VIDEO` (`WxH`, a box at the bottom right repainted with a
//! moving picture every frame for the whole run, the PSNR then measured outside it),
//! `RESUME` (frames of motion after the still phase), `TRIGGER` (paint-over trigger frames),
//! `PRESTILL` (frames held still before the measured phase, which then opens with the screen
//! moving `JUMP` rows at once: a window opening on a clean still screen), `IDR_AT` and `IDR_EVERY`
//! (a key frame asked for that many frames into the still phase, and every so many after, as a
//! joining or recovering client does), `TARGET_DB` (the PSNR
//! whose time to reach it is reported), `SSIM` (1 measures SSIM on every frame, not only from the
//! stop), `ROWS` (a file for every frame's figures), `STREAM` (a file for the coded stream), and
//! `SOURCE` (a file for the last frame's BGRA rows). Each run reports its largest frame and
//! the worst wait a frame meets behind the ones before it on a 12, 20, 50, and 100 Mbit/s link.
//!
//! `cargo test --release --lib cleanup_bench::cleanup_bench -- --exact --ignored --nocapture
//! --test-threads=1`, and `cleanup_bench::cleanup_hold_experiment` the same way.

use crate::CaptureSettings;
use crate::encoders::codec::{
    Codec, FRAME_KEY, JPEG_HEADER_LEN, VIDEO_HEADER_LEN, parse_video_type,
};
use crate::pipeline::X11Pipeline;
use crate::webcam::decode::{Decoder, JpegDecoder, VideoDecoder};
use std::collections::HashMap;
use std::time::Instant;

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(format!("PF_BENCH_{name}"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A document taller than the screen: dense text in a few colors on white, a title bar, and a
/// gradient side panel, so a scroll moves detail an encoder has to spend on.
struct Canvas {
    w: usize,
    h: usize,
    bgra: Vec<u8>,
}

impl Canvas {
    /// The canvas the bench scrolls: the PNG `PF_BENCH_IMAGE` names, else the generated one.
    fn for_bench(w: usize, h: usize) -> Self {
        match std::env::var("PF_BENCH_IMAGE") {
            Ok(path) => {
                let img = image::open(&path)
                    .unwrap_or_else(|e| panic!("{path}: {e}"))
                    .to_rgba8();
                assert!(
                    img.width() as usize >= w && img.height() as usize >= h * 2,
                    "{path} is smaller than the scroll"
                );
                let (iw, ih) = (img.width() as usize, img.height() as usize);
                let mut bgra = vec![0u8; w * ih * 4];
                for y in 0..ih {
                    for x in 0..w {
                        let p = img.get_pixel(x as u32, y as u32).0;
                        let i = (y * w + x) * 4;
                        bgra[i] = p[2];
                        bgra[i + 1] = p[1];
                        bgra[i + 2] = p[0];
                        bgra[i + 3] = 255;
                    }
                }
                let _ = iw;
                Self { w, h: ih, bgra }
            }
            Err(_) => Self::new(w, h * 4),
        }
    }

    fn new(w: usize, h: usize) -> Self {
        let mut bgra = vec![255u8; w * h * 4];
        let colors: [[u8; 3]; 4] = [[20, 20, 20], [160, 40, 30], [30, 110, 40], [40, 40, 170]];
        for y in 0..h {
            let (cell_y, gy) = (y / 16, y % 16);
            for x in 0..w {
                let i = (y * w + x) * 4;
                if x < w / 8 {
                    let g = (y * 255 / h) as u8;
                    bgra[i] = 200 - g / 3;
                    bgra[i + 1] = 180;
                    bgra[i + 2] = 120 + g / 3;
                    continue;
                }
                let (cell_x, gx) = (x / 9, x % 9);
                if gy >= 12 || gx >= 7 {
                    continue;
                }
                let mut s = (cell_x as u32)
                    .wrapping_mul(2654435761)
                    .wrapping_add((cell_y as u32).wrapping_mul(40503))
                    .wrapping_add(1);
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                if (cell_x + cell_y * 7) % 23 == 0 {
                    continue;
                }
                if (s >> ((gy * 3 + gx) % 29)) & 1 == 1 {
                    let c = colors[((cell_y / 3) + cell_x / 40) % 4];
                    bgra[i] = c[2];
                    bgra[i + 1] = c[1];
                    bgra[i + 2] = c[0];
                }
            }
        }
        Self { w, h, bgra }
    }

    /// Draw into `f` the screen at scroll offset `scroll` rows, with a title bar and, when
    /// `caret`, a caret. The callers reuse one buffer, as a capture does: an NVENC session
    /// page-locks a host source once, by its address.
    fn frame(&self, f: &mut [u8], sh: usize, scroll: usize, caret: bool) {
        let row = self.w * 4;
        for y in 0..sh {
            let src = (y + scroll) % self.h;
            f[y * row..(y + 1) * row].copy_from_slice(&self.bgra[src * row..(src + 1) * row]);
        }
        for y in 0..28.min(sh) {
            for x in 0..self.w {
                let i = y * row + x * 4;
                f[i] = 90;
                f[i + 1] = 60;
                f[i + 2] = 40;
            }
        }
        if caret {
            let (cx, cy) = (self.w / 2 + 3, sh / 2);
            for y in cy..(cy + 18).min(sh) {
                for x in cx..cx + 2 {
                    let i = y * row + x * 4;
                    f[i] = 0;
                    f[i + 1] = 0;
                    f[i + 2] = 0;
                }
            }
        }
    }
}

/// Source luma the way the stream's decoder reports it: BT.709 at limited range for the video
/// codecs (BT.601 for VP8), BT.601 at full range for JPEG.
fn source_luma(bgra: &[u8], codec: Codec) -> Vec<u8> {
    let (kr, kb, limited) = match codec {
        Codec::Jpeg => (0.299, 0.114, false),
        Codec::Vp8 => (0.299, 0.114, true),
        _ => (0.2126, 0.0722, true),
    };
    let kg = 1.0 - kr - kb;
    bgra.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let y = kr * p[2] as f64 + kg * p[1] as f64 + kb * p[0] as f64;
            let v = if limited { 16.0 + y * 219.0 / 255.0 } else { y };
            v.round().clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// Mean luma SSIM over 8x8 blocks (the constants of the standard index at 8 bits).
fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut sum, mut n) = (0.0, 0usize);
    for by in (0..h.saturating_sub(7)).step_by(8) {
        for bx in (0..w.saturating_sub(7)).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let (p, q) = (a[y * w + x] as f64, b[y * w + x] as f64);
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let (ma, mb) = (sa / 64.0, sb / 64.0);
            let (va, vb, cov) = (
                saa / 64.0 - ma * ma,
                sbb / 64.0 - mb * mb,
                sab / 64.0 - ma * mb,
            );
            sum += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

/// The worst wait, in ms, a frame of `frames` = (send time s, bytes) meets behind the ones before it
/// on a first-in first-out link of `mbit` Mbit/s, over the frames sent from `from` s on.
fn link_wait(frames: &[(f64, usize)], mbit: f64, from: f64) -> f64 {
    let bps = mbit * 1e6 / 8.0;
    let (mut free, mut worst) = (f64::MIN, 0f64);
    for &(t, bytes) in frames {
        let start = free.max(t);
        if t >= from {
            worst = worst.max(start - t);
        }
        free = start + bytes as f64 / bps;
    }
    worst * 1e3
}

/// A moving picture in `area` (`x`, `y`, `width`, `height`) of the `w`-wide BGRA `frame` at
/// frame `t`: bands that drift a few pixels a frame under a grain that changes every frame, so
/// a predicted frame has motion to code and texture it cannot predict.
fn paint_video(
    frame: &mut [u8],
    w: usize,
    (x0, y0, vw, vh): (usize, usize, usize, usize),
    t: usize,
) {
    let mut seed = (t as u32).wrapping_mul(0x9e37_79b9) | 1;
    for y in y0..y0 + vh {
        for x in x0..x0 + vw {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let grain = (seed & 15) as usize;
            let band = ((x - x0) + (y - y0) / 2 + t * 3) % 256;
            let o = (y * w + x) * 4;
            frame[o] = (band + grain) as u8;
            frame[o + 1] = ((band * 3 / 4) + (y - y0) / 4 + grain) as u8;
            frame[o + 2] = (255 - band / 2 + grain) as u8;
            frame[o + 3] = 255;
        }
    }
}

/// `psnr` of the `w`-wide luma planes `a` and `b` inside `area`.
fn psnr_inside(
    a: &[u8],
    b: &[u8],
    w: usize,
    (x0, y0, vw, vh): (usize, usize, usize, usize),
) -> f64 {
    let mut sum = 0f64;
    for y in y0..y0 + vh {
        for x in x0..x0 + vw {
            let d = a[y * w + x] as f64 - b[y * w + x] as f64;
            sum += d * d;
        }
    }
    let mse = sum / (vw * vh).max(1) as f64;
    if mse <= 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// `psnr` of the `w`-wide luma planes `a` and `b` outside `area`.
fn psnr_outside(
    a: &[u8],
    b: &[u8],
    w: usize,
    (x0, y0, vw, vh): (usize, usize, usize, usize),
) -> f64 {
    let (mut sum, mut n) = (0f64, 0usize);
    for (i, (&x, &y)) in a.iter().zip(b).enumerate() {
        let (px, py) = (i % w, i / w);
        if (x0..x0 + vw).contains(&px) && (y0..y0 + vh).contains(&py) {
            continue;
        }
        let d = x as f64 - y as f64;
        sum += d * d;
        n += 1;
    }
    let mse = sum / n.max(1) as f64;
    if mse <= 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// The 16x16 blocks of the `w` x `h` BGRA `frame` that differ from `previous`, as rectangles a
/// run of them along a row: what a source that reports exactly where it drew says.
fn changed_blocks(
    frame: &[u8],
    previous: &[u8],
    w: usize,
    h: usize,
) -> Vec<smithay::utils::Rectangle<i32, smithay::utils::Physical>> {
    let mut rects = Vec::new();
    for by in (0..h).step_by(16) {
        let rows = by..(by + 16).min(h);
        let mut run: Option<usize> = None;
        for bx in (0..w).step_by(16).chain(std::iter::once(w)) {
            let changed = bx < w
                && rows.clone().any(|y| {
                    let (a, b) = (y * w * 4 + bx * 4, y * w * 4 + (bx + 16).min(w) * 4);
                    frame[a..b] != previous[a..b]
                });
            match (changed, run) {
                (true, None) => run = Some(bx),
                (false, Some(x0)) => {
                    rects.push(smithay::utils::Rectangle::new(
                        (x0 as i32, by as i32).into(),
                        ((bx - x0) as i32, rows.len() as i32).into(),
                    ));
                    run = None;
                }
                _ => {}
            }
        }
    }
    rects
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse <= 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

#[test]
#[ignore]
fn cleanup_bench() {
    let codec = Codec::parse(&env("CODEC", "h264".to_string())).expect("codec");
    let (w, h): (usize, usize) = (env("W", 1920), env("H", 1080));
    let fps: f64 = env("FPS", 60.0);
    let settings = CaptureSettings {
        width: w as i32,
        height: h as i32,
        target_fps: fps,
        codec,
        use_cpu: env("CPU", 0) == 1,
        video_fullframe: env("FULLFRAME", 1) == 1,
        video_streaming_mode: env("TURBO", 1) == 1,
        video_cbr_mode: env("CBR", 1) == 1,
        video_bitrate_kbps: env("KBPS", 8000),
        video_crf: env("CRF", 25),
        video_paintover_crf: env("PAINT_CRF", 18),
        video_paintover_burst_frames: env("BURST", 5),
        use_paint_over_quality: env("PAINT", 1) == 1,
        paint_over_trigger_frames: env("TRIGGER", 15),
        damage_block_threshold: 10,
        damage_block_duration: 20,
        jpeg_quality: env("JPEG_Q", 40),
        paint_over_jpeg_quality: env("PAINT_JPEG_Q", 90),
        video_vbv_multiplier: env("VBV", 0.0),
        video_fullcolor: env("FULLCOLOR", 0) == 1,
        video_bit_depth: env("DEPTH", 8),
        keyframe_interval_s: env("KEYFRAME_S", 0.0),
        ..Default::default()
    };
    let motion: usize = env("MOTION", 90);
    let step: usize = env("STEP", 4);
    let prestill: usize = env("PRESTILL", 0);
    let jump: usize = env("JUMP", 0);
    let still: usize = env("STILL", 240);
    let resume: usize = env("RESUME", 0);
    let caret_period: usize = env("CARET", 0);
    let target_db: f64 = env("TARGET_DB", 0.0);
    let ssim_all = env("SSIM", 0) == 1;
    let idr_at: i64 = env("IDR_AT", 0);
    let idr_every: i64 = env("IDR_EVERY", 0);
    let video = std::env::var("PF_BENCH_VIDEO").ok().and_then(|v| {
        let (vw, vh) = v.split_once('x')?;
        let (vw, vh): (usize, usize) = (vw.parse().ok()?, vh.parse().ok()?);
        Some((
            w.saturating_sub(vw + 32),
            h.saturating_sub(vh + 32),
            vw.min(w),
            vh.min(h),
        ))
    });
    let canvas = Canvas::for_bench(w, h);
    let mut p = X11Pipeline::new(settings.clone());
    let codec = p.codec();
    println!(
        "bench {} {} {}x{} turbo={} cbr={} kbps={} crf={} paint={}/{} caret={} prestill={prestill} jump={jump} encoder={}",
        codec.display(),
        if settings.video_fullframe {
            "full"
        } else {
            "striped"
        },
        w,
        h,
        settings.video_streaming_mode,
        settings.video_cbr_mode,
        settings.video_bitrate_kbps,
        settings.video_crf,
        settings.use_paint_over_quality,
        settings.video_paintover_crf,
        caret_period,
        p.encoder_name()
    );
    let budget = settings.video_bitrate_kbps as f64 * 1000.0 / 8.0 / fps;
    let mut decoders: HashMap<i32, Box<dyn Decoder>> = HashMap::new();
    let mut shown = vec![0u8; w * h];
    let stop = motion + prestill;
    let total = stop + still + resume;
    let mut still_bytes = 0usize;
    let mut resume_bytes = 0usize;
    let mut still_keys = 0usize;
    let mut still_max = (0usize, 0i64);
    let mut first_key: Option<(usize, usize)> = None;
    let mut last_psnr = 0.0;
    let mut last_ssim = 0.0;
    let mut psnr_at_stop = 0.0;
    let mut records: Vec<(i64, usize, String, f64, f64, f64)> = Vec::new();
    let mut sent: Vec<(f64, usize)> = Vec::new();
    let rows_path = std::env::var("PF_BENCH_ROWS").ok();
    let mut stream = std::env::var("PF_BENCH_STREAM")
        .ok()
        .map(|path| std::fs::File::create(&path).unwrap_or_else(|e| panic!("{path}: {e}")));
    let mut frame = vec![0u8; w * 4 * h];
    let mut previous = vec![0u8; w * 4 * h];
    let mut video_psnr: Vec<f64> = Vec::new();
    for t in 0..total {
        let scroll = if t < motion {
            t * step
        } else if t < stop {
            motion * step
        } else if t < stop + still {
            motion * step + jump
        } else {
            motion * step + jump + (t + 1 - stop - still) * step
        };
        let caret = caret_period > 0 && t >= stop && ((t - stop) / caret_period).is_multiple_of(2);
        canvas.frame(&mut frame, h, scroll, caret);
        if let Some(area) = video {
            paint_video(&mut frame, w, area, t);
        }
        let since = t as i64 - stop as i64 - idr_at;
        if idr_at > 0 && since >= 0 && (since == 0 || (idr_every > 0 && since % idr_every == 0)) {
            p.request_idr();
        }
        let changed = changed_blocks(&frame, &previous, w, h);
        previous.copy_from_slice(&frame);
        let start = Instant::now();
        let out = p.process_damaged(&frame, w * 4, &changed);
        let ms = start.elapsed().as_secs_f64() * 1e3;
        let mut bytes = 0usize;
        let mut kinds = String::new();
        for s in &out {
            bytes += s.data.len();
            let (payload, kind) = if s.data[0] == 0x03 {
                (&s.data[JPEG_HEADER_LEN..], 'J')
            } else {
                let (_, k) = parse_video_type(s.data[1]).expect("video type");
                (
                    &s.data[VIDEO_HEADER_LEN..],
                    if k == FRAME_KEY {
                        'K'
                    } else if k == 0x02 {
                        'I'
                    } else {
                        'P'
                    },
                )
            };
            kinds.push(kind);
            if let Some(file) = stream.as_mut() {
                std::io::Write::write_all(file, payload).expect("stream");
            }
            let dec = decoders
                .entry(s.stripe_y_start)
                .or_insert_with(|| match codec {
                    Codec::Jpeg => {
                        Box::new(JpegDecoder::new().expect("jpeg decoder")) as Box<dyn Decoder>
                    }
                    c => Box::new(VideoDecoder::new(c).expect("decoder")),
                });
            match dec.decode(payload) {
                Ok(true) => {
                    let pic = dec.frame().expect("picture");
                    let y0 = s.stripe_y_start as usize;
                    for r in 0..(s.stripe_height as usize).min(pic.height).min(h - y0) {
                        shown[(y0 + r) * w..(y0 + r) * w + w.min(pic.width)].copy_from_slice(
                            &pic.y[r * pic.y_stride..r * pic.y_stride + w.min(pic.width)],
                        );
                    }
                }
                Ok(false) => {}
                Err(e) => println!("  decode error at t={t}: {e:?}"),
            }
        }
        if bytes > 0 {
            sent.push((t as f64 / fps, bytes));
        }
        let rel = t as i64 - stop as i64;
        let in_still = (stop..stop + still).contains(&t);
        if in_still {
            still_bytes += bytes;
            if kinds.contains('K') {
                still_keys += 1;
                if first_key.is_none() {
                    first_key = Some((t - stop, bytes));
                }
            }
            if bytes > still_max.0 {
                still_max = (bytes, rel);
            }
        }
        if t >= stop + still {
            resume_bytes += bytes;
        }
        let measure = !out.is_empty() || rel % 30 == 0 || t + 1 == total || rel == 0;
        if measure {
            let src = source_luma(&frame, codec);
            last_psnr = match video {
                Some(area) => psnr_outside(&shown, &src, w, area),
                None => psnr(&shown, &src),
            };
            if let Some(area) = video.filter(|_| in_still) {
                video_psnr.push(psnr_inside(&shown, &src, w, area));
            }
            last_ssim = if ssim_all || rel >= -1 {
                ssim(&shown, &src, w, h)
            } else {
                0.0
            };
        }
        if rel == 0 {
            psnr_at_stop = last_psnr;
        }
        records.push((
            rel,
            bytes,
            kinds,
            ms,
            if measure { last_psnr } else { -1.0 },
            last_ssim,
        ));
    }
    if let Ok(path) = std::env::var("PF_BENCH_SOURCE") {
        std::fs::write(&path, &frame).unwrap_or_else(|e| panic!("{path}: {e}"));
    }
    if let Some(path) = rows_path {
        let rows: Vec<String> = records
            .iter()
            .map(|(rel, bytes, kinds, ms, q, ss)| {
                format!("[{rel},{bytes},\"{kinds}\",{ms:.2},{q:.3},{ss:.5}]")
            })
            .collect();
        std::fs::write(&path, format!("[{}]\n", rows.join(","))).expect("rows");
    }
    let resume_at = still as i64;
    for (rel, bytes, kinds, ms, q, ss) in &records {
        let near_stop = (-2..24).contains(rel);
        let keyish = (kinds.contains('K') || kinds.contains('I')) && *rel >= -2;
        let near_resume = (resume_at - 1..resume_at + 8).contains(rel);
        let periodic = *rel >= 0 && rel % 30 == 0;
        let big = *rel >= 0 && *bytes as f64 > 3.0 * budget;
        if near_stop || keyish || near_resume || periodic || big {
            let k = if kinds.len() > 6 {
                format!("{}..{}", &kinds[..3], kinds.len())
            } else {
                kinds.clone()
            };
            let q = if *q >= 0.0 {
                format!("{q:5.2}")
            } else {
                "  -  ".into()
            };
            println!(
                "  t={rel:+5} {:>7.1} kB {k:>6} x{:<5.1} {ms:6.2} ms  psnr {q} ssim {ss:.4}",
                *bytes as f64 / 1000.0,
                *bytes as f64 / budget
            );
        }
    }
    let still_rows: Vec<&(i64, usize, String, f64, f64, f64)> = records
        .iter()
        .filter(|r| (0..resume_at).contains(&r.0))
        .collect();
    let still_sent = still_rows.iter().filter(|r| r.1 > 0).count();
    let end_psnr = still_rows
        .iter()
        .rev()
        .find(|r| r.4 >= 0.0)
        .map_or(last_psnr, |r| r.4);
    let end_ssim = still_rows.last().map_or(last_ssim, |r| r.5);
    let reach = |db: f64| still_rows.iter().find(|r| r.4 >= db).map(|r| r.0);
    let secs =
        |f: Option<i64>| f.map_or("never".to_string(), |f| format!("{:.2}s", f as f64 / fps));
    let target = if target_db > 0.0 {
        target_db
    } else {
        end_psnr - 0.5
    };
    let from = stop as f64 / fps - 0.5;
    let waits: Vec<String> = [12.0, 20.0, 50.0, 100.0]
        .iter()
        .map(|&m| format!("{m:.0}:{:.0}", link_wait(&sent, m, from)))
        .collect();
    println!("  still frames sent: {still_sent} of {still}; psnr at stop {psnr_at_stop:.2}");
    if !video_psnr.is_empty() {
        println!(
            "  video area psnr over the still phase: mean {:.2}, least {:.2}",
            video_psnr.iter().sum::<f64>() / video_psnr.len() as f64,
            video_psnr.iter().cloned().fold(f64::INFINITY, f64::min)
        );
    }
    println!(
        "summary codec={} turbo={} cbr={} kbps={} {}x{} first_key={:?} keys={still_keys} max_kB={:.1}@{} still_kB={:.1} still_kbps={:.0} resume_kB={:.1} end_psnr={:.2} end_ssim={:.5} to_end-1dB={} to_end-0.5dB={} to_target({target:.2})={} link_wait_ms={}",
        codec.name(),
        settings.video_streaming_mode,
        settings.video_cbr_mode,
        settings.video_bitrate_kbps,
        w,
        h,
        first_key,
        still_max.0 as f64 / 1000.0,
        still_max.1,
        still_bytes as f64 / 1000.0,
        still_bytes as f64 * 8.0 / 1000.0 / (still as f64 / fps),
        resume_bytes as f64 / 1000.0,
        end_psnr,
        end_ssim,
        secs(reach(end_psnr - 1.0)),
        secs(reach(end_psnr - 0.5)),
        secs(reach(target)),
        waits.join(",")
    );
}

/// One session of the backend the settings select, driven directly: `x264` for the software
/// H.264 session a full-frame capture runs (one stripe), else the ladder's full-frame encoder.
enum Session {
    #[cfg(feature = "gpl")]
    X264(
        Box<crate::encoders::software::H264EncoderWrapper>,
        crate::encoders::session::Planes,
    ),
    Frame(Box<crate::encoders::FrameEncoder>),
}

impl Session {
    fn open(settings: &CaptureSettings) -> (Self, String) {
        #[cfg(feature = "gpl")]
        if settings.codec == Codec::H264 && settings.use_cpu {
            let bps = settings.video_bitrate_kbps.max(1) as u32 * 1000;
            let vbv = crate::encoders::vbv_bits(
                bps,
                settings.target_fps,
                0.0,
                settings.video_vbv_multiplier,
            ) / 1000;
            let enc = crate::encoders::software::H264EncoderWrapper::new(
                settings.width,
                settings.height,
                settings.video_crf,
                false,
                settings.target_fps,
                4,
                settings.video_cbr_mode,
                settings.video_bitrate_kbps,
                vbv as i32,
                0,
                0,
            )
            .expect("x264");
            let planes = crate::encoders::session::Planes::new(
                settings.width as usize,
                settings.height as usize,
                false,
                8,
            );
            return (Session::X264(Box::new(enc), planes), "x264".into());
        }
        let mut s = settings.clone();
        let enc = crate::encoders::select_frame_encoder(
            &mut s,
            crate::encoders::FrameSource::Host { rgba: false },
            None,
            "bench",
        )
        .expect("a full-frame session");
        let name = format!("{} {}", enc.backend_name(), s.codec.display());
        (Session::Frame(Box::new(enc)), name)
    }

    /// Encode `bgra`, holding the quantizer `held` (a quality index) when asked; the bytes past
    /// the wire header and whether they are a key frame.
    fn encode(
        &mut self,
        bgra: &[u8],
        w: usize,
        n: u64,
        crf: u32,
        key: bool,
        held: Option<u32>,
    ) -> (Vec<u8>, bool) {
        match self {
            #[cfg(feature = "gpl")]
            Session::X264(enc, planes) => {
                planes
                    .convert(bgra, w * 4, false, false, false, 4)
                    .expect("convert");
                if let Some(q) = held {
                    enc.hold_quantizer(q as i32);
                }
                let mut out = Vec::new();
                let cw = planes.chroma_width() as i32;
                if !enc.encode_with_headers(
                    &planes.y, &planes.u, &planes.v, w as i32, cw, cw, n as u16, 0, key, false,
                    &mut out,
                ) {
                    return (Vec::new(), false);
                }
                let k = out[1] & 0x0f == FRAME_KEY;
                (out[VIDEO_HEADER_LEN..].to_vec(), k)
            }
            Session::Frame(enc) => {
                if let Some(q) = held {
                    enc.hold_quantizer(q, None);
                }
                let out = enc
                    .encode_host(bgra, w * 4, false, n, crf, key)
                    .expect("encode");
                if out.is_empty() {
                    return (out, false);
                }
                let k = out[1] & 0x0f == FRAME_KEY;
                (out[VIDEO_HEADER_LEN..].to_vec(), k)
            }
        }
    }
}

/// A held key frame against a held predicted frame against nothing, after a scroll: the
/// cleanup frame's size and encode time, the picture's PSNR before and after it and a second
/// later, and what the first frames of renewed motion cost and look like, for the session the
/// settings select (`PF_BENCH_CODEC`, `CPU`, `CBR`, `KBPS`, `W`, `H`).
#[test]
#[ignore]
fn cleanup_hold_experiment() {
    let codec = Codec::parse(&env("CODEC", "h264".to_string())).expect("codec");
    let (w, h): (usize, usize) = (env("W", 1920), env("H", 1080));
    let fps: f64 = env("FPS", 60.0);
    let settings = CaptureSettings {
        width: w as i32,
        height: h as i32,
        target_fps: fps,
        codec,
        use_cpu: env("CPU", 0) == 1,
        video_fullframe: true,
        video_cbr_mode: env("CBR", 1) == 1,
        video_bitrate_kbps: env("KBPS", 8000),
        video_crf: env("CRF", 25),
        ..Default::default()
    };
    let paint: u32 = env("PAINT_CRF", 18);
    let crf = settings.video_crf as u32;
    let canvas = Canvas::for_bench(w, h);
    let budget = settings.video_bitrate_kbps as f64 * 1000.0 / 8.0 / fps;
    for mode in ["none", "key", "delta"] {
        let (mut session, name) = Session::open(&settings);
        let mut dec = VideoDecoder::new(codec).expect("decoder");
        let mut n = 0u64;
        let mut frame = vec![0u8; w * 4 * h];
        let mut step = |session: &mut Session,
                        dec: &mut VideoDecoder,
                        scroll: usize,
                        key: bool,
                        held: Option<u32>| {
            canvas.frame(&mut frame, h, scroll, false);
            let t = Instant::now();
            let (bytes, k) = session.encode(&frame, w, n, crf, key || n == 0, held);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            n += 1;
            if !bytes.is_empty() {
                dec.decode(&bytes).expect("decode");
            }
            let pic = dec.frame().expect("picture");
            let mut shown = vec![0u8; w * h];
            for r in 0..h {
                shown[r * w..r * w + w]
                    .copy_from_slice(&pic.y[r * pic.y_stride..r * pic.y_stride + w]);
            }
            (
                bytes.len(),
                k,
                ms,
                psnr(&shown, &source_luma(&frame, codec)),
            )
        };
        let motion = 90usize;
        for t in 0..motion {
            step(&mut session, &mut dec, t * 4, false, None);
        }
        let stop = motion * 4;
        let mut still = Vec::new();
        for _ in 0..15 {
            still.push(step(&mut session, &mut dec, stop, false, None));
        }
        let before = still.last().unwrap().3;
        let cleanup = match mode {
            "key" => step(&mut session, &mut dec, stop, true, Some(paint)),
            "delta" => step(&mut session, &mut dec, stop, false, Some(paint)),
            _ => step(&mut session, &mut dec, stop, false, None),
        };
        let mut after = Vec::new();
        for _ in 0..60 {
            after.push(step(&mut session, &mut dec, stop, false, None));
        }
        let after_kb: f64 = after.iter().map(|r| r.0 as f64).sum::<f64>() / 1000.0;
        let mut resume = Vec::new();
        for t in 0..30 {
            resume.push(step(
                &mut session,
                &mut dec,
                stop + (t + 1) * 4,
                false,
                None,
            ));
        }
        let resume_first: Vec<String> = resume
            .iter()
            .take(4)
            .map(|r| format!("{:.1}kB/{:.1}dB", r.0 as f64 / 1000.0, r.3))
            .collect();
        let resume_psnr = resume.iter().map(|r| r.3).sum::<f64>() / resume.len() as f64;
        let resume_kb = resume.iter().map(|r| r.0 as f64).sum::<f64>() / 1000.0;
        println!(
            "hold {name:<16} {w}x{h} cbr={} kbps={} mode={mode:<5} cleanup {:>7.1} kB (x{:<5.1} budget) key={} {:5.2} ms | psnr before {before:5.2} after {:5.2} +1s {:5.2} | still 1s {after_kb:6.1} kB | resume 30f {resume_kb:6.1} kB avg {resume_psnr:5.2} dB first {}",
            settings.video_cbr_mode,
            settings.video_bitrate_kbps,
            cleanup.0 as f64 / 1000.0,
            cleanup.0 as f64 / budget,
            cleanup.1,
            cleanup.2,
            cleanup.3,
            after.last().unwrap().3,
            resume_first.join(" ")
        );
    }
}

/// What still blocks beside motion are coded at, through the X11 host-frame pipeline and x264
/// at a constant quality: a texture with a box in it that changes every frame, the frame never
/// still. The texture around the box is refined to the paint-over quality while the box keeps
/// changing (`ledger`), where without the ledger it stayed at the session's quality. OpenH264,
/// the H.264 encoder of a build without `gpl`, takes no quantizer per macroblock.
#[cfg(feature = "gpl")]
#[test]
fn still_blocks_beside_motion_are_refined() {
    let (w, h) = (256usize, 128usize);
    let settings = CaptureSettings {
        width: w as i32,
        height: h as i32,
        target_fps: 60.0,
        codec: Codec::H264,
        use_cpu: true,
        video_fullframe: true,
        video_streaming_mode: false,
        video_cbr_mode: false,
        video_crf: 35,
        video_paintover_crf: 18,
        use_paint_over_quality: true,
        paint_over_trigger_frames: 15,
        ..Default::default()
    };
    let area = (192, 64, 64, 64);
    let mut p = X11Pipeline::new(settings);
    let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
    let mut shown = vec![0u8; w * h];
    let mut frame = vec![0u8; w * h * 4];
    let mut seed = 0x1234_5678u32;
    for px in frame.chunks_mut(4) {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        px.copy_from_slice(&[seed as u8, (seed >> 8) as u8, (seed >> 16) as u8, 255]);
    }
    let changed = [smithay::utils::Rectangle::new(
        (area.0 as i32, area.1 as i32).into(),
        (area.2 as i32, area.3 as i32).into(),
    )];
    for t in 0..240 {
        paint_video(&mut frame, w, area, t);
        for s in p.process_damaged(&frame, w * 4, if t == 0 { &[] } else { &changed }) {
            if dec.decode(&s.data[VIDEO_HEADER_LEN..]).expect("decode") {
                let pic = dec.frame().expect("picture");
                for r in 0..h {
                    shown[r * w..(r + 1) * w].copy_from_slice(&pic.y[r * pic.y_stride..][..w]);
                }
            }
        }
    }
    let around = psnr_outside(&shown, &source_luma(&frame, Codec::H264), w, area);
    println!("around the box: {around:.2} dB");
    assert!(around > 40.0, "the still texture reached {around:.2} dB");
}
