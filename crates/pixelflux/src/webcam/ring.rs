//! Shared-memory frame ring consumed by the Selkies V4L2 interposer.
//!
//! The ring is an anonymous memfd mapped once by this writer and once, read-only, by every
//! interposer client (it receives the fd over the control socket). Page 0 holds the header and the
//! per-slot control blocks; frame bytes start at [`DATA_OFFSET`]. Every constant and offset below is
//! mirrored by `addons/v4l2-interposer/v4l2_interposer.c`; the interposer refuses a connection whose
//! `version` differs.
//!
//! Publishing is a single-writer seqlock per slot: the slot's `seq` is bumped to odd, the frame is
//! written, `seq` is bumped to even, and only then does the header advertise the slot as the latest.
//! Readers copy the newest slot and retry if `seq` changed or was odd, so no cross-process lock exists
//! and a stalled reader never blocks the writer.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use memmap2::MmapMut;

pub const SHM_MAGIC: u32 = 0x434B_5753;
pub const SHM_VERSION: u32 = 1;
pub const CTRL_OFFSET: u32 = 128;
pub const CTRL_STRIDE: u32 = 64;
pub const DATA_OFFSET: u32 = 4096;
pub const MAX_SLOTS: u32 = 4;
pub const MIN_SLOTS: u32 = 2;
/// Byte size of the on-connect configuration struct (`webcam_config_t`).
pub const CONFIG_SIZE: usize = 64;

const HDR_LATEST_SLOT: usize = 48;
const HDR_LATEST_FRAME_SEQ: usize = 56;

/// V4L2 fourcc codes the ring can advertise.
pub const V4L2_PIX_FMT_YUV420: u32 = fourcc(b"YU12");
pub const V4L2_PIX_FMT_NV12: u32 = fourcc(b"NV12");
pub const V4L2_PIX_FMT_YUYV: u32 = fourcc(b"YUYV");
pub const V4L2_PIX_FMT_MJPEG: u32 = fourcc(b"MJPG");

pub const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// Geometry of the advertised device: a fixed-function camera with one format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Bytes per line of the first plane; 0 for compressed formats.
    pub bytesperline: u32,
    /// Maximum bytes of one frame, also the slot size.
    pub sizeimage: u32,
}

impl RingFormat {
    /// Build the format for a raw fourcc at `width` x `height`; `None` for an unknown fourcc.
    pub fn raw(
        fourcc_code: u32,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Option<Self> {
        let (bytesperline, sizeimage) = match fourcc_code {
            V4L2_PIX_FMT_YUV420 | V4L2_PIX_FMT_NV12 => (
                width,
                width * height + 2 * (width.div_ceil(2) * height.div_ceil(2)),
            ),
            V4L2_PIX_FMT_YUYV => (width * 2, width * 2 * height),
            _ => return None,
        };
        Some(RingFormat {
            width,
            height,
            fourcc: fourcc_code,
            fps_num,
            fps_den,
            bytesperline,
            sizeimage,
        })
    }

    /// Build the format for the compressed fourcc (MJPEG): no stride, and a frame budget of two
    /// bytes per pixel, what UVC cameras advertise for MJPEG and far above what a browser's JPEG
    /// of a camera picture takes.
    pub fn compressed(
        fourcc_code: u32,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Option<Self> {
        if fourcc_code != V4L2_PIX_FMT_MJPEG {
            return None;
        }
        Some(RingFormat {
            width,
            height,
            fourcc: fourcc_code,
            fps_num,
            fps_den,
            bytesperline: 0,
            sizeimage: width * height * 2,
        })
    }

    /// Raw or compressed, by fourcc.
    pub fn for_fourcc(
        fourcc_code: u32,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Option<Self> {
        Self::raw(fourcc_code, width, height, fps_num, fps_den)
            .or_else(|| Self::compressed(fourcc_code, width, height, fps_num, fps_den))
    }
}

/// Writer side of the ring. Not `Sync`: one thread publishes.
pub struct Ring {
    fd: OwnedFd,
    map: MmapMut,
    format: RingFormat,
    n_slots: u32,
    slot_size: usize,
    next_slot: u32,
    slot_seq: Vec<u32>,
    frame_seq: u64,
    last_published: Option<(usize, usize)>,
}

impl Ring {
    /// Allocate and zero-initialize the memfd and write the header.
    pub fn new(format: RingFormat, n_slots: u32) -> io::Result<Self> {
        let n_slots = n_slots.clamp(MIN_SLOTS, MAX_SLOTS);
        let slot_size = page_align(format.sizeimage.max(1) as usize);
        let total = DATA_OFFSET as usize + n_slots as usize * slot_size;
        let name = c"selkies-webcam-staging";
        let raw: RawFd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), total as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let map = unsafe { MmapMut::map_mut(&fd)? };
        // Sealed once this writer's own mapping exists, so the read-only contract each peer
        // receives the descriptor under is the kernel's: no peer maps it writable or resizes it
        // (future-write sealing needs Linux 5.1; an older kernel keeps the size seals alone).
        let size_seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe {
            libc::fcntl(
                fd.as_raw_fd(),
                libc::F_ADD_SEALS,
                size_seals | libc::F_SEAL_FUTURE_WRITE,
            )
        } != 0
        {
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, size_seals) };
        }
        let mut ring = Ring {
            fd,
            map,
            format,
            n_slots,
            slot_size,
            next_slot: 0,
            slot_seq: vec![0; n_slots as usize],
            frame_seq: 0,
            last_published: None,
        };
        ring.write_header();
        Ok(ring)
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn format(&self) -> &RingFormat {
        &self.format
    }

    pub fn n_slots(&self) -> u32 {
        self.n_slots
    }

    pub fn slot_size(&self) -> usize {
        self.slot_size
    }

    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    /// The bytes of the most recently published frame, valid until the next `publish`.
    pub fn latest_frame(&self) -> Option<&[u8]> {
        let (slot, used) = self.last_published?;
        let data_off = DATA_OFFSET as usize + slot * self.slot_size;
        Some(&self.map[data_off..data_off + used])
    }

    /// The configuration struct handed to each interposer client once, ahead of the doorbells.
    pub fn config_bytes(&self) -> [u8; CONFIG_SIZE] {
        let f = &self.format;
        let words = [
            SHM_MAGIC,
            SHM_VERSION,
            f.width,
            f.height,
            f.fourcc,
            f.fps_num,
            f.fps_den,
            self.n_slots,
            self.slot_size as u32,
            DATA_OFFSET,
            CTRL_OFFSET,
            CTRL_STRIDE,
            f.bytesperline,
            f.sizeimage,
        ];
        let mut out = [0u8; CONFIG_SIZE];
        for (i, w) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn write_header(&mut self) {
        let f = self.format;
        let words = [
            SHM_MAGIC,
            SHM_VERSION,
            f.width,
            f.height,
            f.fourcc,
            f.fps_num,
            f.fps_den,
            self.n_slots,
            self.slot_size as u32,
            DATA_OFFSET,
            f.bytesperline,
            f.sizeimage,
            0,
            0,
        ];
        for (i, w) in words.iter().enumerate() {
            self.map[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        self.map[HDR_LATEST_FRAME_SEQ..HDR_LATEST_FRAME_SEQ + 8]
            .copy_from_slice(&0u64.to_le_bytes());
    }

    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        unsafe { &*(self.map.as_ptr().add(offset) as *const AtomicU32) }
    }

    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        unsafe { &*(self.map.as_ptr().add(offset) as *const AtomicU64) }
    }

    /// Publish one frame: `fill` writes the frame into the next slot and returns the byte count.
    /// A `fill` returning 0 (or more than the slot holds) abandons the slot without publishing it.
    pub fn publish<F: FnOnce(&mut [u8]) -> usize>(&mut self, ts_ns: u64, fill: F) -> bool {
        let slot = self.next_slot as usize;
        let ctrl = CTRL_OFFSET as usize + slot * CTRL_STRIDE as usize;
        let data_off = DATA_OFFSET as usize + slot * self.slot_size;
        let seq = self.slot_seq[slot];

        self.atomic_u32(ctrl)
            .store(seq.wrapping_add(1), Ordering::Release);
        let slot_size = self.slot_size;
        let used = fill(&mut self.map[data_off..data_off + slot_size]);
        if used == 0 || used > slot_size {
            self.atomic_u32(ctrl)
                .store(seq.wrapping_add(2), Ordering::Release);
            self.slot_seq[slot] = seq.wrapping_add(2);
            return false;
        }
        self.frame_seq += 1;
        self.atomic_u32(ctrl + 4)
            .store(used as u32, Ordering::Relaxed);
        self.atomic_u64(ctrl + 8)
            .store(self.frame_seq, Ordering::Relaxed);
        self.atomic_u64(ctrl + 16).store(ts_ns, Ordering::Relaxed);
        self.atomic_u32(ctrl)
            .store(seq.wrapping_add(2), Ordering::Release);
        self.slot_seq[slot] = seq.wrapping_add(2);

        self.atomic_u32(HDR_LATEST_SLOT)
            .store(slot as u32, Ordering::Release);
        self.atomic_u64(HDR_LATEST_FRAME_SEQ)
            .store(self.frame_seq, Ordering::Release);
        self.last_published = Some((slot, used));
        self.next_slot = (self.next_slot + 1) % self.n_slots;
        true
    }

    /// Read back a slot the way a client does, for tests: (bytesused, frame_seq, bytes).
    #[cfg(test)]
    pub fn read_latest(&self) -> Option<(u32, u64, Vec<u8>)> {
        let fseq = self
            .atomic_u64(HDR_LATEST_FRAME_SEQ)
            .load(Ordering::Acquire);
        if fseq == 0 {
            return None;
        }
        let slot = self.atomic_u32(HDR_LATEST_SLOT).load(Ordering::Acquire) as usize;
        let ctrl = CTRL_OFFSET as usize + slot * CTRL_STRIDE as usize;
        let s1 = self.atomic_u32(ctrl).load(Ordering::Acquire);
        let used = self.atomic_u32(ctrl + 4).load(Ordering::Relaxed) as usize;
        let cseq = self.atomic_u64(ctrl + 8).load(Ordering::Relaxed);
        let data_off = DATA_OFFSET as usize + slot * self.slot_size;
        let bytes = self.map[data_off..data_off + used].to_vec();
        let s2 = self.atomic_u32(ctrl).load(Ordering::Acquire);
        if s1 != s2 || s1 & 1 == 1 {
            return None;
        }
        Some((used as u32, cseq, bytes))
    }
}

pub fn page_align(n: usize) -> usize {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as usize } else { 4096 };
    n.div_ceil(page) * page
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that receives the descriptor maps it read-only and nothing else: a writable map
    /// or a resize is the kernel's to refuse, while this writer's own mapping keeps working.
    #[test]
    fn peers_cannot_write_or_resize_the_ring() {
        let f = RingFormat::raw(V4L2_PIX_FMT_YUV420, 64, 48, 30, 1).unwrap();
        let mut ring = Ring::new(f, 3).unwrap();
        let fd = ring.fd();
        let len = DATA_OFFSET as usize + ring.n_slots() as usize * ring.slot_size();
        let map =
            |prot| unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0) };
        let ro = map(libc::PROT_READ);
        assert_ne!(ro, libc::MAP_FAILED, "a read-only map is what peers get");
        unsafe { libc::munmap(ro, len) };
        let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        assert!(
            seals & libc::F_SEAL_SHRINK != 0 && seals & libc::F_SEAL_GROW != 0,
            "seals {seals:#x}"
        );
        assert_ne!(
            unsafe { libc::ftruncate(fd, 4096) },
            0,
            "the size is sealed"
        );
        if seals & libc::F_SEAL_FUTURE_WRITE != 0 {
            assert_eq!(
                map(libc::PROT_READ | libc::PROT_WRITE),
                libc::MAP_FAILED,
                "no writable map"
            );
        }
        assert!(ring.publish(1, |dst| {
            dst[0] = 7;
            1
        }));
        assert_eq!(
            ring.latest_frame().unwrap()[0],
            7,
            "the writer's own mapping still writes"
        );
    }

    #[test]
    fn header_and_config_layout() {
        let f = RingFormat::raw(V4L2_PIX_FMT_YUV420, 640, 480, 30, 1).unwrap();
        assert_eq!(f.bytesperline, 640);
        assert_eq!(f.sizeimage, 640 * 480 * 3 / 2);
        let ring = Ring::new(f, 3).unwrap();
        let cfg = ring.config_bytes();
        let w = |i: usize| u32::from_le_bytes(cfg[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(w(0), SHM_MAGIC);
        assert_eq!(w(1), SHM_VERSION);
        assert_eq!((w(2), w(3)), (640, 480));
        assert_eq!(w(4), V4L2_PIX_FMT_YUV420);
        assert_eq!((w(5), w(6)), (30, 1));
        assert_eq!(w(7), 3);
        assert_eq!(w(8) as usize, page_align(f.sizeimage as usize));
        assert_eq!(
            (w(9), w(10), w(11)),
            (DATA_OFFSET, CTRL_OFFSET, CTRL_STRIDE)
        );
        assert_eq!((w(12), w(13)), (f.bytesperline, f.sizeimage));
        let hdr = |i: usize| u32::from_le_bytes(ring.map[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!((hdr(10), hdr(11)), (640, f.sizeimage));
        assert_eq!(hdr(12), 0);
        assert!(ring.read_latest().is_none());
    }

    #[test]
    fn publish_cycles_slots_and_is_readable() {
        let f = RingFormat::raw(V4L2_PIX_FMT_YUYV, 16, 8, 30, 1).unwrap();
        let mut ring = Ring::new(f, 2).unwrap();
        for i in 1..=5u8 {
            assert!(ring.publish(i as u64 * 1000, |slot| {
                slot[..f.sizeimage as usize].fill(i);
                f.sizeimage as usize
            }));
            let (used, seq, bytes) = ring.read_latest().unwrap();
            assert_eq!(used, f.sizeimage);
            assert_eq!(seq, i as u64);
            assert!(bytes.iter().all(|b| *b == i));
        }
        assert_eq!(ring.frame_seq(), 5);
        assert!(!ring.publish(0, |_| 0));
        assert_eq!(ring.frame_seq(), 5);
    }

    #[test]
    fn slot_count_is_clamped() {
        let f = RingFormat::raw(V4L2_PIX_FMT_NV12, 32, 32, 15, 1).unwrap();
        assert_eq!(Ring::new(f, 1).unwrap().n_slots(), MIN_SLOTS);
        assert_eq!(Ring::new(f, 99).unwrap().n_slots(), MAX_SLOTS);
    }
}
