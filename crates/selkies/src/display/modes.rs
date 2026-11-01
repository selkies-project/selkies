/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Choosing and making RandR modes: CVT reduced-blanking timings computed natively, the
//! refresh a mode runs at, the refresh a display is held at, and the rule that picks a
//! mode by its geometry and refresh rather than by name.
//!
//! A driver lists several modes under one name, each at its own refresh: NVIDIA's pool
//! holds most sizes at 60 Hz beside the configured mode and lists 1024x768 at 43 Hz ahead
//! of the rest. A vsynced application presents at the refresh, so a mode taken by name
//! alone caps it at whatever rate the driver sorted first. `mode_at` therefore chooses by
//! geometry and refresh, at the configured refresh or the stream's frame rate where that is
//! higher (`target_refresh`), and makes a missing one at that rate under a name RandR
//! accepts beside the driver's. The selection is written over a `ModePool` and a
//! `ModeServer`, so the decision is checked without an X server and the connection
//! implements the two requests it needs.

use std::sync::Mutex;

use super::DisplayError;

/// The RandR mode flags a timing carries, as the protocol encodes them.
pub const HSYNC_POSITIVE: u32 = 1 << 0;
pub const VSYNC_NEGATIVE: u32 = 1 << 3;
pub const INTERLACE: u32 = 1 << 4;
pub const DOUBLE_SCAN: u32 = 1 << 5;

/// How far a mode's refresh may sit from the one asked for and still serve: the room a
/// driver's and CVT's clock rounding take (119.88 for 120), short of the step to the next
/// common rate.
pub const REFRESH_SLACK: f64 = 0.01;

/// The RandR mode timings a `CreateMode` request carries, minus the id and name length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeTimings {
    pub width: u32,
    pub height: u32,
    pub dot_clock: u64,
    pub h_sync_start: u32,
    pub h_sync_end: u32,
    pub h_total: u32,
    pub h_skew: u32,
    pub v_sync_start: u32,
    pub v_sync_end: u32,
    pub v_total: u32,
    pub flags: u32,
}

impl ModeTimings {
    /// The refresh these timings run at.
    pub fn refresh(&self) -> f64 {
        self.dot_clock as f64 / (self.h_total as f64 * self.v_total as f64)
    }
}

/// VESA CVT 1.2 reduced-blanking timings for `width` by `height` at `refresh`, as `cvt -r`
/// computes them: the width rounds up to the 8-pixel CVT cell while the vertical stays
/// exact.
pub fn cvt_rb_mode_info(width: u32, height: u32, refresh: f64) -> ModeTimings {
    let h_active = width.div_ceil(8) * 8;
    let v_active = height;
    let v_sync = if v_active.is_multiple_of(3) && v_active * 4 / 3 == h_active {
        4
    } else if v_active.is_multiple_of(9) && v_active * 16 / 9 == h_active {
        5
    } else if v_active.is_multiple_of(10) && v_active * 16 / 10 == h_active {
        6
    } else if (v_active.is_multiple_of(4) && v_active * 5 / 4 == h_active)
        || (v_active.is_multiple_of(9) && v_active * 15 / 9 == h_active)
    {
        7
    } else {
        10
    };
    let h_period_est = (1_000_000.0 / refresh - 460.0) / v_active as f64;
    let vbi_lines = ((460.0 / h_period_est) as u32 + 1).max(3 + v_sync + 6);
    let v_total = v_active + vbi_lines;
    let h_total = h_active + 160;
    let mut clock_khz = h_total as f64 * 1_000.0 / h_period_est;
    clock_khz -= clock_khz % 250.0;
    ModeTimings {
        width: h_active,
        height: v_active,
        dot_clock: (clock_khz.round() as u64) * 1_000,
        h_sync_start: h_active + 48,
        h_sync_end: h_active + 80,
        h_total,
        h_skew: 0,
        v_sync_start: v_active + 3,
        v_sync_end: v_active + 3 + v_sync,
        v_total,
        flags: HSYNC_POSITIVE | VSYNC_NEGATIVE,
    }
}

/// A RandR mode as the selection reads it: its id, geometry, and the timings its refresh
/// comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub dot_clock: u64,
    pub h_total: u32,
    pub v_total: u32,
    pub flags: u32,
}

impl Mode {
    /// The mode's refresh in Hz as xrandr reports it (the field rate of an interlaced
    /// mode), or 0 for a mode without timings.
    pub fn refresh(&self) -> f64 {
        let mut v_total = self.v_total as f64;
        if self.flags & DOUBLE_SCAN != 0 {
            v_total *= 2.0;
        }
        if self.flags & INTERLACE != 0 {
            v_total /= 2.0;
        }
        if self.h_total == 0 || v_total == 0.0 {
            return 0.0;
        }
        self.dot_clock as f64 / (self.h_total as f64 * v_total)
    }
}

/// The refresh the display ran at when a Selkies process first reached this X server: the
/// one it was configured with, which every mode it is given keeps. `None` where the mode
/// carried no timings to read, as a framebuffer server's first mode does, or before any
/// connection read it.
static CONFIGURED_REFRESH: Mutex<Option<f64>> = Mutex::new(None);

/// The configured refresh this process learned, if any.
pub fn configured_refresh() -> Option<f64> {
    *CONFIGURED_REFRESH.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record the configured refresh once per process; a second call keeps the first.
pub(crate) fn note_configured_refresh(rate: f64) {
    let mut slot = CONFIGURED_REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() && rate > 0.0 {
        *slot = Some(rate);
    }
}

/// The refresh a display mode is chosen or made at, from what is configured and what the
/// stream runs at.
///
/// The configured refresh, or the stream's frame rate where that is higher: a vsynced
/// application presents at the refresh and no faster, so a display slower than the stream
/// caps what the stream can show, and one slower than configured drops what the operator
/// asked for. 60 where neither is known. A framebuffer server paces its vblank by the
/// stream on its own, so there the rate only names the mode.
pub fn target_refresh_from(configured: Option<f64>, stream_fps: Option<f64>) -> f64 {
    let rate = configured.unwrap_or(0.0).max(stream_fps.unwrap_or(0.0));
    if rate > 0.0 { rate } else { 60.0 }
}

/// `target_refresh_from` with the refresh this process learned from the server.
pub fn target_refresh(stream_fps: Option<f64>) -> f64 {
    target_refresh_from(configured_refresh(), stream_fps)
}

/// The two RandR requests a mode selection may need, implemented by the X connection and
/// by a stand-in in tests.
pub trait ModeServer {
    /// Create a user mode named `name` from `timings`; returns its id.
    fn create_mode(&mut self, timings: &ModeTimings, name: &str) -> Result<u32, DisplayError>;
    /// Attach mode `mode` to output `output`.
    fn add_output_mode(&mut self, output: u32, mode: u32) -> Result<(), DisplayError>;
}

/// The modes a screen holds, their names, and which of them an output lists.
#[derive(Clone, Debug, Default)]
pub struct ModePool {
    pub modes: Vec<Mode>,
    pub names: Vec<(u32, String)>,
    pub on_output: Vec<u32>,
}

impl ModePool {
    /// The name of mode `id`, empty when it has none.
    pub fn name_of(&self, id: u32) -> &str {
        self.names
            .iter()
            .find(|(m, _)| *m == id)
            .map(|(_, n)| n.as_str())
            .unwrap_or("")
    }

    /// The mode with id `id`.
    pub fn mode(&self, id: u32) -> Option<&Mode> {
        self.modes.iter().find(|m| m.id == id)
    }
}

/// Resolve or create a `w` by `h` mode at `refresh` on output `out_id`.
///
/// Interlaced and doublescan modes never serve; one without timings, a framebuffer
/// server's, serves any refresh. A mode on the output is preferred, and one elsewhere
/// serves only under a name this made for the purpose. A missing mode is made from CVT-RB
/// timings at `refresh`, exactly `w` wide, and named `name` unless a mode holds that name
/// already, then with its rate appended, since RandR refuses a user mode under a name that
/// exists. CVT rounds the clock down to its quarter-megahertz step, which leaves the mode
/// slower than asked, and a quarter megahertz up runs a small mode fast (60.21 Hz at
/// 1280x720), so vsynced frames beat against the stream's rate; the clock is set to the
/// next 10 kHz up, an EDID timing's step, which keeps it neither slower than asked nor
/// faster by more than 0.01 Hz. Modes are owned by the creating connection, so this must
/// run on the retained connection for the mode to outlive the call.
///
/// Returns the mode's id, attached to the output, and its refresh.
pub fn mode_at(
    server: &mut dyn ModeServer,
    pool: &mut ModePool,
    out_id: u32,
    w: u32,
    h: u32,
    refresh: f64,
    name: &str,
) -> Result<(u32, f64), DisplayError> {
    let mut fits: Vec<(bool, f64, u32, f64)> = Vec::new();
    for m in &pool.modes {
        if (m.width, m.height) != (w, h) || m.flags & (INTERLACE | DOUBLE_SCAN) != 0 {
            continue;
        }
        let on_output = pool.on_output.contains(&m.id);
        if !on_output && pool.name_of(m.id).split('_').next() != Some(name) {
            continue;
        }
        let rate = m.refresh();
        let off = if rate > 0.0 {
            (rate - refresh).abs()
        } else {
            0.0
        };
        if off <= refresh * REFRESH_SLACK {
            fits.push((
                !on_output,
                off,
                m.id,
                if rate > 0.0 { rate } else { refresh },
            ));
        }
    }
    if let Some(&(detached, _, mode_id, rate)) = fits
        .iter()
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)))
    {
        if detached {
            server.add_output_mode(out_id, mode_id)?;
            pool.on_output.push(mode_id);
        }
        return Ok((mode_id, rate));
    }
    let taken = |n: &str| pool.names.iter().any(|(_, existing)| existing == n);
    let candidates = [
        name.to_string(),
        format!("{name}_{refresh:.0}"),
        format!("{name}_{refresh:.2}"),
    ];
    let Some(mode_name) = candidates.iter().find(|n| !taken(n)) else {
        return Err(DisplayError::Refused(format!(
            "no free name for a {w}x{h} mode at {refresh:.2} Hz"
        )));
    };
    let mut info = cvt_rb_mode_info(w, h, refresh);
    let exact = (info.h_total as f64 * info.v_total as f64 * refresh) as u64;
    info.dot_clock = exact.div_ceil(10_000) * 10_000;
    info.width = w;
    let mode_id = server.create_mode(&info, mode_name)?;
    pool.names.push((mode_id, mode_name.clone()));
    pool.modes.push(Mode {
        id: mode_id,
        width: info.width,
        height: info.height,
        dot_clock: info.dot_clock,
        h_total: info.h_total,
        v_total: info.v_total,
        flags: info.flags,
    });
    server.add_output_mode(out_id, mode_id)?;
    pool.on_output.push(mode_id);
    Ok((mode_id, info.refresh()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RandR mode whose timings give `hz`.
    fn mode(id: u32, w: u32, h: u32, hz: f64, flags: u32) -> Mode {
        let (h_total, v_total) = (w + 160, h + 30);
        let mut v_scan = v_total as f64;
        if flags & DOUBLE_SCAN != 0 {
            v_scan *= 2.0;
        }
        if flags & INTERLACE != 0 {
            v_scan /= 2.0;
        }
        Mode {
            id,
            width: w,
            height: h,
            dot_clock: (hz * h_total as f64 * v_scan).round() as u64,
            h_total,
            v_total,
            flags,
        }
    }

    /// A server recording what is created and attached.
    #[derive(Default)]
    struct Server {
        created: Vec<(String, ModeTimings)>,
        attached: Vec<u32>,
    }

    impl ModeServer for Server {
        fn create_mode(&mut self, timings: &ModeTimings, name: &str) -> Result<u32, DisplayError> {
            self.created.push((name.to_string(), *timings));
            Ok(0x900 + self.created.len() as u32 - 1)
        }

        fn add_output_mode(&mut self, _output: u32, mode: u32) -> Result<(), DisplayError> {
            self.attached.push(mode);
            Ok(())
        }
    }

    fn make_pool(modes: Vec<Mode>, names: Vec<(u32, &str)>, on_output: Vec<u32>) -> ModePool {
        ModePool {
            modes,
            names: names
                .into_iter()
                .map(|(id, n)| (id, n.to_string()))
                .collect(),
            on_output,
        }
    }

    fn nvidia_pool() -> ModePool {
        let modes = vec![
            mode(0x1bd, 1024, 768, 43.48, 0),
            mode(0x1be, 1024, 768, 60.0, 0),
            mode(0x1de, 1920, 1080, 119.88, 0),
            mode(0x1df, 1920, 1080, 59.96, 0),
            mode(0x1e1, 1920, 1080, 60.01, DOUBLE_SCAN),
            mode(0x1ee, 1600, 900, 59.95, 0),
            mode(0x1ef, 1280, 720, 119.9, INTERLACE),
        ];
        let names = modes
            .iter()
            .map(|m| (m.id, format!("{}x{}", m.width, m.height)))
            .collect();
        let on_output = modes.iter().map(|m| m.id).collect();
        ModePool {
            modes,
            names,
            on_output,
        }
    }

    fn pick(
        srv: &mut Server,
        pool: &mut ModePool,
        w: u32,
        h: u32,
        configured: Option<f64>,
        fps: f64,
        name: &str,
    ) -> (u32, f64) {
        mode_at(
            srv,
            pool,
            1,
            w,
            h,
            target_refresh_from(configured, Some(fps)),
            name,
        )
        .expect("mode")
    }

    #[test]
    fn modes_are_chosen_by_geometry_and_refresh() {
        let configured = Some(119.88);
        let mut srv = Server::default();
        let mut pool = nvidia_pool();
        let (got, _) = pick(
            &mut srv,
            &mut pool,
            1920,
            1080,
            configured,
            60.0,
            "1920x1080",
        );
        assert_eq!(got, 0x1de, "the configured size keeps the configured mode");
        assert!(srv.created.is_empty());

        let (got, reported) = pick(&mut srv, &mut pool, 1600, 900, configured, 60.0, "1600x900");
        let (name, info) = srv.created.last().cloned().expect("a mode was created");
        let rate = pool.mode(got).expect("mode").refresh();
        assert!(
            (reported - rate).abs() < 1e-6,
            "the refresh reported is the created mode's own"
        );
        assert_ne!(got, 0x1ee);
        assert_eq!(name, "1600x900_120");
        assert!((119.88..=119.89).contains(&rate), "{rate}");
        assert!(srv.attached.contains(&got));
        assert_eq!(info.width, 1600);

        let (again, _) = pick(&mut srv, &mut pool, 1600, 900, configured, 60.0, "1600x900");
        assert_eq!(again, got, "that mode serves the next resize to the size");
        assert_eq!(srv.created.len(), 1);

        let (got, _) = pick(&mut srv, &mut pool, 1280, 720, configured, 60.0, "1280x720");
        assert_ne!(got, 0x1ef, "an interlaced mode never serves");
        assert_eq!(srv.created.last().expect("created").0, "1280x720_120");
        let rate = pool.mode(got).expect("mode").refresh();
        assert!(
            (119.88..=119.89).contains(&rate),
            "a small mode is made at the rate asked: {rate}"
        );

        let (got, _) = pick(
            &mut srv,
            &mut pool,
            1920,
            1080,
            configured,
            144.0,
            "1920x1080",
        );
        let rate = pool.mode(got).expect("mode").refresh();
        assert_eq!(srv.created.last().expect("created").0, "1920x1080_144");
        assert!((144.0..=144.01).contains(&rate), "{rate}");

        pick(
            &mut srv,
            &mut pool,
            1366,
            768,
            configured,
            60.0,
            "selkies-1366x768",
        );
        let (name, info) = srv.created.last().cloned().expect("created");
        assert_eq!((name.as_str(), info.width), ("selkies-1366x768", 1366));

        let mut srv = Server::default();
        let mut pool = nvidia_pool();
        let (got, _) = pick(&mut srv, &mut pool, 1024, 768, Some(60.0), 30.0, "1024x768");
        assert_eq!(
            got, 0x1be,
            "a 60 Hz display skips the 43 Hz mode the driver lists first"
        );
        assert!(srv.created.is_empty());

        let mut srv = Server::default();
        let fb = mode(0x3c, 8192, 4096, 0.0, 0);
        let mut pool = make_pool(vec![fb], vec![(0x3c, "8192x4096")], vec![0x3c]);
        let (got, _) = pick(&mut srv, &mut pool, 8192, 4096, None, 120.0, "8192x4096");
        assert_eq!(
            got, 0x3c,
            "a framebuffer server's mode without timings serves any refresh"
        );
        assert!(srv.created.is_empty());
        assert_eq!(target_refresh_from(None, Some(144.0)), 144.0);
        assert_eq!(target_refresh_from(None, None), 60.0);

        let mut srv = Server::default();
        let other = mode(0x40, 1280, 720, 60.0, 0);
        let mut pool = make_pool(vec![other], vec![(0x40, "selkies-1280x720")], vec![]);
        let (got, _) = pick(&mut srv, &mut pool, 1280, 720, None, 60.0, "1280x720");
        assert_ne!(
            got, 0x40,
            "another output's exact mode is not taken for the framebuffer's"
        );
        assert_eq!(srv.created.last().expect("created").0, "1280x720");
    }

    #[test]
    fn cvt_rounds_width_to_the_cell() {
        let t = cvt_rb_mode_info(1366, 768, 60.0);
        assert_eq!((t.width, t.height), (1368, 768));
        assert!((t.refresh() - 60.0).abs() < 0.3, "{}", t.refresh());
        assert_eq!(t.flags, HSYNC_POSITIVE | VSYNC_NEGATIVE);
    }
}
