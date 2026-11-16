/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The moving scene of `motion-scene` as a fullscreen client of the pixelflux compositor.
//!
//! Draws what the X11 scene draws, frame for frame and at the same places, into `wl_shm`
//! buffers at the size the compositor configures, again after each resize: the background, the
//! strip of squares spelling the frame index, the scrolling checkerboard band, the stepping bar
//! and the band of noise along the bottom (its bytes the stream of Python's
//! `random.Random(1).randbytes`, as the scene's checkers know it). The buffers live in a memfd
//! sealed against shrinking, as toolkits allocate theirs, and the pool is replaced by a fresh one
//! every `REPLACE_FRAMES` frames, as a toolkit replaces its buffers. Prints
//! `{"kind": "mapped", "w", "h"}` whenever a new pool's first frame is shown, and exits after
//! `WLOBS_DURATION` seconds (60) or on SIGTERM.
//!
//! ```text
//! XDG_RUNTIME_DIR=... wl-motion-scene SOCKET
//! ```

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use selkies_suites::scene::{
    BAND_H, BAND_STEP, BAND_Y, BAR_STEP, BAR_W, CHECK, CODE_BITS, CODE_GAP, CODE_SIZE, CODE_X,
    CODE_Y, NOISE_H, NOISE_IMAGES, Twister,
};
use selkies_suites::wayland::{self, emit};

const FPS: f64 = 60.0;
const BUFFERS: usize = 3;
const REPLACE_FRAMES: u32 = 300;
// XRGB8888 is little-endian: blue, green, red, unused.
const BACKGROUND: [u8; 4] = [0x78, 0x28, 0x1E, 0xFF];
const BAR: [u8; 4] = [0x28, 0x3C, 0xDC, 0xFF];
const BLACK: [u8; 4] = [0, 0, 0, 0xFF];
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

#[derive(Default)]
struct Scene {
    configured: bool,
    closed: bool,
    asked: (i32, i32),
    /// Which buffers of the pool in use the compositor has released, by the pool's generation.
    free: Vec<bool>,
    generation: u64,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Scene {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Scene {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Scene {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Scene {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                state.asked = (width, height);
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u64, usize)> for Scene {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &(u64, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && data.0 == state.generation
            && let Some(free) = state.free.get_mut(data.1)
        {
            *free = true;
        }
    }
}

delegate_noop!(Scene: ignore wl_compositor::WlCompositor);
delegate_noop!(Scene: ignore wl_surface::WlSurface);
delegate_noop!(Scene: ignore wl_shm::WlShm);
delegate_noop!(Scene: ignore wl_shm_pool::WlShmPool);

/// Three buffers of one size in a sealed memfd, and the band and noise rows frames are cut from.
struct Canvas {
    w: usize,
    h: usize,
    noise_h: usize,
    size: usize,
    map: *mut u8,
    pool: wl_shm_pool::WlShmPool,
    buffers: Vec<wl_buffer::WlBuffer>,
    band_rows: Vec<Vec<u8>>,
    noises: Vec<Vec<Vec<u8>>>,
    drawn: u32,
}

impl Canvas {
    fn new(
        scene: &mut Scene,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<Scene>,
        w: i32,
        h: i32,
    ) -> Result<Canvas, String> {
        let (wu, hu) = (
            usize::try_from(w).map_err(|e| e.to_string())?,
            usize::try_from(h).map_err(|e| e.to_string())?,
        );
        let stride = wu * 4;
        let size = stride * hu;
        let total = size * BUFFERS;
        // SAFETY: a NUL-terminated name and flags memfd_create takes; the descriptor is new.
        let raw = unsafe {
            libc::memfd_create(
                c"wl-motion-scene".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a descriptor this call just opened and nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let file = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
        file.set_len(u64::try_from(total).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        // SAFETY: fcntl on a descriptor this process owns, with an int argument.
        unsafe {
            libc::fcntl(raw, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK);
        }
        // SAFETY: a shared mapping of the whole file, which stays at least this long (sealed).
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        let map = map.cast::<u8>();
        // SAFETY: the mapping is `total` bytes long and this process's alone to write.
        let all = unsafe { std::slice::from_raw_parts_mut(map, total) };
        for px in all.as_chunks_mut::<4>().0 {
            *px = BACKGROUND;
        }
        let pool = shm.create_pool(
            fd.as_fd(),
            i32::try_from(total).map_err(|e| e.to_string())?,
            qh,
            (),
        );
        scene.generation += 1;
        scene.free = vec![true; BUFFERS];
        let generation = scene.generation;
        let buffers = (0..BUFFERS)
            .map(|b| {
                pool.create_buffer(
                    i32::try_from(b * size).unwrap_or(0),
                    w,
                    h,
                    i32::try_from(stride).unwrap_or(0),
                    wl_shm::Format::Xrgb8888,
                    qh,
                    (generation, b),
                )
            })
            .collect();
        let period = 2 * usize::try_from(CHECK).unwrap_or(16);
        let check = usize::try_from(CHECK).unwrap_or(16);
        let band_rows = (0..usize::from(BAND_H))
            .map(|y| {
                (0..wu + period)
                    .flat_map(|x| {
                        if ((x / check) + (y / check)).is_multiple_of(2) {
                            WHITE
                        } else {
                            BLACK
                        }
                    })
                    .collect()
            })
            .collect();
        let noise_h = usize::from(NOISE_H).min(hu);
        // randbytes(n) is getrandbits(8n) as little-endian bytes: the generator's words in order,
        // each little-endian.
        let mut rng = Twister::python(1);
        let noises = (0..if noise_h > 0 { NOISE_IMAGES } else { 0 })
            .map(|_| {
                (0..noise_h)
                    .map(|_| {
                        (0..2 * stride / 4)
                            .flat_map(|_| rng.next_u32().to_le_bytes())
                            .collect()
                    })
                    .collect()
            })
            .collect();
        Ok(Canvas {
            w: wu,
            h: hu,
            noise_h,
            size,
            map,
            pool,
            buffers,
            band_rows,
            noises,
            drawn: 0,
        })
    }

    /// The frame for `index` into a free buffer, committed; false where none is free.
    fn draw(&mut self, scene: &mut Scene, surface: &wl_surface::WlSurface, index: u32) -> bool {
        let Some(b) = scene.free.iter().position(|f| *f) else {
            return false;
        };
        scene.free[b] = false;
        let (w, stride) = (self.w, self.w * 4);
        // SAFETY: buffer `b` is `size` bytes of the mapping, and the compositor released it.
        let buf = unsafe { std::slice::from_raw_parts_mut(self.map.add(b * self.size), self.size) };
        let mut code: Vec<u8> = (0..w).flat_map(|_| BACKGROUND).collect();
        let (size, gap, cx) = (
            usize::try_from(CODE_SIZE).unwrap_or(0),
            usize::try_from(CODE_GAP).unwrap_or(0),
            usize::try_from(CODE_X).unwrap_or(0),
        );
        for bit in 0..CODE_BITS {
            let x = cx + usize::try_from(bit).unwrap_or(0) * (size + gap);
            let color = if (index >> bit) & 1 == 1 {
                WHITE
            } else {
                BLACK
            };
            let stop = ((x + size) * 4).min(code.len());
            for px in code[x * 4..stop].as_chunks_mut::<4>().0 {
                *px = color;
            }
        }
        let cy = usize::try_from(CODE_Y).unwrap_or(0);
        for y in cy..cy + size {
            buf[y * stride..(y + 1) * stride].copy_from_slice(&code);
        }
        let period = 2 * CHECK;
        let off = usize::try_from((index * BAND_STEP) % period).unwrap_or(0);
        let band_y = usize::try_from(BAND_Y).unwrap_or(0);
        for (y, row) in self.band_rows.iter().enumerate() {
            let at = (band_y + y) * stride;
            buf[at..at + stride].copy_from_slice(&row[off * 4..(off + w) * 4]);
        }
        let bar_bottom = self.h - self.noise_h;
        let bar_w = usize::try_from(BAR_W).unwrap_or(0);
        let x = usize::try_from(
            (index * BAR_STEP) % u32::try_from(w.saturating_sub(bar_w).max(1)).unwrap_or(1),
        )
        .unwrap_or(0);
        let bar: Vec<u8> = (0..w)
            .flat_map(|i| {
                if i >= x && i < x + bar_w {
                    BAR
                } else {
                    BACKGROUND
                }
            })
            .collect();
        for y in band_y + usize::from(BAND_H)..bar_bottom {
            buf[y * stride..(y + 1) * stride].copy_from_slice(&bar);
        }
        if !self.noises.is_empty() {
            let rows = &self.noises[usize::try_from(index).unwrap_or(0) % NOISE_IMAGES];
            let off = usize::try_from(index * 97).unwrap_or(0) % w;
            for (y, row) in rows.iter().enumerate() {
                let at = (bar_bottom + y) * stride;
                buf[at..at + stride].copy_from_slice(&row[off * 4..(off + w) * 4]);
            }
        }
        surface.attach(Some(&self.buffers[b]), 0, 0);
        let h = i32::try_from(self.h).unwrap_or(0);
        surface.damage_buffer(
            0,
            i32::from(CODE_Y),
            i32::try_from(w).unwrap_or(0),
            h - i32::from(CODE_Y),
        );
        surface.commit();
        self.drawn += 1;
        true
    }

    fn close(self) {
        for buffer in &self.buffers {
            buffer.destroy();
        }
        self.pool.destroy();
        // SAFETY: the mapping this canvas made, unmapped once, with nothing borrowing it.
        unsafe {
            libc::munmap(self.map.cast(), self.size * BUFFERS);
        }
    }
}

fn run(socket: &str) -> Result<(), String> {
    let duration = std::env::var("WLOBS_DURATION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(60.0);
    let conn = wayland::connect(socket)?;
    let (globals, mut queue) = registry_queue_init::<Scene>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut scene = Scene {
        asked: (1280, 720),
        ..Scene::default()
    };
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm: {e}"))?;
    let wm: xdg_wm_base::XdgWmBase = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("xdg_wm_base: {e}"))?;
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("motion-scene".into());
    toplevel.set_fullscreen(None);
    surface.commit();
    while !scene.configured {
        queue
            .blocking_dispatch(&mut scene)
            .map_err(|e| e.to_string())?;
    }
    let mut canvas: Option<Canvas> = None;
    let mut index = 0u32;
    let end = Instant::now() + Duration::from_secs_f64(duration.max(0.0));
    let mut next = Instant::now();
    let period = Duration::from_secs_f64(1.0 / FPS);
    while Instant::now() < end && !wayland::terminated() && !scene.closed {
        let stale = canvas.as_ref().is_none_or(|c| {
            (
                i32::try_from(c.w).unwrap_or(0),
                i32::try_from(c.h).unwrap_or(0),
            ) != scene.asked
                || c.drawn >= REPLACE_FRAMES
        });
        if stale {
            if let Some(old) = canvas.take() {
                old.close();
            }
            let (w, h) = scene.asked;
            let mut fresh = Canvas::new(&mut scene, &shm, &qh, w, h)?;
            fresh.draw(&mut scene, &surface, index);
            queue.roundtrip(&mut scene).map_err(|e| e.to_string())?;
            emit(
                "mapped",
                &[("w", Value::from(fresh.w)), ("h", Value::from(fresh.h))],
            );
            canvas = Some(fresh);
        }
        wayland::dispatch_until(&mut queue, &mut scene, next.min(end))?;
        if Instant::now() >= next {
            let next_index = (index + 1) % (1 << CODE_BITS);
            if let Some(c) = canvas.as_mut()
                && c.draw(&mut scene, &surface, next_index)
            {
                index = next_index;
            }
            next = (next + period).max(Instant::now());
        }
        queue.flush().map_err(|e| e.to_string())?;
    }
    if let Some(c) = canvas {
        c.close();
    }
    Ok(())
}

fn main() -> ExitCode {
    selkies_suites::die_with_parent();
    wayland::watch_for_termination();
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wayland-1".into());
    match run(&socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wl-motion-scene: {e}");
            ExitCode::FAILURE
        }
    }
}
