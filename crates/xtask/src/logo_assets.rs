/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Every image in the repository, regenerated from the logo artwork.
//!
//! Every image here is the Selkies mark: the icon on its own, the icon on the round plate the
//! app icons use, or one of the two lockups that set it beside or above the wordmark. They
//! are committed rather than built at release time, so a change to the artwork is carried
//! into all of them by this task rather than by hand into the ones somebody remembers.
//!
//! Two sources are hand-authored, `docs/assets/logo/selkies.svg` (the mark) and
//! `wordmark.svg` (the "Selkies" lettering); everything else is composed from them. The two
//! lockups are built from those, so the mark cannot end up embedded twice with two different
//! gradients, and only their layout lives here, as the transform each places its two pieces
//! at ([`LOCKUPS`]).
//!
//! The app icons set the mark on an opaque white disc, so a launcher that masks the icon to a
//! circle crops nothing and one that does not still gets a shape rather than a floating mark.
//! That plate belongs to the installed-app icon alone: a browser tab draws its icon on its own
//! strip, where the bare mark is the one that reads, so the dashboards keep both and point
//! each consumer at the right one. The desktop image's start button and default wallpaper are
//! the bare mark too, at a size a wallpaper can center.
//!
//! Rasterizing is `rsvg-convert`'s (librsvg), which has to be on `PATH`. `--check` regenerates
//! into a scratch directory and reports which committed images are stale, without writing
//! any of them.

use std::fs;
use std::path::Path;
use std::process::Command;

use regex::Regex;

use crate::{Scratch, repo};

const MARK: &str = "docs/assets/logo/selkies.svg";
const WORDMARK: &str = "docs/assets/logo/wordmark.svg";
const HORIZONTAL: &str = "docs/assets/logo/horizontal.svg";
const VERTICAL: &str = "docs/assets/logo/vertical.svg";

/// Where a lockup puts the two pieces. The mark keeps its own internal transform and is placed
/// by `mark`; the wordmark is a bare path placed by `wordmark`.
struct Lockup {
    dest: &'static str,
    width: u32,
    height: u32,
    viewbox: &'static str,
    mark: &'static str,
    wordmark: &'static str,
}

const LOCKUPS: [Lockup; 2] = [
    Lockup {
        dest: HORIZONTAL,
        width: 308,
        height: 89,
        viewbox: "0 0 3463.466 1000.000",
        mark: "translate(-14.075 -145.442) scale(2.914093)",
        wordmark: "translate(1504.884 724.090) scale(0.295501)",
    },
    Lockup {
        dest: VERTICAL,
        width: 219,
        height: 245,
        viewbox: "0 0 1315.013 1471.000",
        mark: "translate(-14.075 -145.442) scale(2.914093)",
        wordmark: "translate(16.446 1467.171) scale(0.191433)",
    },
];

/// The mark verbatim, wherever a dashboard serves it.
const COPIES: [&str; 1] = ["web/selkies-dashboard/public/selkies.svg"];

/// Bare renders: the source, the width, the height, and the destination.
const RENDERS: [(&str, u32, u32, &str); 5] = [
    (HORIZONTAL, 1024, 296, "docs/assets/logo/horizontal.png"),
    (HORIZONTAL, 480, 139, "docs/assets/logo/horizontal-480.png"),
    (VERTICAL, 480, 537, "docs/assets/logo/vertical.png"),
    (MARK, 180, 180, "web/selkies-dashboard/public/icon.png"),
    (MARK, 512, 512, "docs/assets/logo/mark-512x512.png"),
];

/// Single-image 256x256 containers, which is what every consumer here expects.
const ICONS: [&str; 2] = [
    "docs/assets/logo/favicon.ico",
    "web/selkies-dashboard/public/favicon.ico",
];
const ICON_SIZE: u32 = 256;

/// The plated app icons, by side.
const PLATED: [(u32, &str); 3] = [
    (192, "docs/assets/logo/icon-192x192.png"),
    (512, "docs/assets/logo/icon-512x512.png"),
    (512, "web/selkies-dashboard/public/icon-512.png"),
];
// The plate's proportions, of the icon's side.
const PLATE_RADIUS: f64 = 0.4900;
const MARK_WIDTH: f64 = 0.7400;
const MARK_CENTER: (f64, f64) = (0.5000, 0.4844);

/// Mean per-channel difference `--check` calls stale: librsvg builds disagree on edge pixels
/// by single digits, artwork that moved on by an order of magnitude.
const STALE_THRESHOLD: f64 = 12.0;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    fs::create_dir_all(path.parent().unwrap_or(path)).map_err(io)?;
    fs::write(path, contents).map_err(io)
}

/// The first match of `pattern` in `text`, or the sentence that says `what` is missing.
fn find<'a>(pattern: &str, text: &'a str, what: &str) -> Result<&'a str, String> {
    let pattern = Regex::new(pattern).expect("a valid pattern");
    pattern
        .find(text)
        .map(|found| found.as_str())
        .ok_or_else(|| format!("the artwork has no {what}"))
}

/// Rasterize an SVG to a PNG at the given size with `rsvg-convert`.
fn rasterize(svg: &Path, width: u32, height: u32, out: &Path) -> Result<(), String> {
    fs::create_dir_all(out.parent().unwrap_or(out))
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let output = Command::new("rsvg-convert")
        .args(["-w", &width.to_string(), "-h", &height.to_string(), "-o"])
        .args([out, svg])
        .output()
        .map_err(|e| format!("rsvg-convert: {e}; install librsvg to regenerate the images"))?;
    if !output.status.success() {
        return Err(format!(
            "rsvg-convert {}: {}",
            svg.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// A decoded image: its size and its RGBA bytes.
struct Pixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Decode a PNG, or the PNG a single-image ICO contains.
fn decode(path: &Path) -> Result<Pixels, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut data = fs::read(path).map_err(|e| fail(&e))?;
    if !data.starts_with(PNG_SIGNATURE) {
        // An ICO: a 6-byte header, then 16-byte entries whose last field is the image's offset.
        let offset = data
            .get(18..22)
            .map(|at| u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize);
        let Some(image) = offset
            .and_then(|at| data.get(at..))
            .filter(|image| image.starts_with(PNG_SIGNATURE))
        else {
            return Err(fail(&"neither a PNG nor an icon holding one"));
        };
        data = image.to_vec();
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().map_err(|e| fail(&e))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| fail(&"too large to decode"))?;
    let mut buffer = vec![0; size];
    let frame = reader.next_frame(&mut buffer).map_err(|e| fail(&e))?;
    buffer.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::GrayscaleAlpha => buffer
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[gray, alpha]| [*gray, *gray, *gray, *alpha])
            .collect(),
        other => return Err(fail(&format!("unexpected color type {other:?}"))),
    };
    Ok(Pixels {
        width: frame.width,
        height: frame.height,
        rgba,
    })
}

/// Where the artwork sits inside its own canvas, as fractions of it: left, top, width, height.
///
/// Measured rather than read off the viewBox, because the mark does not fill its canvas and
/// the plate has to be placed against the ink, not the box.
fn mark_extent(svg: &Path, scratch: &Path) -> Result<(f64, f64, f64, f64), String> {
    const PROBE: u32 = 1024;
    let png = scratch.join("probe.png");
    rasterize(svg, PROBE, PROBE, &png)?;
    let probe = decode(&png)?;
    let inked = |x: u32, y: u32| probe.rgba[((y * probe.width + x) * 4 + 3) as usize] != 0;
    let columns: Vec<u32> = (0..probe.width)
        .filter(|&x| (0..probe.height).any(|y| inked(x, y)))
        .collect();
    let rows: Vec<u32> = (0..probe.height)
        .filter(|&y| (0..probe.width).any(|x| inked(x, y)))
        .collect();
    let (Some(left), Some(right), Some(top), Some(bottom)) =
        (columns.first(), columns.last(), rows.first(), rows.last())
    else {
        return Err(format!("{} renders nothing", svg.display()));
    };
    let fraction = |pixels: u32| f64::from(pixels) / f64::from(PROBE);
    Ok((
        fraction(*left),
        fraction(*top),
        fraction(right + 1 - left),
        fraction(bottom + 1 - top),
    ))
}

/// A source SVG's contents, without its root element.
fn inner(svg: &str) -> Result<&str, String> {
    let open = svg
        .find("<svg")
        .and_then(|at| Some(at + svg[at..].find('>')? + 1));
    match (open, svg.rfind("</svg>")) {
        (Some(open), Some(close)) if open <= close => Ok(&svg[open..close]),
        _ => Err("the artwork has no <svg> root".into()),
    }
}

/// A wrapper placing the mark, at the plate's proportions, on a white disc of `size`.
fn plated_svg(mark: &str, extent: (f64, f64, f64, f64), size: u32) -> Result<String, String> {
    let viewbox = find(r#"viewBox="[^"]+""#, mark, "viewBox")?;
    let (mx, my, mw, mh) = extent;
    let side = f64::from(size);
    // The ink, not the canvas, is MARK_WIDTH wide and centered on MARK_CENTER.
    let canvas = side * MARK_WIDTH / mw;
    let x = side * MARK_CENTER.0 - (mx + mw / 2.0) * canvas;
    let y = side * MARK_CENTER.1 - (my + mh / 2.0) * canvas;
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" viewBox=\"0 0 {size} {size}\">\n  \
         <circle cx=\"{:?}\" cy=\"{:?}\" r=\"{:?}\" fill=\"#ffffff\"/>\n  \
         <svg x=\"{x:.3}\" y=\"{y:.3}\" width=\"{canvas:.3}\" height=\"{canvas:.3}\" {viewbox}>{}</svg>\n</svg>\n",
        side / 2.0,
        side / 2.0,
        side * PLATE_RADIUS,
        inner(mark)?
    ))
}

/// A lockup: the mark and the wordmark, each placed by its own transform.
fn build_lockup(lockup: &Lockup, mark: &str, wordmark: &str) -> Result<String, String> {
    let gradient = find(r"(?s)<defs>.*?</defs>", mark, "<defs>")?;
    let art = find(
        r#"(?s)<g transform="translate\(0,460\) scale\(0\.1,-0\.1\)">.*?</g>"#,
        mark,
        "artwork group",
    )?;
    let path = find(r"<path[^>]*/>", inner(wordmark)?, "wordmark path")?;
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"{}\">\n  {gradient}\n  \
         <g transform=\"{}\">\n    {art}\n  </g>\n  {} transform=\"{}\"/>\n</svg>\n",
        lockup.width,
        lockup.height,
        lockup.viewbox,
        lockup.mark,
        path[..path.len() - 2].trim_end(),
        lockup.wordmark
    ))
}

/// A single-image ICO around a PNG of `size`, which the format stores as it is.
fn icon(png: &[u8], size: u32) -> Vec<u8> {
    // 256 is written as 0 in the entry's one-byte width and height.
    let side = if size >= 256 { 0 } else { size as u8 };
    let mut ico = vec![0, 0, 1, 0, 1, 0, side, side, 0, 0, 0, 0, 32, 0];
    ico.extend((png.len() as u32).to_le_bytes());
    ico.extend(22u32.to_le_bytes());
    ico.extend(png);
    ico
}

/// Write every image under `into` (the repository root, or a scratch copy) and name them.
fn generate(into: &Path, scratch: &Path) -> Result<Vec<&'static str>, String> {
    let repo = repo();
    let mark = read(&repo.join(MARK))?;
    let wordmark = read(&repo.join(WORDMARK))?;
    let mut produced = Vec::new();
    for lockup in &LOCKUPS {
        write(
            &into.join(lockup.dest),
            build_lockup(lockup, &mark, &wordmark)?,
        )?;
        produced.push(lockup.dest);
    }
    for dest in COPIES {
        write(&into.join(dest), &mark)?;
        produced.push(dest);
    }
    for (svg, width, height, dest) in RENDERS {
        // The lockups are the ones just written under `into`, or a check would grade fresh
        // images against stale artwork and always call them clean.
        let root = if svg == MARK { &repo } else { into };
        rasterize(&root.join(svg), width, height, &into.join(dest))?;
        produced.push(dest);
    }
    let png = scratch.join("icon.png");
    rasterize(&repo.join(MARK), ICON_SIZE, ICON_SIZE, &png)?;
    let png = fs::read(&png).map_err(|e| format!("{}: {e}", png.display()))?;
    for dest in ICONS {
        write(&into.join(dest), icon(&png, ICON_SIZE))?;
        produced.push(dest);
    }
    let extent = mark_extent(&repo.join(MARK), scratch)?;
    for (size, dest) in PLATED {
        let wrapper = scratch.join(format!("plated-{size}.svg"));
        write(&wrapper, plated_svg(&mark, extent, size)?)?;
        rasterize(&wrapper, size, size, &into.join(dest))?;
        produced.push(dest);
    }
    Ok(produced)
}

/// Why a freshly generated image says the committed one is stale, if it does.
fn staleness(dest: &str, fresh: &Path, committed: &Path) -> Result<Option<String>, String> {
    if !committed.exists() {
        return Ok(Some(format!("{dest} (missing)")));
    }
    if dest.ends_with(".svg") {
        let same = fs::read(fresh).ok() == fs::read(committed).ok();
        return Ok((!same).then(|| dest.to_string()));
    }
    let (a, b) = (decode(fresh)?, decode(committed)?);
    if (a.width, a.height) != (b.width, b.height) {
        return Ok(Some(format!(
            "{dest} (({}, {}) committed, ({}, {}) generated)",
            b.width, b.height, a.width, a.height
        )));
    }
    let total: u64 = a
        .rgba
        .iter()
        .zip(&b.rgba)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    let delta = total as f64 / a.rgba.len() as f64;
    Ok((delta > STALE_THRESHOLD).then(|| format!("{dest} (mean difference {delta:.1}/255)")))
}

/// Regenerate the images in place, or under `check` report the stale ones; `Ok(false)` when
/// one is.
pub fn run(check: bool) -> Result<bool, String> {
    let repo = repo();
    let stops = Regex::new(r##"stop-color="(#[0-9A-Fa-f]{6})""##).expect("a valid pattern");
    let mark = read(&repo.join(MARK))?;
    let stops: Vec<&str> = stops
        .captures_iter(&mark)
        .map(|stop| stop.extract::<1>().1[0])
        .collect();
    if stops.is_empty() {
        return Err(format!("{MARK} declares no gradient stops"));
    }
    println!("mark gradient: {}", stops.join(" "));

    let scratch = Scratch::new("selkies-logo")?;
    if !check {
        let produced = generate(&repo, &scratch.0)?;
        for dest in &produced {
            println!("  wrote {dest}");
        }
        println!("{} images regenerated", produced.len());
        return Ok(true);
    }
    let fresh = scratch.0.join("tree");
    let mut stale = Vec::new();
    for dest in generate(&fresh, &scratch.0)? {
        stale.extend(staleness(dest, &fresh.join(dest), &repo.join(dest))?);
    }
    if stale.is_empty() {
        println!("every committed image matches the artwork");
        return Ok(true);
    }
    println!("stale images:");
    for item in stale {
        println!("  {item}");
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARK_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 46 46\">\n<defs><linearGradient \
        id=\"g\"/></defs>\n<g transform=\"translate(0,460) scale(0.1,-0.1)\"><path d=\"M0 0\"/></g>\n</svg>\n";
    #[test]
    fn a_lockup_places_the_mark_and_the_wordmark_by_their_transforms() {
        let wordmark = "<svg viewBox=\"0 0 9 9\"><path d=\"M1 1\"\n  /></svg>";
        assert_eq!(
            build_lockup(&LOCKUPS[0], MARK_SVG, wordmark).unwrap(),
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"308\" height=\"89\" viewBox=\"0 0 3463.466 1000.000\">\n  \
             <defs><linearGradient id=\"g\"/></defs>\n  \
             <g transform=\"translate(-14.075 -145.442) scale(2.914093)\">\n    \
             <g transform=\"translate(0,460) scale(0.1,-0.1)\"><path d=\"M0 0\"/></g>\n  </g>\n  \
             <path d=\"M1 1\" transform=\"translate(1504.884 724.090) scale(0.295501)\"/>\n</svg>\n"
        );
    }

    #[test]
    fn the_plate_centers_the_ink_rather_than_the_canvas() {
        let plated = plated_svg(MARK_SVG, (0.25, 0.25, 0.5, 0.25), 192).unwrap();
        assert!(
            plated.contains("<circle cx=\"96.0\" cy=\"96.0\" r=\"94.08\" fill=\"#ffffff\"/>"),
            "{plated}"
        );
        assert!(
            plated.contains("<svg x=\"-46.080\" y=\"-13.555\" width=\"284.160\" height=\"284.160\" viewBox=\"0 0 46 46\">"),
            "{plated}"
        );
    }

    #[test]
    fn an_icon_is_one_entry_around_the_png() {
        let ico = icon(b"\x89PNG....", 256);
        assert_eq!(ico[..14], [0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 32, 0]);
        assert_eq!(ico[14..22], [8, 0, 0, 0, 22, 0, 0, 0]);
        assert_eq!(&ico[22..], b"\x89PNG....");
    }
}
