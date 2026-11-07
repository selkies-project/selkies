/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a dmabuf is, beyond the fd number that names it, for the hardware sessions that keep
//! an import of each recurring capture buffer (NVENC's CUDA registrations, VA-API's surfaces).

/// The stable identity of a dmabuf, so a cache keyed by the raw fd number cannot hand back a
/// stale import after that fd was closed and recycled onto a different buffer.
///
/// The fd integer alone is not an identity: the host's slot renegotiation frees the backing buffer
/// objects and the kernel reissues the same small fd numbers for the new ones. Since Linux 5.3 each
/// dma-buf carries its own inode, so `(st_dev, st_ino)` distinguishes two buffers that reuse one fd
/// number, and `size` reports the true allocation; the DRM format modifier and the geometry round
/// out the identity for older kernels where the inode is shared. A cache hit requires every field to
/// match, so a recycled fd whose buffer differs in any of them is re-imported instead of reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DmaBufIdentity {
    dev: u64,
    ino: u64,
    size: i64,
    modifier: u64,
    width: u32,
    height: u32,
}

impl DmaBufIdentity {
    /// Read the identity of the buffer behind `fd`: `fstat` supplies the inode and allocation
    /// size, and the caller supplies the modifier and geometry from the dmabuf descriptor. A failed
    /// `fstat` leaves the inode/size zero, which still combines with the modifier and geometry.
    pub(crate) fn probe(fd: i32, modifier: u64, width: u32, height: u32) -> Self {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let (dev, ino, size) = if unsafe { libc::fstat(fd, &mut st) } == 0 {
            (st.st_dev as u64, st.st_ino as u64, st.st_size as i64)
        } else {
            (0, 0, 0)
        };
        Self {
            dev,
            ino,
            size,
            modifier,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two buffers that reuse one fd number but differ in any identity field are distinct, so a
    /// cache keyed by fd cannot return a stale import: a new inode (the kernel reissues one per
    /// dma-buf since 5.3), a new size, or new geometry each breaks the match.
    #[test]
    fn dmabuf_identity_distinguishes_recycled_fd() {
        let base = DmaBufIdentity {
            dev: 1,
            ino: 10,
            size: 100,
            modifier: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(base, base);
        let mut new_ino = base;
        new_ino.ino = 11;
        assert_ne!(base, new_ino);
        let mut new_size = base;
        new_size.size = 200;
        assert_ne!(base, new_size);
        let mut new_mod = base;
        new_mod.modifier = 1;
        assert_ne!(base, new_mod);
        let mut new_geom = base;
        new_geom.width = 1280;
        assert_ne!(base, new_geom);
    }

    /// `probe` reads a real fd deterministically and folds the modifier and geometry into the
    /// identity: the same fd and parameters yield equal identities, and changing the modifier or the
    /// geometry changes the identity even on the same fd.
    #[test]
    fn dmabuf_identity_probe_is_stable_and_parameterized() {
        use std::os::fd::AsRawFd;
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = f.as_raw_fd();
        let a = DmaBufIdentity::probe(fd, 0x1234, 1920, 1080);
        assert_eq!(a, DmaBufIdentity::probe(fd, 0x1234, 1920, 1080));
        assert_ne!(a, DmaBufIdentity::probe(fd, 0x9999, 1920, 1080));
        assert_ne!(a, DmaBufIdentity::probe(fd, 0x1234, 1280, 720));
    }
}
