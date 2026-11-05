/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The screen read where the X server keeps it: SELKIES-FRAMEBUFFER, which the Selkies images'
//! Xvfb offers for a screen held in system memory, in place of XShm's per-frame copy and
//! XDamage's round trips.
//!
//! The server keeps the screen in a memfd it hands over read-only, and hands each watch a
//! memfd of its own and an eventfd: the watch is a bitmap of the `TILE` x `TILE` tiles drawn
//! into since the client last cleared them, which the server sets as it draws, and `armed`,
//! which the client sets before it waits on the eventfd and the server clears as it signals
//! once a drawing operation ends. A frame is then the tiles drawn since the surface it lands in
//! last held the screen: the capture's pool rotates its surfaces, so each keeps the tiles it
//! is missing ([`FramebufferGrab`]), and the cursor and the watermark composited onto one count
//! among them. The server sets a tile for every drawing that reaches the screen, so the tiles
//! drawn since the last frame, with the overlays that moved or changed their image, are the
//! frame's whole change, which the pipeline takes in place of a content hash.
//!
//! The framebuffer's head counts drawing operations, odd while one runs, so a read is whole
//! where the count stood even before it and unchanged after it; one an operation ran across
//! reads again what that operation drew, and a frame that cannot get a whole read in
//! `READ_ATTEMPTS` is grabbed through XShm, which the server answers between operations. A
//! resize moves the screen to a framebuffer of its new size and retires the old one, which
//! says so in its head and signals the watch; the capture then watches again.
//!
//! The path is taken only where the server answers the request, the framebuffer is sealed
//! against shrinking (a mapping a shrink cut short would fault the read), its head agrees with
//! the reply, and its pixels match what `GetImage` returns: that the extension is there is not
//! proof the memory holds the screen.

use std::io::IoSlice;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ParseError, ReplyError};
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::TryParseFd;

const EXTENSION: &str = "SELKIES-FRAMEBUFFER";
const MAGIC: u32 = 0x4246_4b53;
const VERSION: u32 = 1;

/// Byte offsets of the framebuffer head's fields the client reads.
const HEAD_WIDTH: usize = 8;
const HEAD_HEIGHT: usize = 12;
const HEAD_STRIDE: usize = 16;
const HEAD_BPP: usize = 20;
const HEAD_RETIRED: usize = 28;
const HEAD_SEQ: usize = 32;

/// Byte offsets of the watch's `tile`, `tiles_x`, `tiles_y`, and `armed`, and of its bitmap.
const WATCH_TILE: usize = 8;
const WATCH_TILES_X: usize = 12;
const WATCH_TILES_Y: usize = 16;
const WATCH_ARMED: usize = 20;
const WATCH_BITS: usize = 24;

/// Reads a frame tries for a whole one before it is grabbed through XShm instead.
const READ_ATTEMPTS: u32 = 4;

/// Bytes of tiles past which a read copies its rows of tiles on `COPY_THREADS` threads
/// (`SharedFramebuffer::copy_tiles`); below it, starting them costs more than it saves. Scoped
/// threads of the copy's own rather than the rayon pool: waking the pool's every worker for a
/// copy a frame kept them spinning, 1.9 cores more at 2560x1600 and 60 fps.
const PARALLEL_BYTES: usize = 1 << 20;

/// Threads a large copy runs on, the capture thread among them: a copy is bound by memory
/// bandwidth, which a few threads fill.
const COPY_THREADS: usize = 4;

/// How long a read waits for a drawing operation under way to end before it gives up on the
/// attempt: an operation is one request's drawing, well under a millisecond.
const DRAW_WAIT: Duration = Duration::from_millis(2);

/// A shared mapping, unmapped on drop.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(fd: &OwnedFd, len: usize, writable: bool) -> Result<Self, String> {
        let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // SAFETY: a fresh shared mapping of an fd the server handed over, checked below; nothing
        // else in this process holds or aliases it.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    /// The 32-bit word at byte `offset`, which the server writes concurrently.
    fn word(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.len);
        // SAFETY: in bounds and aligned (the mapping is page-aligned), and every access to the
        // shared word goes through the atomic.
        unsafe { AtomicU32::from_ptr(self.ptr.add(offset).cast()) }
    }

    /// The 64-bit word at byte `offset`, which the server writes concurrently.
    fn word64(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.len);
        // SAFETY: as `word`.
        unsafe { AtomicU64::from_ptr(self.ptr.add(offset).cast()) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// The reply to Watch: the three fds and the framebuffer's geometry.
struct WatchReply {
    framebuffer: OwnedFd,
    watch: OwnedFd,
    event: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    bpp: u32,
    pixel_offset: u32,
    tile: u32,
}

impl TryParseFd for WatchReply {
    fn try_parse_fd<'a>(
        value: &'a [u8],
        fds: &mut Vec<OwnedFd>,
    ) -> Result<(Self, &'a [u8]), ParseError> {
        if value.len() < 32 {
            return Err(ParseError::InsufficientData);
        }
        if value[1] != 3 || fds.len() < 3 {
            return Err(ParseError::MissingFileDescriptors);
        }
        let field = |i: usize| u32::from_ne_bytes(value[8 + 4 * i..12 + 4 * i].try_into().unwrap());
        let mut taken = fds.drain(..3);
        let (framebuffer, watch, event) = (
            taken.next().unwrap(),
            taken.next().unwrap(),
            taken.next().unwrap(),
        );
        drop(taken);
        Ok((
            Self {
                framebuffer,
                watch,
                event,
                width: field(0),
                height: field(1),
                stride: field(2),
                bpp: field(3),
                pixel_offset: field(4),
                tile: field(5),
            },
            &value[32..],
        ))
    }
}

/// A watch on the framebuffer of the screen of one root window (module docblock).
pub(crate) struct SharedFramebuffer {
    opcode: u8,
    id: u32,
    framebuffer: Mapping,
    watch: Mapping,
    event: OwnedFd,
    pub width: u32,
    pub height: u32,
    stride: usize,
    pixel_offset: usize,
    tile: u32,
    tiles_x: u32,
    tiles_y: u32,
}

/// What [`SharedFramebuffer::open`] found: a watch, or why there is none.
pub(crate) enum Opened {
    Watch(SharedFramebuffer),
    /// The server has no SELKIES-FRAMEBUFFER.
    Absent,
    /// The server has it and the watch is not usable, and why.
    Declined(String),
}

impl SharedFramebuffer {
    /// Watch the framebuffer of `root`'s screen, where the server offers one of 32 bits a pixel.
    pub(crate) fn open(conn: &RustConnection, root: u32) -> Opened {
        let opcode = match conn.extension_information(EXTENSION) {
            Ok(Some(info)) => info.major_opcode,
            Ok(None) => return Opened::Absent,
            Err(e) => return Opened::Declined(format!("{EXTENSION} query: {e}")),
        };
        match Self::watch(conn, opcode, root) {
            Ok(fb) => Opened::Watch(fb),
            Err(e) => Opened::Declined(e),
        }
    }

    fn watch(conn: &RustConnection, opcode: u8, root: u32) -> Result<Self, String> {
        let id = conn.generate_id().map_err(|e| format!("watch id: {e}"))?;
        let mut req = vec![opcode, 1];
        req.extend_from_slice(&3u16.to_ne_bytes());
        req.extend_from_slice(&id.to_ne_bytes());
        req.extend_from_slice(&root.to_ne_bytes());
        let reply = conn
            .send_request_with_reply_with_fds::<WatchReply>(&[IoSlice::new(&req)], Vec::new())
            .map_err(|e| format!("{EXTENSION} watch: {e}"))?
            .reply()
            .map_err(|e| match e {
                ReplyError::X11Error(x) if x.error_kind == ErrorKind::Match => {
                    "the screen is on the GPU".to_string()
                }
                e => format!("{EXTENSION} watch refused: {e}"),
            })?;
        if reply.bpp != 32 || reply.tile == 0 || (reply.stride as u64) < reply.width as u64 * 4 {
            return Err(format!(
                "{}x{} at {} bits a pixel, stride {}, tile {}",
                reply.width, reply.height, reply.bpp, reply.stride, reply.tile
            ));
        }
        // SAFETY: a query of the seals on an fd this function owns.
        let seals = unsafe { libc::fcntl(reply.framebuffer.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
            return Err("the framebuffer is not sealed against shrinking".into());
        }
        let pixel_offset = reply.pixel_offset as usize;
        let framebuffer = Mapping::new(
            &reply.framebuffer,
            pixel_offset + reply.stride as usize * reply.height as usize,
            false,
        )?;
        let tiles_x = reply.width.div_ceil(reply.tile);
        let tiles_y = reply.height.div_ceil(reply.tile);
        let words = (tiles_x as usize * tiles_y as usize).div_ceil(64);
        let watch = Mapping::new(&reply.watch, WATCH_BITS + words * 8, true)?;
        let fb = Self {
            opcode,
            id,
            framebuffer,
            watch,
            event: reply.event,
            width: reply.width,
            height: reply.height,
            stride: reply.stride as usize,
            pixel_offset,
            tile: reply.tile,
            tiles_x,
            tiles_y,
        };
        let head_ok = fb.framebuffer.word(0).load(Ordering::Relaxed) == MAGIC
            && fb.framebuffer.word(4).load(Ordering::Relaxed) == VERSION
            && fb.framebuffer.word(HEAD_WIDTH).load(Ordering::Relaxed) == reply.width
            && fb.framebuffer.word(HEAD_HEIGHT).load(Ordering::Relaxed) == reply.height
            && fb.framebuffer.word(HEAD_STRIDE).load(Ordering::Relaxed) == reply.stride
            && fb.framebuffer.word(HEAD_BPP).load(Ordering::Relaxed) == 32;
        let watch_ok = fb.watch.word(0).load(Ordering::Relaxed) == MAGIC
            && fb.watch.word(WATCH_TILE).load(Ordering::Relaxed) == reply.tile
            && fb.watch.word(WATCH_TILES_X).load(Ordering::Relaxed) == tiles_x
            && fb.watch.word(WATCH_TILES_Y).load(Ordering::Relaxed) == tiles_y;
        if !head_ok || !watch_ok {
            fb.close(conn);
            return Err("the framebuffer's head disagrees with the reply".into());
        }
        Ok(fb)
    }

    /// End the watch on the server; the mappings go with `self`.
    pub(crate) fn close(self, conn: &RustConnection) {
        let mut req = vec![self.opcode, 2];
        req.extend_from_slice(&2u16.to_ne_bytes());
        req.extend_from_slice(&self.id.to_ne_bytes());
        let _ = conn.send_request_without_reply(&[IoSlice::new(&req)], Vec::new());
    }

    /// Whether the screen has moved to another framebuffer (a resize): the watch is over.
    pub(crate) fn retired(&self) -> bool {
        self.framebuffer.word(HEAD_RETIRED).load(Ordering::Acquire) != 0
    }

    /// The words of the tile bitmap.
    fn words(&self) -> usize {
        (self.tiles_x as usize * self.tiles_y as usize).div_ceil(64)
    }

    /// Whether any tile was drawn into since the bitmap was last taken.
    pub(crate) fn drawn(&self) -> bool {
        (0..self.words()).any(|i| {
            self.watch
                .word64(WATCH_BITS + i * 8)
                .load(Ordering::Acquire)
                != 0
        })
    }

    /// Take the tiles drawn since the last take, handing each nonzero word of the bitmap to
    /// `into` with its index.
    fn take(&self, mut into: impl FnMut(usize, u64)) {
        for i in 0..self.words() {
            let bits = self
                .watch
                .word64(WATCH_BITS + i * 8)
                .swap(0, Ordering::AcqRel);
            if bits != 0 {
                into(i, bits);
            }
        }
    }

    /// Wait at most `timeout` for a drawing operation to end on a tile not yet taken, or for the
    /// framebuffer to retire: true where one did, or already had.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        self.watch.word(WATCH_ARMED).store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.drawn() || self.retired() {
            return true;
        }
        let mut poll = libc::pollfd {
            fd: self.event.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let deadline = libc::timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        // SAFETY: one pollfd for the duration of the call.
        let ready = unsafe { libc::ppoll(&mut poll, 1, &deadline, std::ptr::null()) } > 0;
        if ready {
            let mut count = 0u64;
            // SAFETY: an 8-byte read of the eventfd's counter into a u64; the fd is nonblocking.
            unsafe {
                libc::read(self.event.as_raw_fd(), (&mut count as *mut u64).cast(), 8);
            }
        }
        ready || self.drawn() || self.retired()
    }

    /// The drawing count once no operation runs, or `None` past `DRAW_WAIT`.
    fn settled_seq(&self) -> Option<u64> {
        let seq = self.framebuffer.word64(HEAD_SEQ);
        let start = Instant::now();
        loop {
            let s = seq.load(Ordering::Acquire);
            if s.is_multiple_of(2) {
                return Some(s);
            }
            if start.elapsed() > DRAW_WAIT {
                return None;
            }
            std::hint::spin_loop();
        }
    }

    /// Copy the tiles `pending` marks within the region at (`x`, `y`) of `width` x `height`
    /// into `dst` (`dst_stride` bytes a row, the region's top-left at its start). Each row of
    /// tiles lands in rows of `dst` of its own, so a change past `PARALLEL_BYTES` copies its
    /// rows of tiles on `COPY_THREADS` threads: one thread copying a whole 2560x1600 screen took
    /// 1.2 to 6.7 ms, reading what the server had just drawn from another core's cache, which
    /// held a full-screen change back behind an XShm grab, the server's copy from its own.
    fn copy_tiles(
        &self,
        pending: &[u64],
        dst: &mut [u8],
        dst_stride: usize,
        (x, y, width, height): (u32, u32, u32, u32),
    ) {
        let tile = self.tile;
        let tx0 = x / tile;
        let tx1 = (x + width).div_ceil(tile).min(self.tiles_x);
        let (ty0, ty1) = (y / tile, (y + height).div_ceil(tile).min(self.tiles_y));
        let (tiles_x, fb_width, stride) = (self.tiles_x as usize, self.width, self.stride);
        let set = |tx: u32, ty: u32| {
            let t = ty as usize * tiles_x + tx as usize;
            pending[t / 64] >> (t % 64) & 1 != 0
        };
        // SAFETY: the pixels are `stride * height` bytes past `pixel_offset` of a mapping that
        // long (`watch`); the server writes them concurrently, which the drawing count guards.
        let src = unsafe {
            std::slice::from_raw_parts(
                self.framebuffer.ptr.add(self.pixel_offset),
                self.stride * self.height as usize,
            )
        };
        let bottom_of = |ty: u32| ((ty + 1) * tile).min(y + height).min(self.height);
        let mut rows = Vec::with_capacity((ty1 - ty0) as usize);
        let mut rest = dst;
        let mut copied = 0usize;
        for ty in ty0..ty1 {
            let (top, bottom) = ((ty * tile).max(y), bottom_of(ty));
            let len = ((bottom - top) as usize * dst_stride).min(rest.len());
            let (band, tail) = std::mem::take(&mut rest).split_at_mut(len);
            rest = tail;
            let marked = (tx0..tx1).filter(|&tx| set(tx, ty)).count();
            copied += marked * (tile * (bottom - top)) as usize * 4;
            if marked > 0 {
                rows.push((ty, top, bottom, band));
            }
        }
        let copy_row = |(ty, top, bottom, band): (u32, u32, u32, &mut [u8])| {
            let mut tx = tx0;
            while tx < tx1 {
                if !set(tx, ty) {
                    tx += 1;
                    continue;
                }
                let run = tx;
                while tx < tx1 && set(tx, ty) {
                    tx += 1;
                }
                let left = (run * tile).max(x);
                let right = (tx * tile).min(x + width).min(fb_width);
                let bytes = (right - left) as usize * 4;
                for row in top..bottom {
                    let s = row as usize * stride + left as usize * 4;
                    let d = (row - top) as usize * dst_stride + (left - x) as usize * 4;
                    band[d..d + bytes].copy_from_slice(&src[s..s + bytes]);
                }
            }
        };
        if copied > PARALLEL_BYTES {
            let per = rows.len().div_ceil(COPY_THREADS);
            let mut parts: Vec<Vec<_>> = Vec::with_capacity(COPY_THREADS);
            let mut rows = rows.into_iter().peekable();
            while rows.peek().is_some() {
                parts.push(rows.by_ref().take(per).collect());
            }
            let first = parts.remove(0);
            std::thread::scope(|scope| {
                for part in parts {
                    scope.spawn(|| part.into_iter().for_each(copy_row));
                }
                first.into_iter().for_each(copy_row);
            });
        } else {
            rows.into_iter().for_each(copy_row);
        }
    }
}

/// The capture's watch, the tiles each pool surface is missing (module docblock), and those
/// drawn since the last frame said where it changed (`changed`).
pub(crate) struct FramebufferGrab {
    pub fb: SharedFramebuffer,
    pending: Vec<Vec<u64>>,
    drawn: Vec<u64>,
}

impl FramebufferGrab {
    /// A grab for `surfaces` pool surfaces, each missing the whole screen.
    pub(crate) fn new(fb: SharedFramebuffer, surfaces: usize) -> Self {
        let words = fb.words();
        Self {
            fb,
            pending: vec![vec![u64::MAX; words]; surfaces],
            drawn: vec![u64::MAX; words],
        }
    }

    /// Take the tiles drawn since the last take into every surface's and into `drawn`.
    fn take(&mut self) {
        let (pending, drawn) = (&mut self.pending, &mut self.drawn);
        self.fb.take(|i, bits| {
            for p in pending.iter_mut() {
                p[i] |= bits;
            }
            drawn[i] |= bits;
        });
    }

    /// Where the screen was drawn since the last call, within the region at (`x`, `y`) of
    /// `width` x `height`, in its coordinates: a rectangle a run of tiles along a row of them.
    pub(crate) fn changed(
        &mut self,
        (x, y, width, height): (u32, u32, u32, u32),
    ) -> Vec<smithay::utils::Rectangle<i32, smithay::utils::Physical>> {
        self.take();
        let tile = self.fb.tile;
        let tiles_x = self.fb.tiles_x as usize;
        let drawn = std::mem::replace(&mut self.drawn, vec![0; self.fb.words()]);
        let set = |tx: u32, ty: u32| {
            let t = ty as usize * tiles_x + tx as usize;
            drawn[t / 64] >> (t % 64) & 1 != 0
        };
        let (tx0, tx1) = (x / tile, (x + width).div_ceil(tile).min(self.fb.tiles_x));
        let (ty0, ty1) = (y / tile, (y + height).div_ceil(tile).min(self.fb.tiles_y));
        let mut rects = Vec::new();
        for ty in ty0..ty1 {
            let mut tx = tx0;
            while tx < tx1 {
                if !set(tx, ty) {
                    tx += 1;
                    continue;
                }
                let run = tx;
                while tx < tx1 && set(tx, ty) {
                    tx += 1;
                }
                let left = (run * tile).max(x);
                let right = (tx * tile).min(x + width);
                let top = (ty * tile).max(y);
                let bottom = ((ty + 1) * tile).min(y + height);
                rects.push(smithay::utils::Rectangle::new(
                    ((left - x) as i32, (top - y) as i32).into(),
                    ((right - left) as i32, (bottom - top) as i32).into(),
                ));
            }
        }
        rects
    }

    /// Mark the whole screen missing from every surface and drawn since the last frame (a
    /// region moved, the surfaces were recreated): the next frame differs from the one before
    /// wherever it lies, which no drawing reported.
    pub(crate) fn invalidate(&mut self) {
        for p in self.pending.iter_mut() {
            p.fill(u64::MAX);
        }
        self.drawn.fill(u64::MAX);
    }

    /// Mark the area at (`x`, `y`), `width` x `height`, in screen coordinates, missing from
    /// surface `idx`: an overlay drawn onto it.
    pub(crate) fn overlaid(&mut self, idx: usize, x: i32, y: i32, width: i32, height: i32) {
        let tile = self.fb.tile as i32;
        let (tiles_x, tiles_y) = (self.fb.tiles_x as i32, self.fb.tiles_y as i32);
        let tx0 = x.max(0) / tile;
        let ty0 = y.max(0) / tile;
        let tx1 = ((x + width).max(0) as u32)
            .div_ceil(tile as u32)
            .min(tiles_x as u32) as i32;
        let ty1 = ((y + height).max(0) as u32)
            .div_ceil(tile as u32)
            .min(tiles_y as u32) as i32;
        let pending = &mut self.pending[idx];
        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                let t = (ty * tiles_x + tx) as usize;
                pending[t / 64] |= 1 << (t % 64);
            }
        }
    }

    /// Bring surface `idx` (`dst`, `dst_stride` bytes a row) up to the screen over the region at
    /// (`x`, `y`) of `width` x `height`: false where no whole read came in `READ_ATTEMPTS`, or
    /// the region runs past the framebuffer (a resize the capture has not followed yet), the
    /// surface left to an XShm grab, which [`Self::grabbed`] then records.
    pub(crate) fn read(
        &mut self,
        idx: usize,
        dst: &mut [u8],
        dst_stride: usize,
        region: (u32, u32, u32, u32),
    ) -> bool {
        let (x, y, width, height) = region;
        if x + width > self.fb.width || y + height > self.fb.height {
            return false;
        }
        self.take();
        for _ in 0..READ_ATTEMPTS {
            let Some(before) = self.fb.settled_seq() else {
                continue;
            };
            self.fb
                .copy_tiles(&self.pending[idx], dst, dst_stride, region);
            fence(Ordering::Acquire);
            if self.fb.framebuffer.word64(HEAD_SEQ).load(Ordering::Acquire) == before {
                self.pending[idx].fill(0);
                return true;
            }
            self.take();
        }
        false
    }

    /// Surface `idx` was grabbed whole through XShm: it misses nothing.
    pub(crate) fn grabbed(&mut self, idx: usize) {
        self.take();
        self.pending[idx].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A framebuffer and watch in anonymous memory, `width` x `height` with `tile`-pixel tiles,
    /// the pixels each their own index.
    fn fake(width: u32, height: u32, tile: u32) -> SharedFramebuffer {
        let anon = |len: usize| {
            // SAFETY: a memfd of this process, sized before it is mapped.
            let fd = unsafe {
                let fd = libc::memfd_create(c"test".as_ptr(), 0);
                assert!(fd >= 0);
                assert_eq!(libc::ftruncate(fd, len as i64), 0);
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
            };
            let map = Mapping::new(&fd, len, true).unwrap();
            (fd, map)
        };
        let stride = width as usize * 4;
        let (_, framebuffer) = anon(4096 + stride * height as usize);
        let tiles_x = width.div_ceil(tile);
        let tiles_y = height.div_ceil(tile);
        let (_, watch) = anon(WATCH_BITS + (tiles_x * tiles_y).div_ceil(64) as usize * 8);
        let (event, _) = anon(8);
        for i in 0..(width * height) as usize {
            framebuffer
                .word(4096 + i * 4)
                .store(i as u32, Ordering::Relaxed);
        }
        SharedFramebuffer {
            opcode: 0,
            id: 0,
            framebuffer,
            watch,
            event,
            width,
            height,
            stride,
            pixel_offset: 4096,
            tile,
            tiles_x,
            tiles_y,
        }
    }

    fn pixel(dst: &[u8], stride: usize, x: usize, y: usize) -> u32 {
        u32::from_ne_bytes(dst[y * stride + x * 4..][..4].try_into().unwrap())
    }

    #[test]
    fn a_surface_takes_the_tiles_it_misses_within_its_region() {
        let mut grab = FramebufferGrab::new(fake(64, 48, 16), 2);
        let (w, h) = (40usize, 30usize);
        let mut dst = vec![0u8; w * h * 4];
        assert!(grab.read(0, &mut dst, w * 4, (8, 10, w as u32, h as u32)));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(pixel(&dst, w * 4, x, y), ((y + 10) * 64 + x + 8) as u32);
            }
        }
        dst.fill(0);
        grab.fb
            .watch
            .word64(WATCH_BITS)
            .store(1 << 5, Ordering::Relaxed);
        assert!(grab.read(0, &mut dst, w * 4, (8, 10, w as u32, h as u32)));
        let copied = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(&dst, w * 4, x, y) != 0)
            .count();
        assert_eq!(
            copied,
            16 * 16,
            "tile 5, x 16..32 and y 16..32, within the region"
        );
        let mut other = vec![0u8; w * h * 4];
        assert!(grab.read(1, &mut other, w * 4, (8, 10, w as u32, h as u32)));
        assert_eq!(
            pixel(&other, w * 4, 0, 0),
            (10 * 64 + 8) as u32,
            "a surface that held nothing yet takes the whole region"
        );
    }

    #[test]
    fn a_read_an_operation_runs_across_is_not_taken() {
        let mut grab = FramebufferGrab::new(fake(32, 32, 16), 1);
        grab.fb
            .framebuffer
            .word64(HEAD_SEQ)
            .store(1, Ordering::Relaxed);
        let mut dst = vec![0u8; 32 * 32 * 4];
        assert!(
            !grab.read(0, &mut dst, 32 * 4, (0, 0, 32, 32)),
            "a drawing operation that never ends leaves the frame to XShm"
        );
        grab.grabbed(0);
        assert!(grab.pending[0].iter().all(|&w| w == 0));
    }

    #[test]
    fn a_frame_says_where_the_screen_was_drawn_within_its_region() {
        let mut grab = FramebufferGrab::new(fake(64, 64, 16), 1);
        assert_eq!(
            grab.changed((8, 8, 48, 48)).len(),
            4,
            "a first frame changed whole, a run of tiles a row"
        );
        grab.fb
            .watch
            .word64(WATCH_BITS)
            .store(0b0110 << 4, Ordering::Relaxed);
        let rects = grab.changed((8, 8, 48, 48));
        assert_eq!(
            rects,
            vec![smithay::utils::Rectangle::new(
                (8, 8).into(),
                (32, 16).into()
            )],
            "tiles (1,1) and (2,1), at 16..48 by 16..32, in the region's coordinates"
        );
        assert!(grab.changed((8, 8, 48, 48)).is_empty());
    }

    #[test]
    fn an_overlay_marks_its_tiles_missing() {
        let mut grab = FramebufferGrab::new(fake(64, 64, 16), 1);
        grab.grabbed(0);
        grab.overlaid(0, 20, 30, 10, 10);
        let set: Vec<usize> = (0..16)
            .filter(|&t| grab.pending[0][0] >> t & 1 != 0)
            .collect();
        assert_eq!(set, vec![5, 9], "tiles (1,1) and (1,2)");
    }
}
