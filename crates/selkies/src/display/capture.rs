/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What both transports share about a display's capture: the `pixelflux::CaptureSettings`
//! population from the server settings and a display's live state
//! (`populate_capture_settings`), the frames every page of a display holds
//! (`CommonFrames`), the constants of the lost-frame repair, the structural settings whose
//! change restarts a capture (`structurally_same`), and the cursor payload a page is sent.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use base64::Engine as _;
use log::info;
use pixelflux::CaptureSettings;
use pixelflux::encoders::Codec;
use pixelflux::sink::CursorImage;

use super::wayland::wayland_output_id;
use crate::settings::{self, Settings};

/// How long a started capture may go without its first frame before the log says so. A
/// capture emits its first frame at once, damage or none, so a silence this long means the
/// capture never came up, and the page shows only its waiting message.
pub const FIRST_FRAME_WAIT_S: f64 = 5.0;

/// How many dropped frame ids a transport remembers, so it can hold back what would predict
/// from one. A frame predicts from one of the last few its encoder produced (pixelflux's
/// reference window), so nothing is lost by forgetting an older one; the ids are 16-bit, so
/// one is named again after 65536 frames.
pub const LOST_FRAME_MEMORY: usize = 64;

/// The shortest H.264 `frame_num` range, which every longer one is a multiple of: the frames
/// a multiple of it past a keyframe are where a stream's `frame_num` can wrap to 0. A
/// decoder that misses that frame cannot be predicted past the gap, so the encoder answers a
/// loss covering it with a keyframe.
pub const FRAME_NUM_WRAP: u32 = 16;

/// The bit of a video header's type byte (offset 1) a delta frame carries where the encoder
/// keeps it as a long-term reference predicting from a frame every page holds
/// (`CommonFrames`): every page can decode it, so it goes to each as a key frame does.
/// pixelflux sets it only once told which frames those are.
pub const FRAME_ANCHOR: u8 = 0x08;

/// The one line a capture that never delivered a frame earns.
pub fn no_first_frame(display_id: &str, encoder: &str) -> String {
    format!(
        "Capture for '{display_id}' ({encoder}) has delivered no frame in {FIRST_FRAME_WAIT_S:.0} s; the page is waiting for a stream that is not coming."
    )
}

/// The frames every page streaming one display holds, each told to the display's encoder
/// once (`acknowledge_reference`).
///
/// A page joins as its transport takes a key frame for it (`join`), holding nothing until
/// it holds that one, and holds each later frame it is sent, which predicts from one it
/// holds (a transport sends no other): over WebSockets once its relay writes the frame, over
/// WebRTC once transport-cc reported every packet of it received. It leaves when it waits
/// for a key frame again, and when it goes. An encoder told which frames every page holds
/// keeps the newest such long-term reference while it marks another, and predicts the next
/// from a frame every page holds (`FRAME_ANCHOR`), so each page recovers from a loss of any
/// depth on its own and the stream carries no key frame for it.
///
/// A second set counts the frames every page was handed, which it may yet lose: over
/// WebSockets once its relay queues the frame, over WebRTC once its packets leave the pacer.
/// The encoder predicts each anchor from one of those, so a page whose link holds frames
/// for a while costs no anchor reaching back to the last.
pub struct CommonFrames<P: Hash + Eq> {
    acknowledge: Box<dyn Fn(u16) + Send + Sync>,
    pages: HashMap<P, VecDeque<u16>>,
    told: VecDeque<u16>,
    key: Option<u16>,
}

impl<P: Hash + Eq> CommonFrames<P> {
    /// Frames told to `acknowledge` as every page comes to hold them.
    pub fn new(acknowledge: impl Fn(u16) + Send + Sync + 'static) -> Self {
        CommonFrames {
            acknowledge: Box::new(acknowledge),
            pages: HashMap::new(),
            told: VecDeque::with_capacity(LOST_FRAME_MEMORY),
            key: None,
        }
    }

    fn remember(held: &mut VecDeque<u16>, frame_id: u16) {
        if held.len() == LOST_FRAME_MEMORY {
            held.pop_front();
        }
        held.push_back(frame_id);
    }

    /// `page` is sent a key frame: no frame is common until it holds that.
    pub fn join(&mut self, page: P) {
        self.pages
            .insert(page, VecDeque::with_capacity(LOST_FRAME_MEMORY));
    }

    /// `page` holds `frame_id`, a key frame starting its holdings afresh; a frame every page
    /// now holds is told to the encoder.
    pub fn hold(&mut self, page: &P, frame_id: u16, key: bool)
    where
        P: Clone,
    {
        if key {
            self.pages
                .insert(page.clone(), VecDeque::with_capacity(LOST_FRAME_MEMORY));
            if self.key != Some(frame_id) {
                self.key = Some(frame_id);
                self.told.clear();
            }
        } else if !self.pages.contains_key(page) {
            return;
        }
        if let Some(held) = self.pages.get_mut(page) {
            Self::remember(held, frame_id);
        }
        if !self.told.contains(&frame_id)
            && self.pages.values().all(|other| other.contains(&frame_id))
        {
            Self::remember(&mut self.told, frame_id);
            (self.acknowledge)(frame_id);
        }
    }

    /// `page` waits for a key frame, or is gone.
    pub fn leave(&mut self, page: &P) {
        self.pages.remove(page);
    }

    /// How many pages hold frames.
    pub fn pages(&self) -> usize {
        self.pages.len()
    }
}

/// The per-display state a capture is started from, as each transport resolves it from its
/// own client record: the knobs `populate_capture_settings` copies into the shared fields.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayCapture {
    pub display_id: String,
    pub is_wayland: bool,
    pub scale: f64,
    pub framerate: f64,
    pub encoder: String,
    /// The caller's resolved choice (`settings::effective_use_cpu`).
    pub use_cpu: bool,
    pub cbr: bool,
    pub bitrate_kbps: f64,
    pub crf: i32,
    pub paintover_crf: i32,
    pub paintover_burst: i32,
    pub fullcolor: bool,
    pub ten_bit: bool,
    pub streaming: bool,
    pub use_paint_over_quality: bool,
    pub capture_cursor: bool,
    /// The input handler's DPI-scaled cursor cap; 0 for none.
    pub cursor_size_cap_hint: i32,
    pub cpu_encode_threading: bool,
}

/// Assign every `CaptureSettings` field the WebSocket and WebRTC paths share.
///
/// All shared knobs are set here, once -- a knob plumbed into only one path is a parity bug.
/// Callers keep the per-mode fields (geometry, JPEG quality, stripe-header framing).
///
/// `use_cpu` is the caller's resolved choice; which software H.264 encoder then runs is the
/// pixelflux build's. Device selection forwards only explicit paths, hardware detection being
/// pixelflux's: an `--encode-dri` path is authoritative, otherwise `--gpu-id` picks the
/// encoder device by index (-1 requests software encoding) and, unset, pixelflux encodes on
/// ID 0 -- the first GPU -- unless AUTO_GPU affinity aims it elsewhere. The compositor render
/// node is distinct: `--render-dri` wins, else pixelflux resolves `--auto-gpu` against the
/// machine. The cursor size is the Wayland compositor's theme size (X11 sets it on the X
/// server itself; `<= 0` keeps the theme default), and the cap tracks the input handler's
/// DPI-scaled value so pixelflux's XFixes monitor caps shapes the same way. On Wayland the
/// capture is bound to its compositor output (`display2` -> output 2), the id every
/// per-display IDR/rate/tunable call routes through. The watermark is burned into the frame
/// by pixelflux on every backend and stays server-side (never broadcast).
pub fn populate_capture_settings(cs: &mut CaptureSettings, server: &Settings, p: &DisplayCapture) {
    cs.target_fps = p.framerate;
    cs.capture_cursor = p.capture_cursor;
    cs.debug_logging = server.debug().on;

    let codec_name = settings::codec_for_encoder(&p.encoder);
    if let Some(codec) = Codec::parse(codec_name) {
        cs.codec = codec;
    }
    cs.video_crf = p.crf;
    cs.video_paintover_crf = p.paintover_crf;
    cs.video_paintover_burst_frames = p.paintover_burst;
    cs.video_fullcolor = p.fullcolor;
    cs.video_bit_depth = if p.ten_bit { 10 } else { 8 };
    cs.video_streaming_mode = p.streaming;
    cs.video_cbr_mode = p.cbr;
    cs.video_bitrate_kbps = p.bitrate_kbps.round() as i32;
    // 0 = infinite GOP (on-demand keyframes only); 0 QP bounds = encoder default.
    cs.keyframe_interval_s = server.keyframe_interval();
    cs.video_min_qp = server.video_min_qp() as i32;
    cs.video_max_qp = server.video_max_qp() as i32;
    cs.use_cpu = p.use_cpu;
    cs.cpu_encode_threading = p.cpu_encode_threading;
    if cs.use_cpu && p.encoder != "jpeg" {
        let library = settings::software_encoders()
            .get(codec_name)
            .copied()
            .unwrap_or("no software encoder in this pixelflux build");
        info!(
            target: "display",
            "Display '{}' encodes {} in software ({library}).",
            p.display_id,
            settings::codec_label(codec_name)
        );
    }

    cs.use_paint_over_quality = p.use_paint_over_quality;
    cs.paint_over_trigger_frames = 15;
    cs.damage_block_threshold = 10;
    cs.damage_block_duration = 20;

    let dri_node = server.encode_dri();
    if !dri_node.is_empty() {
        cs.encode_node_path = dri_node.to_string();
        cs.encode_node_index = settings::parse_dri_node_to_index(dri_node);
    } else if let Some(gid) = settings::parse_gpu_id(server.gpu_id()) {
        cs.encode_node_index = gid;
    }
    let render_dri = server.render_dri();
    if !render_dri.is_empty() {
        cs.render_node_path = render_dri.to_string();
    }
    cs.auto_gpu = server.auto_gpu().to_string();

    cs.use_wayland = Some(p.is_wayland);
    cs.recording_socket = server.recording_socket().to_string();
    cs.wayland_host_display = server.wayland_host_display().to_string();
    cs.cursor_size = server.cursor_size() as i32;
    cs.cursor_size_cap = if p.cursor_size_cap_hint > 0 {
        p.cursor_size_cap_hint
    } else {
        cs.cursor_size.max(32)
    };
    if p.is_wayland {
        cs.scale = p.scale;
        cs.display_id = wayland_output_id(Some(&p.display_id));
    }

    let watermark = server.watermark_path();
    if !watermark.is_empty() && std::path::Path::new(watermark).exists() {
        cs.watermark_path = watermark.to_string();
        cs.watermark_location_enum = server.watermark_location() as i32;
    }
    cs.acknowledge_references = true;
}

/// Whether two capture settings agree on everything structural, so a running capture is kept
/// across a reconfiguration: the codec, the encoder path, its threading, the chroma, the bit depth, and the rate-control mode. Everything else applies live.
pub fn structurally_same(a: &CaptureSettings, b: &CaptureSettings) -> bool {
    a.codec == b.codec
        && a.use_cpu == b.use_cpu
        && a.cpu_encode_threading == b.cpu_encode_threading
        && a.video_fullcolor == b.video_fullcolor
        && a.video_bit_depth == b.video_bit_depth
        && a.video_cbr_mode == b.video_cbr_mode
}

/// The cursor payload a client is sent: the PNG as base64, the image's real pixel size (not
/// the nominal cursor-size setting, since clients scale and place the hotspot against these
/// dimensions and capped shapes are rarely square), the hotspot, and the cache handle.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CursorPayload {
    pub curdata: String,
    pub width: u32,
    pub height: u32,
    pub hotx: i32,
    pub hoty: i32,
    pub handle: u32,
}

/// Encoder-independent cursor cache handle: a CRC over the image and its geometry, so a
/// client's cursor cache dedupes flips between the same shapes. Never 0: the wire contract
/// reserves handle 0 for hide.
pub fn cursor_content_handle(image: &[u8], width: u32, height: u32, hot_x: i32, hot_y: i32) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(image);
    let mut meta = Vec::with_capacity(16);
    meta.extend_from_slice(&(width as i32).to_le_bytes());
    meta.extend_from_slice(&(height as i32).to_le_bytes());
    meta.extend_from_slice(&hot_x.to_le_bytes());
    meta.extend_from_slice(&hot_y.to_le_bytes());
    crc.update(&meta);
    let handle = crc.sum();
    if handle == 0 { 1 } else { handle }
}

/// The size a PNG's IHDR declares, where the bytes are a PNG.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    (w > 0 && h > 0).then_some((w, h))
}

/// Translate a pixelflux cursor event into the client cursor payload.
///
/// `hide` clears the cursor; `png` carries an image; anything else (a transient extraction
/// failure) keeps the last good cursor, so `None` skips the event. The payload carries the
/// image's real pixel size from its PNG header, `size` where the bytes carry none.
pub fn format_cursor(image: &CursorImage, size: u32) -> Option<CursorPayload> {
    match image.kind {
        "hide" => Some(CursorPayload {
            curdata: String::new(),
            width: 0,
            height: 0,
            hotx: 0,
            hoty: 0,
            handle: 0,
        }),
        "png" if !image.png.is_empty() => {
            let (width, height) = png_size(&image.png).unwrap_or((size, size));
            Some(CursorPayload {
                curdata: base64::engine::general_purpose::STANDARD.encode(&image.png),
                width,
                height,
                hotx: image.hot_x,
                hoty: image.hot_y,
                handle: cursor_content_handle(&image.png, width, height, image.hot_x, image.hot_y),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn structural_fields_compare_reusable_when_unchanged() {
        let a = CaptureSettings::default();
        let b = CaptureSettings::default();
        assert!(structurally_same(&a, &b));
        let c = CaptureSettings {
            use_cpu: !a.use_cpu,
            ..CaptureSettings::default()
        };
        assert!(!structurally_same(&a, &c));
        let d = CaptureSettings {
            codec: Codec::Vp8,
            ..CaptureSettings::default()
        };
        assert!(!structurally_same(&a, &d));
        let e = CaptureSettings {
            video_crf: a.video_crf + 1,
            ..CaptureSettings::default()
        };
        assert!(
            structurally_same(&a, &e),
            "a live knob is no structural change"
        );
    }

    #[test]
    fn capture_settings_follow_the_display_and_the_server() {
        let server = Settings::parse(&["--cursor-size".to_string(), "40".to_string()], &|_| None);
        let mut cs = CaptureSettings::default();
        let display = DisplayCapture {
            display_id: "display3".into(),
            is_wayland: true,
            scale: 1.5,
            framerate: 72.0,
            encoder: "vp9enc".into(),
            use_cpu: false,
            cbr: true,
            bitrate_kbps: 8000.4,
            crf: 30,
            paintover_crf: 20,
            paintover_burst: 7,
            fullcolor: true,
            ten_bit: true,
            streaming: true,
            use_paint_over_quality: false,
            capture_cursor: true,
            cursor_size_cap_hint: 0,
            cpu_encode_threading: false,
        };
        populate_capture_settings(&mut cs, &server, &display);
        assert_eq!(cs.codec, Codec::Vp9);
        assert_eq!(cs.target_fps, 72.0);
        assert!(cs.video_cbr_mode && cs.video_fullcolor && cs.video_streaming_mode);
        assert_eq!(
            (cs.video_bit_depth, cs.video_bitrate_kbps, cs.video_crf),
            (10, 8000, 30)
        );
        assert_eq!(
            (cs.video_paintover_crf, cs.video_paintover_burst_frames),
            (20, 7)
        );
        assert_eq!(cs.use_wayland, Some(true));
        assert_eq!((cs.display_id, cs.scale), (3, 1.5));
        assert_eq!(
            (cs.cursor_size, cs.cursor_size_cap),
            (40, 40),
            "the cap follows the theme size where no hint is given"
        );
        assert!(cs.acknowledge_references && cs.capture_cursor && !cs.cpu_encode_threading);
        assert_eq!(
            cs.encode_node_index,
            CaptureSettings::default().encode_node_index,
            "no node named leaves pixelflux's pick"
        );

        let x11 = DisplayCapture {
            encoder: "h264enc".into(),
            is_wayland: false,
            cursor_size_cap_hint: 96,
            ..display
        };
        let mut cs = CaptureSettings::default();
        populate_capture_settings(&mut cs, &server, &x11);
        assert_eq!(cs.codec, Codec::H264);
        assert_eq!(cs.use_wayland, Some(false));
        assert_eq!(cs.cursor_size_cap, 96);
        assert_eq!(cs.display_id, 0, "X11 keeps the default output");
    }

    #[test]
    fn common_frames_are_told_once_every_page_holds_them() {
        let told = Arc::new(Mutex::new(Vec::new()));
        let sink = told.clone();
        let mut common: CommonFrames<&str> =
            CommonFrames::new(move |id| sink.lock().unwrap().push(id));
        common.join("a");
        common.join("b");
        common.hold(&"a", 1, true);
        common.hold(&"b", 1, true);
        common.hold(&"a", 2, false);
        assert!(
            told.lock().unwrap().as_slice() == [1],
            "{:?}",
            told.lock().unwrap()
        );
        common.hold(&"b", 2, false);
        assert_eq!(told.lock().unwrap().as_slice(), [1, 2]);
        common.hold(&"b", 2, false);
        assert_eq!(
            told.lock().unwrap().as_slice(),
            [1, 2],
            "a frame is told once"
        );
        common.leave(&"b");
        common.hold(&"a", 3, false);
        assert_eq!(
            told.lock().unwrap().as_slice(),
            [1, 2, 3],
            "a page that left no longer gates"
        );
        common.hold(&"c", 4, false);
        assert_eq!(
            told.lock().unwrap().as_slice(),
            [1, 2, 3],
            "a page that never joined holds nothing"
        );
        common.hold(&"a", 9, true);
        common.hold(&"a", 10, false);
        assert_eq!(
            told.lock().unwrap().as_slice(),
            [1, 2, 3, 9, 10],
            "a key frame starts the holdings afresh"
        );
    }

    #[test]
    fn cursor_payloads() {
        let hide = CursorImage {
            kind: "hide",
            png: Vec::new(),
            hot_x: 0,
            hot_y: 0,
        };
        assert_eq!(format_cursor(&hide, 32).map(|p| p.handle), Some(0));
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&24u32.to_be_bytes());
        png.extend_from_slice(&40u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let image = CursorImage {
            kind: "png",
            png: png.clone(),
            hot_x: 3,
            hot_y: 5,
        };
        let payload = format_cursor(&image, 32).expect("payload");
        assert_eq!(
            (payload.width, payload.height, payload.hotx, payload.hoty),
            (24, 40, 3, 5)
        );
        assert_ne!(payload.handle, 0);
        assert_eq!(
            payload.curdata,
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let moved = CursorImage {
            kind: "png",
            png,
            hot_x: 4,
            hot_y: 5,
        };
        assert_ne!(
            format_cursor(&moved, 32).expect("payload").handle,
            payload.handle,
            "the hotspot is part of the handle"
        );
        let error = CursorImage {
            kind: "error",
            png: Vec::new(),
            hot_x: 0,
            hot_y: 0,
        };
        assert_eq!(format_cursor(&error, 32), None);
        assert_eq!(
            no_first_frame("primary", "h264enc"),
            "Capture for 'primary' (h264enc) has delivered no frame in 5 s; the page is waiting for a stream that is not coming."
        );
    }
}
