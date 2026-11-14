/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The page-locked rows of the client buffers a hardware session uploads from, and when each
//! registration ends.
//!
//! A registration names addresses rather than the memory behind them, and it outlives the
//! mapping it was made for: a pool mapped later over the same addresses is read from the old
//! pool's pages, by every session on the device's CUDA context. smithay unmaps a client's pool
//! when the client resizes it, or once the pool and every buffer made from it are gone, and
//! tells the encoder neither, so a client's rows are never registered where smithay maps them.
//! Each buffer gets a mapping of its own over the same pages, which only this table unmaps and
//! only after unregistering it, and its rows are uploaded from there. No other mapping can land
//! on those addresses while the registration stands, whatever becomes of the pool.
//!
//! A buffer is registered the second time it is presented, so a client that makes a new buffer
//! for every frame uploads unpinned instead of registering each one. The table is checked on
//! every upload from host memory, so a registration ends at the first such upload after its
//! buffer is destroyed or its rows arrive at another address after a resize, once
//! [`IDLE_UPLOADS`] uploads pass without the buffer, or when the session lets go of every
//! registration ([`ClientPins::release`]). A session that stops uploading from host memory, idle
//! or encoding dmabufs, keeps those pages locked until then, which holds memory but is never
//! misread, since nothing else can be mapped there.

use std::any::Any;
use std::sync::{Arc, Weak};

use libc::c_void;

/// The buffer a client's rows are read through: a wl_buffer's data, which tells one buffer
/// apart from another at the same address and lives as long as the buffer does.
pub type RowsOwner = Arc<dyn Any + Send + Sync>;

/// Uploads a registration's buffer may go unpresented before the registration ends: more than
/// the three buffers a client cycles through, and few enough that a buffer the client stopped
/// presenting does not keep its pages locked for long while the session uploads.
const IDLE_UPLOADS: u64 = 8;

/// The CUDA calls a registration takes, under the session's pushed context.
pub(crate) trait HostRegister {
    /// Page-lock `len` bytes at `base`, answering whether it took.
    fn register(&self, base: usize, len: usize) -> bool;
    /// End the registration made at `base`.
    fn unregister(&self, base: usize);
}

/// Where a buffer's rows are uploaded from.
enum Pages {
    /// Presented once, uploaded where smithay maps it.
    Seen,
    /// Page-locked at a mapping of their own: `len` bytes at `at`, the rows `offset` bytes in.
    Locked {
        at: usize,
        len: usize,
        offset: usize,
    },
    /// Locking failed, which is not retried.
    Unlocked,
}

/// One client buffer's rows, `len` bytes smithay maps at `base`.
struct ClientRows {
    base: usize,
    len: usize,
    owner: Weak<dyn Any + Send + Sync>,
    /// The upload that last presented the buffer.
    claimed: u64,
    pages: Pages,
}

impl ClientRows {
    fn is(&self, owner: &RowsOwner) -> bool {
        std::ptr::addr_eq(self.owner.as_ptr(), Arc::as_ptr(owner))
    }

    fn end(self, cuda: &impl HostRegister) {
        if let Pages::Locked { at, len, .. } = self.pages {
            cuda.unregister(at);
            // SAFETY: the mapping `lock` made, which nothing reads once it is unregistered.
            unsafe { libc::munmap(at as *mut c_void, len) };
        }
    }
}

/// Map the pages under `len` bytes at `base` a second time and page-lock that mapping.
fn lock(base: usize, len: usize, cuda: &impl HostRegister) -> Pages {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let start = base & !(page - 1);
    let span = (base + len).next_multiple_of(page) - start;
    // SAFETY: the pages lie within the client's shared pool mapping, which a zero old size
    // leaves in place while a new mapping of the same pages is made elsewhere.
    let at = unsafe { libc::mremap(start as *mut c_void, 0, span, libc::MREMAP_MAYMOVE) };
    if at == libc::MAP_FAILED {
        return Pages::Unlocked;
    }
    if !cuda.register(at as usize, span) {
        // SAFETY: the mapping just made, which nothing else knows of.
        unsafe { libc::munmap(at, span) };
        return Pages::Unlocked;
    }
    Pages::Locked {
        at: at as usize,
        len: span,
        offset: base - start,
    }
}

/// The client rows one session holds registered, and the uploads it has made.
#[derive(Default)]
pub(crate) struct ClientPins {
    rows: Vec<ClientRows>,
    uploads: u64,
}

impl ClientPins {
    /// The address to upload the `len` bytes smithay maps at `base`, read through `owner`, from:
    /// their page-locked mapping where the buffer was presented before, else `base` itself. The
    /// address stays valid until the next call on this table. A page-locked mapping lies outside
    /// smithay's SIGBUS guard and is for the device's upload only: it must never be read on the
    /// CPU, where a pool the client shrank would raise a SIGBUS that nothing turns into an error.
    pub(crate) fn claim(
        &mut self,
        base: usize,
        len: usize,
        owner: &RowsOwner,
        cuda: &impl HostRegister,
    ) -> usize {
        self.uploads += 1;
        let now = self.uploads;
        let mut held = None;
        for r in std::mem::take(&mut self.rows) {
            let same = r.is(owner);
            if same && r.base == base && r.len == len {
                held = Some(r);
            } else if same || r.owner.strong_count() == 0 || now - r.claimed >= IDLE_UPLOADS {
                r.end(cuda);
            } else {
                self.rows.push(r);
            }
        }
        let rows = match held {
            Some(mut r) => {
                if matches!(r.pages, Pages::Seen) {
                    r.pages = lock(base, len, cuda);
                }
                r.claimed = now;
                r
            }
            None => ClientRows {
                base,
                len,
                owner: Arc::downgrade(owner),
                claimed: now,
                pages: Pages::Seen,
            },
        };
        let from = match rows.pages {
            Pages::Locked { at, offset, .. } => at + offset,
            Pages::Seen | Pages::Unlocked => base,
        };
        self.rows.push(rows);
        from
    }

    /// Count an upload no client buffer took part in, such as the session's own pool's, so a
    /// client whose window is composed rather than encoded in place has its rows let go too.
    pub(crate) fn age(&mut self, cuda: &impl HostRegister) {
        if self.rows.is_empty() {
            return;
        }
        self.uploads += 1;
        let now = self.uploads;
        for r in std::mem::take(&mut self.rows) {
            if r.owner.strong_count() == 0 || now - r.claimed >= IDLE_UPLOADS {
                r.end(cuda);
            } else {
                self.rows.push(r);
            }
        }
    }

    /// End every registration.
    pub(crate) fn release(&mut self, cuda: &impl HostRegister) {
        for r in self.rows.drain(..) {
            r.end(cuda);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The base of every page-locked mapping, the buffer presented longest ago first.
    #[cfg(test)]
    pub(crate) fn locked(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|r| match r.pages {
                Pages::Locked { at, .. } => Some(at),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A buffer stand-in, told apart by its allocation as a wl_buffer's data is.
    fn owner() -> RowsOwner {
        Arc::new(())
    }

    /// Every register and unregister the table makes, in order, each locking where `takes`.
    struct Calls {
        registered: RefCell<Vec<usize>>,
        unregistered: RefCell<Vec<usize>>,
        takes: bool,
    }

    impl Calls {
        fn new() -> Self {
            Calls {
                registered: RefCell::default(),
                unregistered: RefCell::default(),
                takes: true,
            }
        }
    }

    impl HostRegister for Calls {
        fn register(&self, base: usize, _len: usize) -> bool {
            self.registered.borrow_mut().push(base);
            self.takes
        }

        fn unregister(&self, base: usize) {
            self.unregistered.borrow_mut().push(base);
        }
    }

    /// A memfd pool of `len` bytes filled with `fill`, mapped shared as smithay maps one, in
    /// place of the test's own mapping at `at` where it is given. `MAP_FIXED` replaces that
    /// mapping in one step: unmapping it first would leave a hole another test's thread could
    /// map into, and the fixed mapping would then replace that thread's memory.
    fn pool(len: usize, fill: u8, at: Option<usize>) -> usize {
        let fd = crate::memfd_create(c"client-pool", libc::MFD_CLOEXEC);
        assert!(fd >= 0, "memfd_create");
        // SAFETY: a fresh memfd sized to `len` and mapped whole, at `at` only over a mapping
        // the test made there itself.
        unsafe {
            assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
            let fixed = if at.is_some() { libc::MAP_FIXED } else { 0 };
            let p = libc::mmap(
                at.unwrap_or(0) as *mut c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | fixed,
                fd,
                0,
            );
            libc::close(fd);
            assert_ne!(p, libc::MAP_FAILED, "mmap");
            std::ptr::write_bytes(p as *mut u8, fill, len);
            p as usize
        }
    }

    fn byte(at: usize) -> u8 {
        // SAFETY: an address inside a mapping the test holds.
        unsafe { *(at as *const u8) }
    }

    const LEN: usize = 0x3000;

    #[test]
    fn a_buffer_is_registered_once_from_its_second_frame_and_uploaded_from_there() {
        let (cuda, at) = (Calls::new(), pool(LEN, 7, None));
        let mut pins = ClientPins::default();
        let a = owner();
        assert_eq!(pins.claim(at + 0x10, 0x100, &a, &cuda), at + 0x10);
        let from = pins.claim(at + 0x10, 0x100, &a, &cuda);
        assert_ne!(
            from,
            at + 0x10,
            "the rows are uploaded from a mapping of their own"
        );
        for _ in 0..20 {
            assert_eq!(pins.claim(at + 0x10, 0x100, &a, &cuda), from);
        }
        assert_eq!(*cuda.registered.borrow(), [from - 0x10]);
        // SAFETY: the pool mapped above, which the table reads no more.
        unsafe { *((at + 0x10) as *mut u8) = 9 };
        assert_eq!(
            byte(from),
            9,
            "what the client paints shows where it is uploaded from"
        );
        pins.release(&cuda);
        assert_eq!(*cuda.unregistered.borrow(), [from - 0x10]);
    }

    #[test]
    fn the_buffers_of_one_pool_a_client_alternates_between_are_each_registered_once() {
        let (cuda, at) = (Calls::new(), pool(LEN, 0, None));
        let mut pins = ClientPins::default();
        let buffers = [owner(), owner(), owner()];
        for i in 0..60 {
            pins.claim(at + (i % 3) * 0x1000, 0x1000, &buffers[i % 3], &cuda);
        }
        assert_eq!(cuda.registered.borrow().len(), 3);
        assert!(cuda.unregistered.borrow().is_empty());
        pins.release(&cuda);
    }

    #[test]
    fn a_client_that_makes_a_new_buffer_for_every_frame_never_registers_one() {
        let (cuda, at) = (Calls::new(), pool(LEN, 0, None));
        let mut pins = ClientPins::default();
        for _ in 0..20 {
            assert_eq!(pins.claim(at, 0x1000, &owner(), &cuda), at);
        }
        assert!(cuda.registered.borrow().is_empty());
    }

    #[test]
    fn a_pool_mapped_where_another_lay_is_uploaded_from_its_own_pages() {
        let cuda = Calls::new();
        let at = pool(LEN, 1, None);
        let mut pins = ClientPins::default();
        let (old, new) = (owner(), owner());
        pins.claim(at, LEN, &old, &cuda);
        let stale = pins.claim(at, LEN, &old, &cuda);
        assert_eq!(pool(LEN, 2, Some(at)), at);
        pins.claim(at, LEN, &new, &cuda);
        let from = pins.claim(at, LEN, &new, &cuda);
        assert_eq!(byte(from), 2);
        assert_eq!(
            byte(stale),
            1,
            "the old buffer's mapping still holds the old pool"
        );
        assert_ne!(from, stale);
        pins.release(&cuda);
        // SAFETY: the second pool.
        unsafe { libc::munmap(at as *mut c_void, LEN) };
    }

    #[test]
    fn a_buffer_whose_rows_moved_ends_the_registration_at_the_old_address() {
        let (cuda, at) = (Calls::new(), pool(LEN, 0, None));
        let mut pins = ClientPins::default();
        let a = owner();
        pins.claim(at, 0x1000, &a, &cuda);
        let old = pins.claim(at, 0x1000, &a, &cuda);
        assert_eq!(pins.claim(at + 0x2000, 0x1000, &a, &cuda), at + 0x2000);
        assert_eq!(*cuda.unregistered.borrow(), [old]);
        assert!(pins.locked().is_empty());
    }

    #[test]
    fn a_destroyed_buffer_is_let_go_at_the_next_upload() {
        let (cuda, at) = (Calls::new(), pool(LEN, 0, None));
        let mut pins = ClientPins::default();
        let gone = owner();
        pins.claim(at, 0x1000, &gone, &cuda);
        let from = pins.claim(at, 0x1000, &gone, &cuda);
        drop(gone);
        pins.age(&cuda);
        assert_eq!(*cuda.unregistered.borrow(), [from]);
        assert!(pins.is_empty());
    }

    #[test]
    fn a_buffer_the_client_stopped_presenting_is_let_go() {
        let (cuda, at) = (Calls::new(), pool(LEN, 0, None));
        let mut pins = ClientPins::default();
        let (idle, live) = (owner(), owner());
        pins.claim(at, 0x1000, &idle, &cuda);
        let from = pins.claim(at, 0x1000, &idle, &cuda);
        for _ in 1..IDLE_UPLOADS {
            pins.claim(at + 0x1000, 0x1000, &live, &cuda);
        }
        assert!(
            cuda.unregistered.borrow().is_empty(),
            "a few frames keep it"
        );
        pins.age(&cuda);
        assert_eq!(*cuda.unregistered.borrow(), [from]);
        pins.release(&cuda);
    }

    #[test]
    fn rows_that_could_not_be_locked_are_not_tried_again_or_unregistered() {
        let (mut cuda, at) = (Calls::new(), pool(LEN, 0, None));
        cuda.takes = false;
        let mut pins = ClientPins::default();
        let a = owner();
        for _ in 0..4 {
            assert_eq!(pins.claim(at, 0x1000, &a, &cuda), at);
        }
        assert_eq!(cuda.registered.borrow().len(), 1);
        pins.release(&cuda);
        assert!(cuda.unregistered.borrow().is_empty());
        assert!(pins.is_empty());
    }
}
