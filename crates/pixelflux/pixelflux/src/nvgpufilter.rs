/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Multi-GPU NVENC `GET_ATTACHED_IDS` / `GET_PROBED_IDS` ioctl filter — it exists so a container
//! handed only a subset of the host's GPUs can still open an NVENC session.
//!
//! The problem: on NVIDIA driver 570-595, `libnvidia-encode` / `libcuda` / `libnvcuvid` enumerate
//! every *host* GPU via the RM `GET_ATTACHED_IDS` ioctl and try to peer-init each one — including
//! GPUs the container never exposed. A GPU whose `/dev/nvidiaX` node is absent or unopenable then makes
//! `nvEncOpenEncodeSessionEx` fail with UNSUPPORTED_DEVICE, so the session cannot open at all even
//! though a perfectly usable GPU is right there in the container.
//!
//! The fix is to strip the unreachable GPUs out of that enumeration response before the libraries
//! act on it. It GOT-patches `ioctl` in those NVIDIA libraries *only* — deliberately not an
//! LD_PRELOAD object, which would shadow every `ioctl` in the process and need its own recursion
//! guard. Because this crate's own GOT is left untouched, the wrapper's inner `ioctl` still resolves
//! to the real libc instead of re-entering itself.
//!
//! Everything hinges on at least one host GPU being hidden from the container, since that is the
//! only situation the bug arises in: on 565-or-before / 610-or-later drivers (enumeration already
//! correct) and whenever the container can see every host GPU, the strict-subset rule downstream
//! makes the whole filter a no-op.

// Walking the ELF tables and repointing GOT slots is raw-pointer work from
// end to end, so the safety contract is carried by the function signatures
// rather than by a block around each dereference.
#![allow(unsafe_op_in_unsafe_fn)]

use libc::{c_char, c_int, c_long, c_ulong, c_void};
use std::ffi::CStr;
use std::sync::Once;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// The ioctl command number (`_IOC_NR`, bits 0-7) that identifies the RM control escape —
/// the low byte the request gate keys on to recognize `NV_RM_CONTROL_REQUEST`. Kept as a named
/// constant only for documentation and the `ioc_nr_extracts_low_byte` unit test, hence
/// `#[allow(dead_code)]`.
#[allow(dead_code)]
const NV_ESC_RM_CONTROL: c_ulong = 0x2A;
/// The RM control ioctl the gate recognizes: `_IOWR('F'=0x46, NR=0x2A,
/// sizeof(NVOS54_PARAMETERS)=32)`, encoding DIR=READ|WRITE, TYPE=0x46, NR=0x2A, SIZE=0x20.
///
/// Matching is by DIR|TYPE|NR with the encoded `_IOC_SIZE` deliberately masked off (via
/// `ioc_no_size`): a driver whose `NVOS54_PARAMETERS` has a different `sizeof` encodes a different
/// size and would slip past an exact-request compare, yet it is still the very same command.
/// Matching the NR alone would be too loose and pinning the exact encoded size too tight, so the
/// gate keys on DIR|TYPE|NR and validates the true per-command parameter layout separately via
/// `ctrl.params_size`. This is the deliberate middle ground.
const NV_RM_CONTROL_REQUEST: c_ulong = 0xC020_462A;
/// Locates the `_IOC_SIZE` field so it can be cleared for size-agnostic request matching:
/// the 14-bit encoded parameter size lives at bit 16 of an ioctl request (`asm-generic/ioctl.h`).
/// Paired with `IOC_SIZEMASK` by `ioc_no_size`.
const IOC_SIZESHIFT: u32 = 16;
/// 14-bit mask covering the `_IOC_SIZE` field (`(1 << 14) - 1`); shifted by `IOC_SIZESHIFT`
/// and cleared from a request by `ioc_no_size` so the driver's param-struct size cannot affect the
/// match.
const IOC_SIZEMASK: c_ulong = 0x3FFF;
/// Identifies the attached-GPU enumeration response the filter rewrites — RM control command
/// `NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS`, matched against `NvRmControlParams::cmd`.
const GPU_GET_ATTACHED_IDS: u32 = 0x0201;
/// Identifies the probed-GPU enumeration response the filter rewrites — RM control command
/// `NV0000_CTRL_CMD_GPU_GET_PROBED_IDS`, matched against `NvRmControlParams::cmd`.
const GPU_GET_PROBED_IDS: u32 = 0x0214;
/// The RM ABI caps attached GPUs at 32, so this is the fixed `gpuIds` array capacity in both
/// enumeration params structs — and the bound the filter's scans and scratch buffers are sized to.
const MAX_ATTACHED_GPUS: usize = 32;
/// Marks the end of the live ids: the RM fills unused `gpuIds` slots with this sentinel, so
/// the first `INVALID_GPU_ID` terminates the populated prefix the filter scans and rewrites.
const INVALID_GPU_ID: u32 = 0xFFFF_FFFF;
/// The `params_size` that marks an ATTACHED response and tells it apart from PROBED: both
/// structs open with the same `gpuIds[32]`, so only the exact total size (here `gpuIds[32]` =
/// `4 * MAX_ATTACHED_GPUS`) disambiguates them before the shared leading array is rewritten.
const ATTACHED_PARAMS_SIZE: usize = 4 * MAX_ATTACHED_GPUS;
/// The `params_size` that marks a PROBED response and tells it apart from ATTACHED: this
/// struct is `gpuIds[32]` followed by `excludedGpuIds[32]` (`4 * MAX_ATTACHED_GPUS * 2`). Only the
/// shared leading `gpuIds[32]` is rewritten; the trailing `excludedGpuIds[32]` lists GPUs to
/// exclude, not to use, so it is deliberately left untouched.
const PROBED_PARAMS_SIZE: usize = 4 * MAX_ATTACHED_GPUS * 2;

/// The param block the RM control ioctl hands back (`NVOS54_PARAMETERS`, 32 bytes, pointed at
/// by `arg`) — modeled here so the filter can identify and validate an enumeration call before it
/// touches the GPU-id array it carries.
///
/// The rewrite reads `cmd` to identify the enumeration API, `status` and `params` to confirm the
/// call succeeded and carries a payload, and `params_size` to validate the exact payload layout
/// before dereferencing `params` (a `u64` user pointer to the command-specific struct).
#[repr(C)]
struct NvRmControlParams {
    h_client: u32,
    h_object: u32,
    cmd: u32,
    flags: u32,
    params: u64,
    params_size: u32,
    status: u32,
}

/// The `DT_*` `.dynamic` tags the GOT patch needs to locate a library's symbol, string, and
/// relocation tables — spelled out here because libc exposes `PT_DYNAMIC` but not these individual
/// values. `DT_NULL` (0) terminates the array.
const DT_NULL: i64 = 0;
/// ELF `.dynamic` tag `DT_PLTRELSZ` (2): total byte size of the PLT relocation table.
const DT_PLTRELSZ: i64 = 2;
/// ELF `.dynamic` tag `DT_RELA` (7): address of the general relocation table (`.rela.dyn`).
const DT_RELA: i64 = 7;
/// ELF `.dynamic` tag `DT_RELASZ` (8): total byte size of the `.rela.dyn` relocation table.
const DT_RELASZ: i64 = 8;
/// ELF `.dynamic` tag `DT_STRTAB` (5): address of the dynamic string table (symbol names).
const DT_STRTAB: i64 = 5;
/// ELF `.dynamic` tag `DT_SYMTAB` (6): address of the dynamic symbol table.
const DT_SYMTAB: i64 = 6;
/// ELF `.dynamic` tag `DT_JMPREL` (23): address of the PLT relocation table (`.rela.plt`).
const DT_JMPREL: i64 = 23;

// The two GOT-slot relocation types the filter repoints, per target architecture: the
// eagerly-bound `GLOB_DAT` slot the `-fno-plt` form uses (in `.rela.dyn`, the layout NVIDIA ships)
// and the classic lazily-bound `JUMP_SLOT` slot (in `.rela.plt`). Their numeric codes differ by
// architecture, so keying on the x86-64 values alone would silently match nothing on aarch64 and
// leave the multi-GPU filter a no-op there. `RELOC_ARCH_SUPPORTED` is false on any other
// architecture, where `install` reports the filter unsupported rather than patching nothing.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = assert!(RELOC_ARCH_SUPPORTED);

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        const RELOC_GLOB_DAT: u32 = 6;
        const RELOC_JUMP_SLOT: u32 = 7;
        const RELOC_ARCH_SUPPORTED: bool = true;
    } else if #[cfg(target_arch = "aarch64")] {
        const RELOC_GLOB_DAT: u32 = 1025;
        const RELOC_JUMP_SLOT: u32 = 1026;
        const RELOC_ARCH_SUPPORTED: bool = true;
    } else {
        const RELOC_GLOB_DAT: u32 = u32::MAX;
        const RELOC_JUMP_SLOT: u32 = u32::MAX;
        const RELOC_ARCH_SUPPORTED: bool = false;
    }
}

/// Identity of the RM control device, cached so the hook only rewrites responses that truly
/// came from it: the `rdev` of `/dev/nvidiactl`, resolved once at `install()`. A value of 0 means it
/// was never resolved, in which case the fd-identity gate is skipped and matching falls back to the
/// request/size checks alone.
static NVIDIACTL_RDEV: AtomicU64 = AtomicU64::new(0);

/// One `Elf64_Dyn` entry, modeled so the GOT patch can iterate a library's `.dynamic` array:
/// a `d_tag` (`DT_*`) paired with `d_un`, the 64-bit `d_val`/`d_ptr` union interpreted per tag.
#[repr(C)]
struct Elf64Dyn {
    d_tag: i64,
    d_un: u64,
}

/// One `Elf64_Rela` relocation entry, modeled so the patch can find each GOT slot and the
/// symbol it binds: `r_offset` (the target GOT slot, base-relative), `r_info` (packed symbol index +
/// relocation type, split by `elf64_r_sym`/`elf64_r_type`), and the unused `r_addend`.
#[repr(C)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

/// Extract the symbol-table index from an `Elf64_Rela` `r_info` field (its high 32 bits) —
/// how the patch learns which symbol a relocation binds, so it can check for `ioctl`.
#[inline]
fn elf64_r_sym(info: u64) -> u64 {
    info >> 32
}

/// Extract the relocation type from an `Elf64_Rela` `r_info` field (its low 32 bits) — how
/// the patch tells a `JUMP_SLOT`/`GLOB_DAT` GOT entry from relocations it must ignore.
#[inline]
fn elf64_r_type(info: u64) -> u32 {
    (info & 0xffff_ffff) as u32
}

/// Extract the `_IOC_NR` command byte (bits 0-7) of an ioctl request — the field that names
/// the command. Used only by documentation and the `ioc_nr_extracts_low_byte` unit test, hence
/// `#[allow(dead_code)]`.
#[allow(dead_code)]
#[inline]
fn ioc_nr(req: c_ulong) -> c_ulong {
    req & 0xFF
}

/// Strip the `_IOC_SIZE` field from a request (leaving DIR|TYPE|NR) so the gate stays bound
/// to the exact RM control command without coupling to any one driver's param-struct size — a driver
/// that changes `sizeof(NVOS54_PARAMETERS)` must still match.
#[inline]
fn ioc_no_size(req: c_ulong) -> c_ulong {
    req & !(IOC_SIZEMASK << IOC_SIZESHIFT)
}

/// The reachability test the whole filter turns on: a GPU id is kept only if its
/// `/dev/nvidia{minor}` node opens. Some runtimes create a node for every host GPU and enforce the
/// allocation in the device cgroup, so an unallocated node exists but `open()` fails with `EPERM`;
/// a presence test would count it as visible and leave the filter uninstalled.
fn node_reachable(minor: u32) -> bool {
    let path = format!("/dev/nvidia{}\0", minor);
    path_opens(CStr::from_bytes_with_nul(path.as_bytes()).unwrap())
}

/// Whether `path` opens read-write from this process. The descriptor is closed at once.
fn path_opens(path: &CStr) -> bool {
    unsafe {
        let fd = libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOCTTY,
        );
        if fd < 0 {
            return false;
        }
        libc::close(fd);
        true
    }
}

/// Map an RM `gpuId` to the `/dev/nvidia` minor that `node_reachable` needs: the id only
/// carries a PCI address, not a device minor, so this bridges the two by scanning
/// `/proc/driver/nvidia/gpus`, returning -1 when no match is found.
///
/// 1. **Extract the PCI address from the id**: the PCI address is encoded in `gpuId >> 8`, so
///    `want_bus` is the low bus byte `(gpuId >> 8) & 0xFF` and `want_full` is the full
///    `gpuId >> 8` (domain folded in for larger ids).
/// 2. **Match a /proc entry**: each subdirectory is named by its PCI address
///    `domain:bus:slot.func` in hex; split on `:` and `.` (expecting four fields) and parse the
///    domain and bus. An entry matches when its bus equals `want_bus` **or** its combined
///    `(domain << 8) | bus` equals `want_full` — the dual test covers ids that fold the domain in.
/// 3. **Read the minor**: from the matched entry's `information` file, parse the `Device Minor: N`
///    line and return `N`. The scan processes only the first matching directory (it `break`s
///    afterward) and returns -1 if no minor line is present.
fn gpuid_to_minor(gpu_id: u32) -> i32 {
    let want_bus = (gpu_id >> 8) & 0xFF;
    let want_full = gpu_id >> 8;
    let dir = match std::fs::read_dir("/proc/driver/nvidia/gpus") {
        Ok(d) => d,
        Err(_) => return -1,
    };
    for ent in dir.flatten() {
        let name = ent.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let parts: Vec<&str> = name.split([':', '.']).collect();
        if parts.len() != 4 {
            continue;
        }
        let dom = u32::from_str_radix(parts[0], 16);
        let bus = u32::from_str_radix(parts[1], 16);
        let (dom, bus) = match (dom, bus) {
            (Ok(d), Ok(b)) => (d, b),
            _ => continue,
        };
        if bus != want_bus && ((dom << 8) | bus) != want_full {
            continue;
        }
        let info = format!("/proc/driver/nvidia/gpus/{}/information", name);
        if let Ok(text) = std::fs::read_to_string(&info) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("Device Minor:")
                    && let Ok(m) = rest.trim().parse::<i32>()
                {
                    return m;
                }
            }
        }
        break;
    }
    -1
}

/// Drop the hidden GPUs from a `gpuIds[]` array in place — but only ever a strict subset, so
/// the filter can never accidentally blank out every GPU and strand the container with none. Ids
/// that pass `keep` are compacted to the front and the rest back-filled with `INVALID_GPU_ID`.
///
/// 1. **Scan the populated prefix**: iterate until the first `INVALID_GPU_ID`, counting `total`
///    live ids and collecting the kept ones into a scratch array (`nkept`).
/// 2. **Commit only a strict subset** (`0 < nkept < total`): copy the kept prefix back over `ids`
///    and set every trailing slot to `INVALID_GPU_ID`.
/// 3. **Fail-safe**: if none survive or all survive, leave the array untouched — never blank out
///    every GPU, and never do redundant work when nothing was dropped.
///
/// The `keep` predicate is injected so this stays free of `/proc` and `/dev` access and is
/// unit-testable in isolation.
fn filter_ids(ids: &mut [u32; MAX_ATTACHED_GPUS], keep: impl Fn(u32) -> bool) {
    let mut kept = [0u32; MAX_ATTACHED_GPUS];
    let (mut total, mut nkept) = (0usize, 0usize);
    for &id in ids.iter() {
        if id == INVALID_GPU_ID {
            break;
        }
        total += 1;
        if keep(id) {
            kept[nkept] = id;
            nkept += 1;
        }
    }
    if nkept > 0 && nkept < total {
        ids[..nkept].copy_from_slice(&kept[..nkept]);
        for slot in ids[nkept..].iter_mut() {
            *slot = INVALID_GPU_ID;
        }
    }
}

/// The seam where a hidden GPU gets scrubbed out: the `ioctl` wrapper installed into the
/// NVIDIA libraries' GOT slots, which forwards to the real `libc::ioctl` and then post-processes the
/// response. The three steps below exist only to make interposing on `ioctl` safe:
///
/// 1. **Call through**: invoke the genuine `libc::ioctl`. Only this crate's own GOT is left
///    unpatched, so this inner call resolves to libc rather than back into this wrapper (no
///    recursion).
/// 2. **Filter the result** under `catch_unwind`: `rewrite_attached_ids` may drop hidden GPUs from
///    the response. A panic must never unwind across this `extern "C"` boundary — the compiler
///    guard would abort the whole process — so any panic is caught and the unmodified real result
///    is returned instead.
/// 3. **Preserve `errno`**: the real syscall's `errno` is saved before the `/proc` + `/dev` lookups
///    inside the filter and restored afterward, so a caller reading `errno` sees the syscall's value.
unsafe extern "C" fn filtered_ioctl(fd: c_int, req: c_ulong, arg: *mut c_void) -> c_int {
    let rc = libc::ioctl(fd, req as _, arg);
    let saved_errno = *libc::__errno_location();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rewrite_attached_ids(fd, rc, req, arg);
    }));
    *libc::__errno_location() = saved_errno;
    rc
}

/// Scrub the hidden GPUs out of a successful `GET_ATTACHED_IDS` / `GET_PROBED_IDS` response so
/// the NVIDIA libraries never try to peer-init a GPU the container cannot reach: it drops every id
/// whose `/dev/nvidia` node is absent. Split from `filtered_ioctl` so the whole thing runs under
/// `catch_unwind`.
///
/// The guards run cheap-to-expensive and each returns early — leaving the response untouched — the
/// moment one fails, so the overwhelmingly common non-enumeration ioctl pays almost nothing:
///
/// 1. **Request gate**: the syscall succeeded (`rc == 0`), `arg` is non-null, and the request is
///    the RM_CONTROL ioctl matched by DIR|TYPE|NR (size-agnostic, via `ioc_no_size`).
/// 2. **fd identity gate**: when `NVIDIACTL_RDEV` was resolved at install, `fstat` the fd and
///    require a character device whose `rdev` equals `/dev/nvidiactl`'s — so only that char
///    device's responses are rewritten. An `fstat` failure passes through untouched; if the rdev
///    was never resolved the gate is skipped and matching relies on the request/size checks alone.
/// 3. **Payload gate**: the RM call itself reports success (`ctrl.status == 0`) and carries a
///    parameter pointer (`ctrl.params != 0`).
/// 4. **Command + exact-size dispatch**: `cmd` selects ATTACHED or PROBED, and `params_size` must
///    equal that command's exact struct size. Requiring the exact size both disambiguates the two
///    (their layouts share a leading array) and stops a lying size from steering a rewrite under a
///    different layout. Both APIs are filtered so behavior is uniform across driver versions —
///    buggy 570-595 where it actually drops GPUs, and correct drivers where the strict-subset rule
///    makes it a no-op.
/// 5. **Filter the leading `gpuIds[32]`**: `filter_ids` keeps an id only when `gpuid_to_minor`
///    resolves it and the corresponding `/dev/nvidia` node is present. For PROBED the trailing
///    `excludedGpuIds[]` is deliberately left untouched (it lists GPUs to exclude, not to use).
///
/// When `PIXELFLUX_GPU_FILTER_DEBUG` is set, the before/after live-id counts are logged.
unsafe fn rewrite_attached_ids(fd: c_int, rc: c_int, req: c_ulong, arg: *mut c_void) {
    if rc != 0 || ioc_no_size(req) != ioc_no_size(NV_RM_CONTROL_REQUEST) || arg.is_null() {
        return;
    }
    let cached = NVIDIACTL_RDEV.load(Ordering::Relaxed);
    if cached != 0 {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return;
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFCHR || st.st_rdev as u64 != cached {
            return;
        }
    }
    let ctrl = &mut *(arg as *mut NvRmControlParams);
    if ctrl.status != 0 || ctrl.params == 0 {
        return;
    }
    let which = match ctrl.cmd {
        GPU_GET_ATTACHED_IDS if ctrl.params_size as usize == ATTACHED_PARAMS_SIZE => "ATTACHED",
        GPU_GET_PROBED_IDS if ctrl.params_size as usize == PROBED_PARAMS_SIZE => "PROBED",
        _ => return,
    };
    let ids = &mut *(ctrl.params as *mut [u32; MAX_ATTACHED_GPUS]);
    let debug = std::env::var_os("PIXELFLUX_GPU_FILTER_DEBUG").is_some();
    let before = ids.iter().take_while(|&&id| id != INVALID_GPU_ID).count();
    filter_ids(ids, |id| {
        let minor = gpuid_to_minor(id);
        minor >= 0 && node_reachable(minor as u32)
    });
    if debug {
        let after = ids.iter().take_while(|&&id| id != INVALID_GPU_ID).count();
        eprintln!("[pixelflux] GET_{which}_IDS intercepted: {before} host GPU(s) -> {after} kept");
    }
}

/// Reports the current `PROT_*` protection of the page holding `addr` (from
/// `/proc/self/maps`, or -1 when the address is not found or maps is unreadable) for two reasons the
/// GOT patch depends on: so it can refuse to dereference an address that a wrong relative/absolute
/// guess landed in an unreadable page, and so it can restore a patched GOT page to its true original
/// protection rather than a hardcoded read-only.
///
/// Each maps line begins `lo-hi perms ...`; the address range and the `rwxp` permission string are
/// the first two whitespace fields. For the line whose `[lo, hi)` range contains `addr`, the `r`,
/// `w`, and `x` characters are translated into `PROT_READ`/`PROT_WRITE`/`PROT_EXEC`.
fn page_prot(addr: usize) -> i32 {
    let text = match std::fs::read_to_string("/proc/self/maps") {
        Ok(t) => t,
        Err(_) => return -1,
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let range = match it.next() {
            Some(r) => r,
            None => continue,
        };
        let perms = match it.next() {
            Some(p) => p,
            None => continue,
        };
        let mut rr = range.split('-');
        let lo = rr.next().and_then(|s| usize::from_str_radix(s, 16).ok());
        let hi = rr.next().and_then(|s| usize::from_str_radix(s, 16).ok());
        if let (Some(lo), Some(hi)) = (lo, hi)
            && addr >= lo
            && addr < hi
        {
            let b = perms.as_bytes();
            let mut prot = 0;
            if b.first() == Some(&b'r') {
                prot |= libc::PROT_READ;
            }
            if b.get(1) == Some(&b'w') {
                prot |= libc::PROT_WRITE;
            }
            if b.get(2) == Some(&b'x') {
                prot |= libc::PROT_EXEC;
            }
            return prot;
        }
    }
    -1
}

/// Turn a `DT_*` `d_ptr` into an absolute address across libc implementations that disagree
/// on what it holds: glibc pre-relocates these pointers to absolute, while musl leaves them
/// file-relative, so the patch cannot assume either. The heuristic treats a value below the load
/// `base` as a relative offset to add to `base`, and a value at or above `base` as already absolute.
#[inline]
fn dyn_addr(base: usize, v: u64) -> usize {
    let v = v as usize;
    if v < base { base + v } else { v }
}

/// Hand every GOT slot through which one loaded library calls `symbol` to `visit`, by walking its
/// `PT_DYNAMIC` array — the only way to interpose without an LD_PRELOAD object, or to read where the
/// dynamic linker bound an import — covering both the lazy-PLT and `-fno-plt` relocation forms
/// because NVIDIA ships the latter.
///
/// 1. **Parse `.dynamic`**: iterate the `Elf64Dyn` entries until `DT_NULL`, recording the dynamic
///    symbol table, string table, PLT relocation table (`DT_JMPREL`/`DT_PLTRELSZ`), and general
///    relocation table (`DT_RELA`/`DT_RELASZ`). Table addresses are resolved through `dyn_addr` to
///    handle the glibc-absolute vs musl-relative pointer conventions.
/// 2. **Sanity-gate the tables**: bail if the symbol or string table is missing, and — via the
///    `readable` closure over `page_prot` — bail if a table's mapping is known but lacks read
///    permission (an address whose mapping cannot be determined is presumed readable), so a wrong
///    relative/absolute guess from `dyn_addr` cannot fault on first dereference.
/// 3. **Visit both relocation tables**: hand each present, adequately-sized, readable table to
///    `visit_reloc_table`. The PLT table (`.rela.plt`) carries the classic lazily-bound `JUMP_SLOT`
///    entries; the general table (`.rela.dyn`) carries the `GLOB_DAT` slot that `-fno-plt` NVIDIA
///    builds use to bind `ioctl`.
unsafe fn visit_import_slots(
    base: usize,
    dynp: *const Elf64Dyn,
    symbol: &[u8],
    visit: &mut dyn FnMut(*mut *mut c_void),
) {
    let mut symtab: *const libc::Elf64_Sym = std::ptr::null();
    let mut strtab: *const c_char = std::ptr::null();
    let mut jmprel: *const Elf64Rela = std::ptr::null();
    let mut pltrelsz: usize = 0;
    let mut rela: *const Elf64Rela = std::ptr::null();
    let mut relasz: usize = 0;

    let mut d = dynp;
    while (*d).d_tag != DT_NULL {
        match (*d).d_tag {
            DT_SYMTAB => symtab = dyn_addr(base, (*d).d_un) as *const libc::Elf64_Sym,
            DT_STRTAB => strtab = dyn_addr(base, (*d).d_un) as *const c_char,
            DT_JMPREL => jmprel = dyn_addr(base, (*d).d_un) as *const Elf64Rela,
            DT_PLTRELSZ => pltrelsz = (*d).d_un as usize,
            DT_RELA => rela = dyn_addr(base, (*d).d_un) as *const Elf64Rela,
            DT_RELASZ => relasz = (*d).d_un as usize,
            _ => {}
        }
        d = d.add(1);
    }
    if symtab.is_null() || strtab.is_null() {
        return;
    }
    let readable = |p: usize| {
        let prot = page_prot(p);
        prot < 0 || (prot & libc::PROT_READ) != 0
    };
    if !readable(symtab as usize) || !readable(strtab as usize) {
        return;
    }

    let ent = std::mem::size_of::<Elf64Rela>();
    if !jmprel.is_null() && pltrelsz >= ent && readable(jmprel as usize) {
        visit_reloc_table(base, symtab, strtab, jmprel, pltrelsz / ent, symbol, visit);
    }
    if !rela.is_null() && relasz >= ent && readable(rela as usize) {
        visit_reloc_table(base, symtab, strtab, rela, relasz / ent, symbol, visit);
    }
}

/// Hand exactly the GOT slots of `symbol` in one relocation table to `visit` and nothing else —
/// every candidate entry is name-checked so no other symbol the library imports is disturbed. It
/// handles both the `JUMP_SLOT` and `GLOB_DAT` entry forms: an entry is skipped unless its
/// relocation type is one of those with a nonzero symbol index whose name, resolved through
/// `symtab`/`strtab`, is exactly `symbol`; the slot of a match lives at `base + r_offset`.
unsafe fn visit_reloc_table(
    base: usize,
    symtab: *const libc::Elf64_Sym,
    strtab: *const c_char,
    rela: *const Elf64Rela,
    count: usize,
    symbol: &[u8],
    visit: &mut dyn FnMut(*mut *mut c_void),
) {
    for i in 0..count {
        let r = rela.add(i);
        let rtype = elf64_r_type((*r).r_info);
        if rtype != RELOC_JUMP_SLOT && rtype != RELOC_GLOB_DAT {
            continue;
        }
        let sym_idx = elf64_r_sym((*r).r_info) as usize;
        if sym_idx == 0 {
            continue;
        }
        let name_off = (*symtab.add(sym_idx)).st_name as usize;
        let name = CStr::from_ptr(strtab.add(name_off));
        if name.to_bytes() != symbol {
            continue;
        }
        visit((base + (*r).r_offset as usize) as *mut *mut c_void);
    }
}

/// Repoint one `ioctl` GOT slot to `filtered_ioctl`.
///
/// 1. **Make the page writable**: read the slot's current protection via `page_prot` (defaulting to
///    read/write when maps is unreadable) and `mprotect` the page to `PROT_READ | PROT_WRITE`.
/// 2. **Publish the new pointer atomically**: store `filtered_ioctl` into the slot with an
///    `AtomicPtr` `Release` store, since another thread may be dispatching through this GOT slot
///    concurrently and the write must not tear.
/// 3. **Restore protection**: return the page to its original protection rather than a hardcoded
///    read-only — under partial RELRO these libraries lazily bind through this page, so leaving it
///    read-only would fault the next symbol resolve.
unsafe fn patch_ioctl_slot(slot: *mut *mut c_void) {
    let page = libc::sysconf(libc::_SC_PAGESIZE) as c_long;
    if page <= 0 {
        return;
    }
    let page = page as usize;
    let pg = (slot as usize & !(page - 1)) as *mut c_void;
    let mut orig = page_prot(slot as usize);
    if orig < 0 {
        orig = libc::PROT_READ | libc::PROT_WRITE;
    }
    if libc::mprotect(pg, page, libc::PROT_READ | libc::PROT_WRITE) == 0 {
        let ap = &*(slot as *const AtomicPtr<c_void>);
        ap.store(filtered_ioctl as *mut c_void, Ordering::Release);
        libc::mprotect(pg, page, orig);
    }
}

/// `dl_iterate_phdr` callback: for each loaded object matching a targeted NVIDIA library,
/// find its `PT_DYNAMIC` segment and repoint its `ioctl` slots. Returns 0 to keep iterating.
///
/// 1. **Panic firewall**: the whole body runs under `catch_unwind` because a panic must not unwind
///    across this `extern "C"` boundary into the C `dl_iterate_phdr` — that would abort the
///    process; a caught panic just ends this object's processing and iteration continues.
/// 2. **Tight library match**: only `libnvcuvid`, `libnvidia-encode`, and `libcuda.so` are patched
///    — the libraries that issue the enumeration ioctl (`libnvidia-encode` calls through
///    `libnvcuvid`). Unrelated modules (`libcudart`, `libnvidia-ml`, `libnvidia-glcore`, …) and
///    objects with no name are skipped so their `ioctl` bindings are left untouched.
/// 3. **Patch each `PT_DYNAMIC`**: for a matched object, walk its program headers and hand every
///    `ioctl` slot of each dynamic segment to `patch_ioctl_slot`, using the object's load address as
///    the base.
unsafe extern "C" fn patch_phdr_cb(
    info: *mut libc::dl_phdr_info,
    _size: libc::size_t,
    _data: *mut c_void,
) -> c_int {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let info = &*info;
        if info.dlpi_name.is_null() || *info.dlpi_name == 0 {
            return;
        }
        let name = CStr::from_ptr(info.dlpi_name).to_string_lossy();
        if !name.contains("libnvcuvid")
            && !name.contains("libnvidia-encode")
            && !name.contains("libcuda.so")
        {
            return;
        }
        let base = info.dlpi_addr as usize;
        for i in 0..info.dlpi_phnum as isize {
            let ph = &*info.dlpi_phdr.offset(i);
            if ph.p_type == libc::PT_DYNAMIC {
                visit_import_slots(
                    base,
                    (base + ph.p_vaddr as usize) as *const Elf64Dyn,
                    b"ioctl",
                    &mut |slot| patch_ioctl_slot(slot),
                );
            }
        }
    }));
    0
}

/// Where the first loaded object whose path contains `object` has its import of `symbol` bound:
/// the address its GOT slot holds, which is the library's own PLT stub while a lazy binding has not
/// been resolved yet. `None` when no such object is loaded or it does not import `symbol`.
pub(crate) fn bound_import(object: &str, symbol: &[u8]) -> Option<usize> {
    struct Query<'a> {
        object: &'a str,
        symbol: &'a [u8],
        found: Option<usize>,
        done: bool,
    }
    unsafe extern "C" fn cb(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut c_void,
    ) -> c_int {
        let q = &mut *(data as *mut Query);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let info = &*info;
            if info.dlpi_name.is_null()
                || !CStr::from_ptr(info.dlpi_name)
                    .to_string_lossy()
                    .contains(q.object)
            {
                return;
            }
            q.done = true;
            let (symbol, found) = (q.symbol, &mut q.found);
            let base = info.dlpi_addr as usize;
            for i in 0..info.dlpi_phnum as isize {
                let ph = &*info.dlpi_phdr.offset(i);
                if ph.p_type == libc::PT_DYNAMIC {
                    visit_import_slots(
                        base,
                        (base + ph.p_vaddr as usize) as *const Elf64Dyn,
                        symbol,
                        &mut |slot| {
                            if found.is_none() {
                                *found = Some(
                                    (*(slot as *const AtomicPtr<c_void>)).load(Ordering::Acquire)
                                        as usize,
                                );
                            }
                        },
                    );
                }
            }
        }));
        q.done as c_int
    }
    let mut q = Query {
        object,
        symbol,
        found: None,
        done: false,
    };
    unsafe {
        libc::dl_iterate_phdr(Some(cb), &mut q as *mut Query as *mut c_void);
    }
    q.found
}

/// True when at least one host GPU is hidden from the container — the only situation that
/// can trigger the peer-init bug, so it gates whether the filter installs at all.
///
/// Compares two counts: `host` is the number of GPUs the kernel driver knows, from the non-dot
/// entries of `/proc/driver/nvidia/gpus`; `visible` is the number of `/dev/nvidia{0..31}` nodes
/// the container can open. GPUs are hidden when `host > visible`, and the `visible > 0`
/// clause ensures there is still a usable GPU (a container with no GPUs at all is not this case).
fn has_hidden_gpus() -> bool {
    let host = std::fs::read_dir("/proc/driver/nvidia/gpus")
        .map(|d| {
            d.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count()
        })
        .unwrap_or(0);
    let visible = (0..MAX_ATTACHED_GPUS as u32)
        .filter(|&m| node_reachable(m))
        .count();
    host > visible && visible > 0
}

/// Install the `GET_ATTACHED_IDS`/`GET_PROBED_IDS` GOT filter, at most once and only when a
/// host GPU is hidden from the container. Idempotent and safe to call before every NVENC session
/// open (guarded by a `Once`).
///
/// 1. **Escape hatch**: if `PIXELFLUX_DISABLE_GPU_FILTER` is set, log and return without patching
///    (also useful for A/B testing the filter).
/// 2. **Cache the nvidiactl identity**: `stat` `/dev/nvidiactl` and store its `rdev` in
///    `NVIDIACTL_RDEV` so the ioctl hook can later verify an fd's identity before rewriting. If the
///    `stat` fails the value stays 0, which makes the hook skip that identity gate.
/// 3. **Patch only when needed**: if `has_hidden_gpus()` reports GPUs hidden from the container,
///    run `dl_iterate_phdr` over `patch_phdr_cb` to repoint the `ioctl` GOT slots in the targeted
///    NVIDIA libraries. Otherwise nothing is patched — the filter is a strict no-op.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("PIXELFLUX_DISABLE_GPU_FILTER").is_some() {
            eprintln!("[pixelflux] multi-GPU NVENC filter disabled via PIXELFLUX_DISABLE_GPU_FILTER");
            return;
        }
        if !RELOC_ARCH_SUPPORTED {
            eprintln!("[pixelflux] multi-GPU NVENC filter unsupported on this architecture; not patching");
            return;
        }
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(c"/dev/nvidiactl".as_ptr(), &mut st) == 0 {
                NVIDIACTL_RDEV.store(st.st_rdev as u64, Ordering::Relaxed);
            }
        }
        if has_hidden_gpus() {
            unsafe {
                libc::dl_iterate_phdr(Some(patch_phdr_cb), std::ptr::null_mut());
            }
            eprintln!("[pixelflux] multi-GPU NVENC ioctl filter installed (GET_ATTACHED_IDS/GET_PROBED_IDS)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping a strict subset compacts the survivors and back-fills the rest: of three
    /// live ids, keeping two (0x100, 0x300) rewrites the array so the survivors move to the front
    /// and every trailing slot becomes `INVALID_GPU_ID`.
    #[test]
    fn filter_keeps_strict_subset_and_invalidates_rest() {
        let mut ids = [INVALID_GPU_ID; MAX_ATTACHED_GPUS];
        ids[0] = 0x100;
        ids[1] = 0x200;
        ids[2] = 0x300;
        filter_ids(&mut ids, |id| id == 0x100 || id == 0x300);
        assert_eq!(ids[0], 0x100);
        assert_eq!(ids[1], 0x300);
        assert_eq!(ids[2], INVALID_GPU_ID);
        assert_eq!(ids[3], INVALID_GPU_ID);
    }

    /// Fail-safe when every id is kept: `nkept == total`, so the array is left exactly as-is
    /// rather than needlessly rewritten.
    #[test]
    fn filter_noop_when_all_kept() {
        let mut ids = [INVALID_GPU_ID; MAX_ATTACHED_GPUS];
        ids[0] = 0xAA;
        ids[1] = 0xBB;
        filter_ids(&mut ids, |_| true);
        assert_eq!(ids[0], 0xAA);
        assert_eq!(ids[1], 0xBB);
        assert_eq!(ids[2], INVALID_GPU_ID);
    }

    /// Fail-safe when no id is kept: `nkept == 0` never blanks the array, so the ids survive
    /// untouched instead of leaving zero usable GPUs.
    #[test]
    fn filter_noop_when_none_kept() {
        let mut ids = [INVALID_GPU_ID; MAX_ATTACHED_GPUS];
        ids[0] = 0xAA;
        ids[1] = 0xBB;
        filter_ids(&mut ids, |_| false);
        assert_eq!(ids[0], 0xAA);
        assert_eq!(ids[1], 0xBB);
    }

    /// `ioc_nr` extracts the low NR byte: the full request `0xC020462A` yields
    /// `NV_ESC_RM_CONTROL` (0x2A).
    #[test]
    fn ioc_nr_extracts_low_byte() {
        assert_eq!(ioc_nr(0xC020462A), NV_ESC_RM_CONTROL);
    }

    /// The GOT-slot relocation codes track the build architecture: keying on the x86-64
    /// numbers on aarch64 would match nothing and silently disable the filter, so each supported
    /// arch carries its own `JUMP_SLOT` / `GLOB_DAT` pair. The two arches the filter supports use
    /// distinct codes, and both must be flagged supported.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn reloc_types_track_the_target_arch() {
        assert_ne!(RELOC_JUMP_SLOT, RELOC_GLOB_DAT);
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(RELOC_JUMP_SLOT, 7);
            assert_eq!(RELOC_GLOB_DAT, 6);
        }
        #[cfg(target_arch = "aarch64")]
        {
            assert_eq!(RELOC_JUMP_SLOT, 1026);
            assert_eq!(RELOC_GLOB_DAT, 1025);
        }
    }

    /// Proves the property the whole gate rests on — matching stays size-agnostic yet
    /// NR-precise — so a future driver revision cannot silently defeat it. A request that shares
    /// DIR|TYPE|NR but encodes a different `_IOC_SIZE` (here 0x30, versus the canonical 0x20 =
    /// `sizeof(NVOS54_PARAMETERS)`) still matches once the size is masked off, so a driver whose
    /// `NVOS54_PARAMETERS` differs in size is not rejected by the request gate; a request carrying a
    /// different NR (0x2B) does not match, confirming NR precision is retained.
    #[test]
    fn request_match_ignores_param_size() {
        let base = ioc_no_size(NV_RM_CONTROL_REQUEST);
        let other_size = base | (0x30 << IOC_SIZESHIFT);
        assert_ne!(other_size, NV_RM_CONTROL_REQUEST);
        assert_eq!(ioc_no_size(other_size), base);
        let other_nr = (NV_RM_CONTROL_REQUEST & !0xFF) | 0x2B;
        assert_ne!(ioc_no_size(other_nr), base);
    }

    /// Guards against silent NVIDIA ABI drift by pinning the on-wire layout the size dispatch
    /// relies on: ATTACHED params are `gpuIds[32]` (128 bytes) and PROBED params are
    /// `gpuIds[32] + excludedGpuIds[32]` (256 bytes). Because both lead with the same `gpuIds[32]`,
    /// these exact sizes are what let the exact-size guards disambiguate the two commands;
    /// `NvRmControlParams` is 32 bytes.
    #[test]
    fn param_struct_sizes_match_nvidia_layout() {
        assert_eq!(ATTACHED_PARAMS_SIZE, 128);
        assert_eq!(PROBED_PARAMS_SIZE, 256);
        assert_eq!(std::mem::size_of::<NvRmControlParams>(), 32);
    }

    /// A node that exists but refuses `open()` is unreachable, as a device-cgroup-denied
    /// `/dev/nvidiaN` is; existence alone must not count. Root bypasses mode bits, so the
    /// unopenable case is only asserted for an unprivileged user.
    #[test]
    fn reachability_requires_open_not_existence() {
        assert!(path_opens(c"/dev/null"));
        assert!(!path_opens(c"/nonexistent/pixelflux-node"));
        let path =
            std::env::temp_dir().join(format!("pixelflux-unopenable-{}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(!path_opens(&cpath));
        }
        std::fs::remove_file(&path).unwrap();
    }
}
