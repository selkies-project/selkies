/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A moving scene whose every frame says which frame it is.
//!
//! Fills an X display with a solid background, a strip of squares spelling the frame index in
//! binary, a bar that steps along the width each frame, and a scrolling checkerboard band, all
//! placed by the frame index alone. A viewer that reads the index off a decoded picture can
//! rebuild the frame it should be looking at and count the pixels that disagree; a picture
//! decoded against the wrong reference disagrees everywhere the scene moved. A band of noise
//! along the bottom, fresh every frame so no motion search can predict it, costs the encoder what
//! real content does and keeps a constant-bitrate stream at its configured rate; it is not
//! rebuilt. A noise height of 0 leaves it out, for a moving scene every encoder fits a low rate
//! with.
//!
//! ```text
//! motion-scene DISPLAY WIDTH HEIGHT FPS [NOISE_HEIGHT]
//! ```
//!
//! Each frame is drawn off screen and copied to the window in one request, so a grab of the
//! screen always holds a whole frame. The noise is the noise the suites' checkers know: four
//! images of Python's `random.Random(1).getrandbits(24)` colors, row by row.

use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, ImageFormat, ImageOrder,
    Rectangle, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use selkies_suites::scene::{
    BACKGROUND, BAND_H, BAND_STEP, BAND_Y, BAR, BAR_STEP, BAR_W, BLACK, CHECK, CODE_BITS, CODE_GAP,
    CODE_SIZE, CODE_X, CODE_Y, NOISE_H, NOISE_IMAGES, Twister, WHITE,
};

/// Upload `pixels` (0x00RRGGBB, row by row) into `pixmap`, in as many requests as the server's
/// request size needs.
fn put(
    connection: &RustConnection,
    pixmap: u32,
    gc: u32,
    width: u16,
    pixels: &[u32],
) -> Result<(), String> {
    let row = usize::from(width) * 4;
    let per = (connection.maximum_request_bytes().saturating_sub(64) / row).max(1);
    let rows = pixels.len() / usize::from(width);
    let mut y = 0;
    while y < rows {
        let n = per.min(rows - y);
        let mut bytes = Vec::with_capacity(n * row);
        for &p in &pixels[y * usize::from(width)..(y + n) * usize::from(width)] {
            bytes.extend_from_slice(&p.to_le_bytes());
        }
        connection
            .put_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                gc,
                width,
                u16::try_from(n).map_err(|e| e.to_string())?,
                0,
                i16::try_from(y).map_err(|e| e.to_string())?,
                0,
                24,
                &bytes,
            )
            .map_err(|e| e.to_string())?;
        y += n;
    }
    Ok(())
}

fn number(arg: Option<&String>, what: &str) -> Result<u32, String> {
    arg.ok_or_else(|| format!("no {what}"))?
        .parse()
        .map_err(|e| format!("{what}: {e}"))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let display = args
        .get(1)
        .ok_or("usage: motion-scene DISPLAY WIDTH HEIGHT FPS [NOISE_HEIGHT]")?;
    let width = u16::try_from(number(args.get(2), "width")?).map_err(|e| e.to_string())?;
    let height = u16::try_from(number(args.get(3), "height")?).map_err(|e| e.to_string())?;
    let fps = number(args.get(4), "fps")?.max(1);
    let noise_h = match args.get(5) {
        Some(_) => {
            u16::try_from(number(args.get(5), "noise height")?).map_err(|e| e.to_string())?
        }
        None => NOISE_H,
    };

    let (connection, screen) =
        RustConnection::connect(Some(display)).map_err(|e| format!("{display}: {e}"))?;
    let setup = connection.setup();
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == 24)
        .ok_or("no 24-bit pixmap format")?;
    if format.bits_per_pixel != 32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err("the scene draws 32-bit little-endian pixels only".to_owned());
    }
    let root = setup.roots.get(screen).ok_or("no screen")?;
    let (root_window, depth) = (root.root, root.root_depth);
    let id = || connection.generate_id().map_err(|e| e.to_string());
    let window = id()?;
    connection
        .create_window(
            depth,
            window,
            root_window,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(BACKGROUND),
        )
        .map_err(|e| e.to_string())?;
    let gc = id()?;
    connection
        .create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))
        .map_err(|e| e.to_string())?;

    let period = 2 * CHECK;
    let band_w = width + u16::try_from(period).map_err(|e| e.to_string())?;
    let band = id()?;
    connection
        .create_pixmap(depth, band, window, band_w, BAND_H)
        .map_err(|e| e.to_string())?;
    let checker: Vec<u32> = (0..u32::from(BAND_H))
        .flat_map(|y| {
            (0..u32::from(band_w)).map(move |x| {
                if ((x / CHECK) + (y / CHECK)).is_multiple_of(2) {
                    WHITE
                } else {
                    BLACK
                }
            })
        })
        .collect();
    put(&connection, band, gc, band_w, &checker)?;

    let noise_w = 2 * width;
    let mut rng = Twister::python(1);
    let mut noises = Vec::new();
    for _ in 0..(if noise_h > 0 { NOISE_IMAGES } else { 0 }) {
        let noise = id()?;
        connection
            .create_pixmap(depth, noise, window, noise_w, noise_h)
            .map_err(|e| e.to_string())?;
        let pixels: Vec<u32> = (0..usize::from(noise_w) * usize::from(noise_h))
            .map(|_| rng.bits24())
            .collect();
        put(&connection, noise, gc, noise_w, &pixels)?;
        noises.push(noise);
    }
    let frame = id()?;
    connection
        .create_pixmap(depth, frame, window, width, height)
        .map_err(|e| e.to_string())?;
    connection.map_window(window).map_err(|e| e.to_string())?;

    let bar_bottom = i16::try_from(height.saturating_sub(noise_h)).map_err(|e| e.to_string())?;
    let bar_top = BAND_Y + i16::try_from(BAND_H).map_err(|e| e.to_string())?;
    let fill = |color: u32, rect: Rectangle| -> Result<(), String> {
        connection
            .change_gc(gc, &ChangeGCAux::new().foreground(color))
            .map_err(|e| e.to_string())?;
        connection
            .poly_fill_rectangle(frame, gc, &[rect])
            .map_err(|e| e.to_string())?;
        Ok(())
    };
    let draw = |index: u32| -> Result<(), String> {
        fill(
            BACKGROUND,
            Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            },
        )?;
        let shift = u16::try_from((index * BAND_STEP) % period).map_err(|e| e.to_string())?;
        connection
            .copy_area(
                band,
                frame,
                gc,
                i16::try_from(shift).unwrap_or(0),
                0,
                0,
                BAND_Y,
                width,
                BAND_H,
            )
            .map_err(|e| e.to_string())?;
        let x = (index * BAR_STEP) % (u32::from(width).saturating_sub(BAR_W).max(1));
        if bar_bottom > bar_top {
            fill(
                BAR,
                Rectangle {
                    x: i16::try_from(x).map_err(|e| e.to_string())?,
                    y: bar_top,
                    width: u16::try_from(BAR_W).map_err(|e| e.to_string())?,
                    height: u16::try_from(bar_bottom - bar_top).map_err(|e| e.to_string())?,
                },
            )?;
        }
        if !noises.is_empty() {
            let source = noises[usize::try_from(index).unwrap_or(0) % NOISE_IMAGES];
            let shift = (index * 97) % u32::from(width);
            connection
                .copy_area(
                    source,
                    frame,
                    gc,
                    i16::try_from(shift).map_err(|e| e.to_string())?,
                    0,
                    0,
                    bar_bottom,
                    width,
                    noise_h,
                )
                .map_err(|e| e.to_string())?;
        }
        for bit in 0..CODE_BITS {
            let x = CODE_X + i16::try_from(bit).unwrap_or(0) * (CODE_SIZE + CODE_GAP);
            let size = u16::try_from(CODE_SIZE).unwrap_or(0);
            fill(
                if (index >> bit) & 1 == 1 {
                    WHITE
                } else {
                    BLACK
                },
                Rectangle {
                    x,
                    y: CODE_Y,
                    width: size,
                    height: size,
                },
            )?;
        }
        connection
            .copy_area(frame, window, gc, 0, 0, 0, 0, width, height)
            .map_err(|e| e.to_string())?;
        connection.flush().map_err(|e| e.to_string())?;
        Ok(())
    };

    let interval = Duration::from_millis(u64::from((1000 + fps / 2) / fps).max(1));
    let mut index = 0u32;
    draw(index)?;
    let mut next = Instant::now() + interval;
    loop {
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
        next += interval;
        index = (index + 1) % (1 << CODE_BITS);
        draw(index)?;
        // A connection the server dropped ends the scene rather than spinning.
        if let Err(e) = connection.poll_for_event() {
            return Err(e.to_string());
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("motion-scene: {e}");
        std::process::exit(1);
    }
}
