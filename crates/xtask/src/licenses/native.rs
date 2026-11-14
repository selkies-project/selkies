/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The native libraries behind the crates that bind, vendor, or load them, which crate
//! metadata cannot see.
//!
//! An entry's category and the features that link a copyleft library in its place are one fact
//! for every graph the crate is in. What the library is and how it reaches an artifact is told
//! once per inventory that lists it: the server's, `docs/licensing.md`, and pixelflux's wheels',
//! `crates/pixelflux/LICENSES.md`. Every crate named like a native binding (`-sys`, `_sys`,
//! `-ffi`) has to be described for each inventory whose graph contains it, so a new binding
//! fails the check until it is inventoried; every entry the server describes has to be in its
//! graph, so a dropped one fails until it is taken off the page.

use super::spdx::Rank;

/// A native library behind a crate, and how each inventory describes it.
pub struct Native {
    pub krate: &'static str,
    /// The category of the native code.
    pub rank: Rank,
    /// The crate features that link a copyleft library in its place.
    pub copyleft_features: &'static [&'static str],
    /// The server's description, where its graph has the crate.
    pub server: Option<Text>,
    /// The description of pixelflux's wheels, where their graph has the crate.
    pub wheel: Option<Text>,
}

/// What a native library is, its license, and how it reaches one artifact.
pub struct Text {
    pub library: &'static str,
    pub license: &'static str,
    pub how: &'static str,
}

/// The entry for `krate`, if there is one.
pub fn find(krate: &str) -> Option<&'static Native> {
    NATIVE.iter().find(|native| native.krate == krate)
}

/// Every entry, in the order the server's page lists them.
pub const NATIVE: &[Native] = &[
    Native {
        krate: "codec-sys",
        rank: Rank::Weak,
        copyleft_features: &["x264", "x265"],
        server: Some(Text {
            library: "libvpx, SVT-AV1, dav1d, and libde265 in every build; libx264 and x265 in the \
                      GPL build, kvazaar in the non-GPL one",
            license: "BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media \
                      patent license), BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265), \
                      GPL-2.0-or-later (libx264, x265)",
            how: "linked shared libraries, through pixelflux; the software H.264 (GPL build), \
                  HEVC, VP8, VP9, and AV1 encoders and the virtual camera's decoders",
        }),
        wheel: Some(Text {
            library: "libvpx, SVT-AV1, dav1d, and libde265 on every wheel; libx264 and x265 \
                      (GPL-2.0-or-later) on the GPL wheel, kvazaar on the non-GPL one",
            license: "BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media \
                      patent license), BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265)",
            how: "linked shared libraries, bound at build time from the headers of the copies that \
                  are linked (bundled into the wheels by auditwheel); crate itself MIT OR \
                  Apache-2.0, path dependency",
        }),
    },
    Native {
        krate: "openh264-sys2",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "Cisco OpenH264, and libstdc++ for its C++",
            license: "BSD-2-Clause (OpenH264), GPL-3.0-or-later WITH GCC-exception-3.1 \
                      (libstdc++)",
            how: "compiled from the source vendored in the crate and linked statically, through \
                  pixelflux, libstdc++ as a shared library; full-frame software H.264 of the \
                  non-GPL build and the virtual camera's H.264 decoder",
        }),
        wheel: Some(Text {
            library: "Cisco OpenH264 2.6 (vendored source)",
            license: "BSD-2-Clause",
            how: "compiled from vendored source and linked statically",
        }),
    },
    Native {
        krate: "turbojpeg-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libjpeg-turbo",
            license: "IJG AND BSD-3-Clause AND Zlib",
            how: "compiled from the source vendored in the crate and linked statically, through \
                  pixelflux; JPEG",
        }),
        wheel: Some(Text {
            library: "libjpeg-turbo 3.1 (vendored source)",
            license: "IJG AND BSD-3-Clause AND Zlib",
            how: "compiled from vendored source and linked statically (cmake feature)",
        }),
    },
    Native {
        krate: "va-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libva, libva-drm",
            license: "MIT",
            how: "`dlopen` at run time through pixelflux, never linked; VA-API encoding",
        }),
        wheel: Some(Text {
            library: "libva (libva.so.2, libva-drm.so.2)",
            license: "MIT",
            how: "dlopen at run time, never linked or shipped; the bindings are generated from the \
                  libva headers vendored under pixelflux/va-sys/headers/ (MIT); crate itself MIT \
                  OR Apache-2.0, path dependency",
        }),
    },
    Native {
        krate: "nvcodec-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "NVENC, NvFBC, and the CUDA driver (`libnvidia-encode.so.1`, \
                      `libnvidia-fbc.so.1`, `libcuda.so.1`)",
            license: "proprietary (NVIDIA driver); the vendored headers MIT",
            how: "`dlopen` at run time through pixelflux, never linked or shipped",
        }),
        wheel: Some(Text {
            library: "NVIDIA NVENC (libnvidia-encode.so.1), framebuffer capture \
                      (libnvidia-fbc.so.1), and CUDA driver (libcuda.so.1)",
            license: "proprietary driver libraries; nvEncodeAPI.h is MIT, the CUDA bindings are \
                      declarations generated from the CUDA toolkit headers",
            how: "dlopen at run time, never linked or shipped; crate MIT OR Apache-2.0",
        }),
    },
    Native {
        krate: "gbm-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libgbm (Mesa)",
            license: "MIT",
            how: "linked shared library, through pixelflux",
        }),
        wheel: Some(Text {
            library: "libgbm (Mesa)",
            license: "MIT",
            how: "linked shared library (system)",
        }),
    },
    Native {
        krate: "pixman-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libpixman-1",
            license: "MIT",
            how: "linked shared library, through pixelflux's capture compositor",
        }),
        wheel: Some(Text {
            library: "libpixman-1",
            license: "MIT",
            how: "linked shared library (system)",
        }),
    },
    Native {
        krate: "drm-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libdrm headers",
            license: "MIT",
            how: "bindings generated from the headers, through pixelflux; no libdrm symbol is \
                  linked",
        }),
        wheel: Some(Text {
            library: "libdrm headers (bindings only)",
            license: "MIT",
            how: "bindings generated from the headers; drm-ffi issues the ioctls itself and no \
                  libdrm symbol is linked",
        }),
    },
    Native {
        krate: "drm-ffi",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "Linux DRM ioctls (no library)",
            license: "MIT",
            how: "pure Rust ioctl wrappers, through pixelflux",
        }),
        wheel: Some(Text {
            library: "Linux DRM ioctls (no library)",
            license: "MIT",
            how: "pure Rust ioctl wrappers",
        }),
    },
    Native {
        krate: "wayland-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libwayland-server",
            license: "MIT",
            how: "`dlopen` at run time through pixelflux's capture compositor, never linked",
        }),
        wheel: Some(Text {
            library: "libwayland-server",
            license: "MIT",
            how: "dlopen at run time (`dlopen` feature), never linked",
        }),
    },
    Native {
        krate: "input-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libinput",
            license: "MIT",
            how: "in the graph through smithay's libinput backend, which pixelflux does not use; \
                  nothing is linked",
        }),
        wheel: Some(Text {
            library: "libinput",
            license: "MIT",
            how: "in the crate graph through smithay's backend_libinput; no symbol is referenced, \
                  so the linker drops it from NEEDED",
        }),
    },
    Native {
        krate: "libudev-sys",
        rank: Rank::Weak,
        copyleft_features: &[],
        server: Some(Text {
            library: "libudev (systemd)",
            license: "LGPL-2.1-or-later",
            how: "in the graph through smithay's udev backend, which pixelflux does not use; \
                  nothing is linked",
        }),
        wheel: Some(Text {
            library: "libudev (systemd)",
            license: "LGPL-2.1-or-later",
            how: "in the crate graph through smithay's backend_udev; no symbol is referenced, so \
                  the linker drops it from NEEDED",
        }),
    },
    Native {
        krate: "xkbcommon",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libxkbcommon",
            license: "MIT",
            how: "linked shared library; keymaps, in the server and in pixelflux",
        }),
        wheel: Some(Text {
            library: "libxkbcommon",
            license: "MIT",
            how: "linked shared library (system)",
        }),
    },
    Native {
        krate: "libpulse-sys",
        rank: Rank::Weak,
        copyleft_features: &[],
        server: Some(Text {
            library: "libpulse",
            license: "LGPL-2.1-or-later",
            how: "linked shared library, replaceable; the sound-server control plane \
                  (`libpulse-binding`) and pcmflux's capture",
        }),
        wheel: None,
    },
    Native {
        krate: "opusic-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "libopus",
            license: "BSD-3-Clause",
            how: "compiled from the source bundled in the crate and linked statically, through \
                  pcmflux; Opus",
        }),
        wheel: None,
    },
    Native {
        krate: "aws-lc-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "AWS-LC",
            license: "ISC AND Apache-2.0 AND MIT AND BSD-3-Clause",
            how: "compiled from the source vendored in the crate and linked statically; the HTTPS \
                  server's, the WebRTC stack's, and outgoing requests' cryptography (`aws-lc-rs`)",
        }),
        wheel: None,
    },
    Native {
        krate: "nvml-wrapper-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "NVML (`libnvidia-ml.so.1`)",
            license: "proprietary (NVIDIA driver)",
            how: "`dlopen` at run time when present, never linked or shipped; GPU statistics",
        }),
        wheel: None,
    },
    Native {
        krate: "linux-raw-sys",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "Linux kernel ABI (no library)",
            license: "GPL-2.0-only WITH Linux-syscall-note (the kernel's UAPI headers)",
            how: "syscall numbers and structures as Rust constants (rustix)",
        }),
        wheel: Some(Text {
            library: "Linux kernel ABI (syscall numbers and structs)",
            license: "Linux-syscall-note",
            how: "pure-Rust constants, no library",
        }),
    },
    Native {
        krate: "libc",
        rank: Rank::Weak,
        copyleft_features: &[],
        server: Some(Text {
            library: "C runtime: glibc and libgcc_s, or musl in the musl builds",
            license: "LGPL-2.1-or-later (glibc), GPL-3.0-or-later WITH GCC-exception-3.1 \
                      (libgcc_s), MIT (musl)",
            how: "linked shared libraries (glibc, libgcc_s) or statically (musl), as for every \
                  program",
        }),
        wheel: Some(Text {
            library: "C runtime (glibc, or musl on musllinux wheels)",
            license: "LGPL-2.1-or-later (glibc), MIT (musl)",
            how: "linked shared library (system), as for every program",
        }),
    },
    Native {
        krate: "libloading",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: Some(Text {
            library: "the driver and desktop libraries above that are opened at run time",
            license: "MIT and proprietary",
            how: "`dlopen`, never linked",
        }),
        wheel: Some(Text {
            library: "libEGL.so.1 (Mesa/Khronos, MIT), libpipewire-0.3.so.0 (MIT), \
                      libwayland-server.so.0 (MIT), libva.so.2 and libva-drm.so.2 (MIT), \
                      libcuda.so.1/libnvidia-encode.so.1/libnvidia-fbc.so.1 (proprietary), and on \
                      aarch64 the Jetson Linux \
                      libnvv4l2.so/libnvbuf_utils.so/libnvbufsurface.so/libnvbufsurftransform.so \
                      (proprietary)",
            license: "MIT and proprietary driver libraries",
            how: "dlopen at run time, never linked",
        }),
    },
    Native {
        krate: "pyo3-ffi",
        rank: Rank::Permissive,
        copyleft_features: &[],
        server: None,
        wheel: Some(Text {
            library: "libpython (CPython)",
            license: "PSF-2.0",
            how: "extension module: symbols resolved from the hosting interpreter, not linked",
        }),
    },
];
