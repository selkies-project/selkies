# Third-party licenses

pixelflux itself is licensed under the [Mozilla Public License 2.0](LICENSE).
This file inventories what a built `pixelflux` extension contains or loads, per
build configuration, and the license of each piece. It is enforced by
`scripts/check-licenses.py`, `deny.toml`, and the root license checks
(see [How this is enforced](#how-this-is-enforced)).

Categories used below:

| Category | Meaning | Licenses |
| --- | --- | --- |
| copyleft | the whole combined binary must be distributed under the license's terms | GPL-2.0-or-later (libx264, x265) |
| weak copyleft | the library itself stays under its license and must remain replaceable (dynamic linking is fine) | LGPL-2.1-or-later (glibc, libudev), LGPL-3.0-or-later (libde265) |
| permissive | attribution only | MIT, BSD, Apache-2.0, ISC, Zlib, BSL-1.0, Unlicense, WTFPL, ... |

MPL-2.0 is file-level copyleft (modified MPL files stay MPL, the larger work
may be under any license) and is grouped with the permissive licenses here.

## Build configurations

| Configuration | How it is selected | Software H.264 encoder | Software H.265 encoder | Cargo features |
| --- | --- | --- | --- | --- |
| default (GPL) | `pip install pixelflux` / the published wheels | libx264 (GPL-2.0-or-later) | x265 (GPL-2.0-or-later) | `gpl` (default) |
| non-GPL | `PIXELFLUX_ENABLE_GPL=0 pip install .` | Cisco OpenH264 (BSD-2-Clause, compiled from vendored source) | kvazaar (BSD-3-Clause) | `--no-default-features --features openh264` |

Both configurations share everything else: JPEG, the VP8/VP9 (libvpx) and AV1
(SVT-AV1) software encoders, the virtual camera's decoders (OpenH264, libvpx,
dav1d, libde265, libjpeg-turbo), NVENC, VA-API, capture, and compositor. A build
with neither feature does not compile.

## Native libraries and vendored code

Every component of either build that is not a Rust crate. "How used" is what
was observed on the built extension (`readelf -d`, `ldd`, the strings passed to
`dlopen`) and in the crates' build scripts; "Build" says which configuration
contains it.

| Component | License | Category | Build | How used | Notes |
| --- | --- | --- | --- | --- | --- |
| libx264 (via `codec-sys`, feature `x264`) | GPL-2.0-or-later | copyleft | GPL only | linked shared library (`NEEDED libx264.so.*`), bound at build time from its headers; auditwheel bundles it into the GPL wheels | Software H.264, built with the patches under `patches/x264` (GPL-2.0-or-later, as x264). |
| Cisco OpenH264 2.6 (via `openh264-sys2`) | BSD-2-Clause | permissive | both | compiled from the source vendored in the crate (needs a C++ toolchain and nasm) and linked statically; no binary download | Its decoder is the virtual camera's H.264 decoder in every build; its encoder is the software H.264 of the non-GPL build. Cisco's royalty-covered binary module is irrelevant to a source build; the AVC patent pool applies to any H.264 codec and is the deployer's concern. Pulls `libstdc++` in as the only C++ code. |
| x265 (via `codec-sys`, feature `x265`) | GPL-2.0-or-later | copyleft | GPL only | linked shared library (`NEEDED libx265.so.*`), bound at build time from its headers; bundled into the GPL wheels | software H.265 (incl. 4:4:4), built with the patches under `patches/x265` (GPL-2.0-or-later, as x265) |
| kvazaar (via `codec-sys`, feature `kvazaar`) | BSD-3-Clause | permissive | non-GPL only | linked shared library, bound at build time from its headers; bundled into the non-GPL wheels | software H.265 of a GPL-free build (4:2:0) |
| libvpx (via `codec-sys`) | BSD-3-Clause | permissive | both | linked shared library, bound at build time from its headers; bundled into the wheels | software VP8 and VP9 encoders, built with the patches under `patches/libvpx` (BSD-3-Clause, as libvpx), and the virtual camera's VP8 and VP9 decoders |
| SVT-AV1 (via `codec-sys`) | BSD-3-Clause-Clear (with the Alliance for Open Media patent license) | permissive | both | linked shared library, bound at build time from its headers; bundled into the wheels | software AV1, built with the patch under `patches/svtav1` (BSD-3-Clause-Clear, as SVT-AV1) |
| dav1d (via `codec-sys`) | BSD-2-Clause | permissive | both | linked shared library, bound at build time from its headers; bundled into the wheels | the virtual camera's AV1 decoder |
| libde265 (via `codec-sys`) | LGPL-3.0-or-later | weak copyleft | both | linked shared library, bound at build time from its headers; bundled into the wheels, replaceable | the virtual camera's HEVC decoder |
| libva, libva-drm (via `va-sys`) | MIT | permissive | both | `dlopen("libva.so.2")` and `dlopen("libva-drm.so.2")` at run time, never linked; not in the wheel, the host's copy serves the driver it was built for. The bindings are generated from the libva 2.24 headers vendored under `va-sys/headers/` (MIT) | VA-API encoders and video processor |
| libdrm | MIT | permissive | both (headers only) | `drm-sys` carries bindings generated from its headers and `drm-ffi` issues the ioctls itself; no libdrm symbol is linked | DRM/KMS |
| libgbm (Mesa) | MIT | permissive | both | linked shared library (`gbm-sys`); excluded from the wheel | GPU buffer allocation |
| libpixman-1 | MIT | permissive | both | linked shared library (`pixman-sys`); excluded from the wheel | software renderer of the compositor |
| libxkbcommon | MIT | permissive | both | linked shared library (`xkbcommon` crate); excluded from the wheel | keymaps |
| libwayland-server | MIT | permissive | both | `dlopen` at run time (`wayland-sys` `dlopen` feature), never linked | capture compositor. libwayland-client is not used: the Wayland client side is the pure-Rust `wayland-backend`. |
| NVIDIA NvFBC (`libnvidia-fbc.so.1`) | proprietary (NVIDIA driver component); the `NvFBC.h` header vendored under `nvcodec-sys/headers/` is under NVIDIA's MIT-style permission grant | permissive (header); driver component | both | `dlopen("libnvidia-fbc.so.1")` at run time for the X11 zero-copy capture path, never linked; excluded from the wheel | ships with the NVIDIA driver and is injected into containers under the `video` driver capability, like `libnvidia-encode.so.1`. The header is vendored for provenance only; the bindings are hand-written |
| libEGL (Mesa / vendor) | MIT (Mesa), Khronos headers | permissive | both | `dlopen("libEGL.so.1")` at run time for the dmabuf import path, never linked; excluded from the wheel | GL entry points are resolved through `eglGetProcAddress` |
| libpipewire-0.3 | MIT | permissive | both | `dlopen("libpipewire-0.3.so.0")` at run time for the virtual-camera PipeWire sink and the portal screen-capture streams, never linked | no build-time PipeWire dependency; the xdg-desktop-portal bus client is the pure-Rust `zbus` |
| libinput | MIT | permissive | both (graph only) | `input-sys` is in the crate graph through smithay's `backend_libinput`, but no symbol is referenced and the linker drops it: not in `NEEDED` | compositor backend not used by pixelflux |
| libudev (systemd) | LGPL-2.1-or-later | weak copyleft | both (graph only) | `libudev-sys` is in the crate graph through smithay's `backend_udev`; not referenced, not in `NEEDED` | compositor backend not used by pixelflux |
| libjpeg-turbo 3.1 (via `turbojpeg-sys`) | IJG AND BSD-3-Clause AND Zlib | permissive | both | compiled from the source vendored in the crate (cmake + nasm) and linked statically; no system `libturbojpeg` | JPEG encoder and the virtual-camera MJPEG decoder |
| libyuv | not used | - | - | - | `yuv` is a pure-Rust crate (BSD-3-Clause OR Apache-2.0), not a binding to Google's libyuv |
| libxcb, libX11 | not linked | - | - | `x11rb` is pure Rust and opens the X socket itself; no libxcb/libX11 symbol is linked (the `auditwheel --exclude` entries for them are harmless) | X11 capture, XTEST, XFixes cursor |
| NVIDIA NVENC (`libnvidia-encode.so.1`) and CUDA driver API (`libcuda.so.1`) | proprietary (NVIDIA driver) | - | both | `dlopen` at run time, never linked, never shipped; only present on hosts with the NVIDIA driver | NVENC encoder |
| NVIDIA Jetson Linux encoder and surface libraries (`libnvv4l2.so`, and `libnvbuf_utils.so` on JetPack 4 or `libnvbufsurface.so` with `libnvbufsurftransform.so` on JetPack 5 and 6) | proprietary (NVIDIA Jetson Linux components) | - | both | `dlopen` at run time on `aarch64` alone, never linked, never shipped; only present on a board carrying JetPack | the Tegra H.264 encoder. No header is vendored: the interface is declared in `encoders/tegra.rs` and `abi_matches()` refuses a node if this build lays any of it out differently |
| NVIDIA SDK headers (`nvcodec-sys/headers/nvEncodeAPI.h`, NVENCAPI 13.0 from FFmpeg's nv-codec-headers) | MIT | permissive | both | vendored header; the committed `src/bindgen/nvenc.rs` is generated from it | the crate `nvcodec-sys` is MIT OR Apache-2.0 |
| KDE plasma-wayland-protocols XMLs (`protocols/kde-output-device-v2.xml`, `kde-output-management-v2.xml`) | MIT-CMU | permissive | both | vendored protocol descriptions compiled to Rust by `wayland-scanner` at build time; nothing is linked | newer than the copies `wayland-protocols-plasma` bundles, which KWin outgrew (`src/wayland/kdeproto.rs`) |
| CUDA driver API declarations (`nvcodec-sys/src/bindgen/cuda.rs`) | bindgen output of the CUDA toolkit `cuda.h` declarations (the 31 functions pixelflux calls); the toolkit header is under the NVIDIA CUDA Toolkit EULA | - | both | committed FFI declarations only, as cudarc/cuda-sys ship; the toolkit is not needed to build and nothing from it is redistributed | regenerated only with the `regen` feature |
| Linux kernel ABI (`linux-raw-sys`) | Linux-syscall-note (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT for the crate) | permissive | both | syscall numbers and structs as Rust constants | rustix backend |
| CPython (`libpython`) | PSF-2.0 | permissive | both | extension module: symbols come from the hosting interpreter, nothing is linked | `pyo3` with `extension-module` |
| glibc (`libc`, `libm`, `libpthread`, `libdl`) | LGPL-2.1-or-later | weak copyleft | both | linked shared libraries, as for every program; musllinux wheels use musl (MIT) | C runtime |
| libgcc_s, libstdc++ | GPL-3.0-or-later WITH GCC-exception-3.1 | permissive in effect (the runtime library exception covers linked programs) | libgcc_s: both; libstdc++: whenever OpenH264 is compiled in, the only C++ code | linked shared libraries; excluded from the wheel | GCC runtime |
| smithay (git dependency, rev `928d4a9b`) | MIT | permissive | both | Rust source, compiled in | the only non-crates.io crate; `deny.toml` allows exactly that repository |

## Rust crates

The crate graph was resolved with `cargo metadata` (normal dependencies only,
Linux targets) for both configurations: the same 203 crates in the default (GPL)
build and the non-GPL build. Every one of them has a permissive license (MPL-2.0
for `pixelflux` itself); no crate is GPL, LGPL, AGPL, or unlicensed. The two
builds differ in `codec-sys`'s features alone: `x264` and `x265` in the GPL
build, `kvazaar` in the non-GPL one. The OpenH264 crates stay in both, since
their decoder is the virtual camera's H.264 decoder.

License expressions as published by the crates (count of the 203):
MIT OR Apache-2.0 (and spellings of it) 96, MIT 63, MIT OR Apache-2.0 OR Zlib (and spellings of it) 11, Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT 6, BSD-2-Clause 5, BSD-3-Clause 4, Unlicense OR MIT 4, BSD-3-Clause OR Apache-2.0 3, BSD-2-Clause OR Apache-2.0 OR MIT 2, ISC 2, and one each of (MIT OR Apache-2.0) AND Unicode-3.0 (`unicode-ident`), 0BSD OR MIT OR Apache-2.0 (`adler2`), Apache-2.0 (`gethostname`), BSL-1.0 (`xxhash-rust`), CC0-1.0 OR Apache-2.0 (`imgref`), MPL-2.0 (`pixelflux`), Zlib (`zlib-rs`). `scripts/check-licenses.py --markdown` regenerates the full
table below.

<details>
<summary>All 203 crates (Build: both, GPL only, non-GPL only)</summary>

| Crate | Version | License (SPDX) | Category | Build | Native library / note |
| --- | --- | --- | --- | --- | --- |
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | permissive | both |  |
| aligned | 0.4.3 | MIT OR Apache-2.0 | permissive | both |  |
| aligned-vec | 0.6.4 | MIT | permissive | both |  |
| anyhow | 1.0.104 | MIT OR Apache-2.0 | permissive | both |  |
| appendlist | 1.4.0 | MIT | permissive | both |  |
| arg_enum_proc_macro | 0.3.4 | MIT | permissive | both |  |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 | permissive | both |  |
| as-slice | 0.2.1 | MIT OR Apache-2.0 | permissive | both |  |
| async-trait | 0.1.92 | MIT OR Apache-2.0 | permissive | both |  |
| av-scenechange | 0.14.1 | MIT | permissive | both |  |
| av1-grain | 0.2.5 | BSD-2-Clause | permissive | both |  |
| avif-serialize | 0.8.9 | BSD-3-Clause | permissive | both |  |
| base64 | 0.23.1 | MIT OR Apache-2.0 | permissive | both |  |
| bit_field | 0.10.3 | Apache-2.0/MIT | permissive | both |  |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | permissive | both |  |
| bitstream-io | 4.10.0 | MIT/Apache-2.0 | permissive | both |  |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 | permissive | both |  |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT | permissive | both |  |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT | permissive | both |  |
| byteorder-lite | 0.1.0 | Unlicense OR MIT | permissive | both |  |
| calloop | 0.14.5 | MIT | permissive | both |  |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | permissive | both |  |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 | permissive | both |  |
| codec-sys | 0.1.0 | MIT OR Apache-2.0 | permissive | both | libvpx, SVT-AV1, dav1d, and libde265 on every wheel; libx264 and x265 (GPL-2.0-or-later) on the GPL wheel, kvazaar on the non-GPL one (BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media patent license), BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265), weak copyleft) |
| color_quant | 1.1.0 | MIT | permissive | both |  |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 | permissive | both |  |
| crc32fast | 1.5.4 | MIT OR Apache-2.0 | permissive | both |  |
| crossbeam-deque | 0.8.8 | MIT OR Apache-2.0 | permissive | both |  |
| crossbeam-epoch | 0.9.21 | MIT OR Apache-2.0 | permissive | both |  |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 | permissive | both |  |
| cursor-icon | 1.2.0 | MIT OR Apache-2.0 OR Zlib | permissive | both |  |
| dlib | 0.5.3 | MIT | permissive | both |  |
| downcast-rs | 1.2.1 | MIT/Apache-2.0 | permissive | both |  |
| drm | 0.14.1 | MIT | permissive | both |  |
| drm-ffi | 0.9.1 | MIT | permissive | both | Linux DRM ioctls (no library) (MIT, permissive) |
| drm-fourcc | 2.2.0 | MIT | permissive | both |  |
| drm-sys | 0.8.1 | MIT | permissive | both | libdrm headers (bindings only) (MIT, permissive) |
| either | 1.19.0 | MIT OR Apache-2.0 | permissive | both |  |
| endi | 1.1.1 | MIT | permissive | both |  |
| enumflags2 | 0.7.12 | MIT OR Apache-2.0 | permissive | both |  |
| enumflags2_derive | 0.7.12 | MIT OR Apache-2.0 | permissive | both |  |
| equator | 0.4.2 | MIT | permissive | both |  |
| equator-macro | 0.4.2 | MIT | permissive | both |  |
| equivalent | 1.0.3 | Apache-2.0 OR MIT | permissive | both |  |
| errno | 0.3.14 | MIT OR Apache-2.0 | permissive | both |  |
| exr | 1.74.2 | BSD-3-Clause | permissive | both |  |
| fastrand | 2.5.0 | Apache-2.0 OR MIT | permissive | both |  |
| fax | 0.2.7 | MIT | permissive | both |  |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 | permissive | both |  |
| flate2 | 1.1.11 | MIT OR Apache-2.0 | permissive | both |  |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | permissive | both |  |
| futures-io | 0.3.34 | MIT OR Apache-2.0 | permissive | both |  |
| gbm | 0.18.0 | MIT | permissive | both |  |
| gbm-sys | 0.4.0 | MIT | permissive | both | libgbm (Mesa) (MIT, permissive) |
| gcd | 2.3.0 | MIT/Apache-2.0 | permissive | both |  |
| gethostname | 1.1.0 | Apache-2.0 | permissive | both |  |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | permissive | both |  |
| gif | 0.14.2 | MIT OR Apache-2.0 | permissive | both |  |
| glam | 0.33.12 | MIT OR Apache-2.0 | permissive | both |  |
| half | 2.7.1 | MIT OR Apache-2.0 | permissive | both |  |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | permissive | both |  |
| hex | 0.4.3 | MIT OR Apache-2.0 | permissive | both |  |
| image | 0.25.10 | MIT OR Apache-2.0 | permissive | both |  |
| image-webp | 0.2.4 | MIT OR Apache-2.0 | permissive | both |  |
| imgref | 1.12.3 | CC0-1.0 OR Apache-2.0 | permissive | both |  |
| indexmap | 2.14.2 | Apache-2.0 OR MIT | permissive | both |  |
| input | 0.10.0 | MIT | permissive | both |  |
| input-sys | 1.19.0 | MIT | permissive | both | libinput (MIT, permissive) |
| io-lifetimes | 1.0.11 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both |  |
| itertools | 0.14.0 | MIT OR Apache-2.0 | permissive | both |  |
| itoa | 1.0.18 | MIT OR Apache-2.0 | permissive | both |  |
| lebe | 0.5.3 | BSD-3-Clause | permissive | both |  |
| libc | 0.2.191 | MIT OR Apache-2.0 | permissive | both | C runtime (glibc, or musl on musllinux wheels) (LGPL-2.1-or-later (glibc), MIT (musl), weak copyleft) |
| libloading | 0.8.9 | ISC | permissive | both | libEGL.so.1 (Mesa/Khronos, MIT), libpipewire-0.3.so.0 (MIT), libwayland-server.so.0 (MIT), libva.so.2 and libva-drm.so.2 (MIT), libcuda.so.1/libnvidia-encode.so.1/libnvidia-fbc.so.1 (proprietary), and on aarch64 the Jetson Linux libnvv4l2.so/libnvbuf_utils.so/libnvbufsurface.so/libnvbufsurftransform.so (proprietary) (MIT and proprietary driver libraries, permissive) |
| libloading | 0.9.0 | ISC | permissive | both | libEGL.so.1 (Mesa/Khronos, MIT), libpipewire-0.3.so.0 (MIT), libwayland-server.so.0 (MIT), libva.so.2 and libva-drm.so.2 (MIT), libcuda.so.1/libnvidia-encode.so.1/libnvidia-fbc.so.1 (proprietary), and on aarch64 the Jetson Linux libnvv4l2.so/libnvbuf_utils.so/libnvbufsurface.so/libnvbufsurftransform.so (proprietary) (MIT and proprietary driver libraries, permissive) |
| libm | 0.2.16 | MIT | permissive | both |  |
| libudev-sys | 0.1.4 | MIT | permissive | both | libudev (systemd) (LGPL-2.1-or-later, weak copyleft) |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both | Linux kernel ABI (syscall numbers and structs) (Linux-syscall-note, permissive) |
| linux-raw-sys | 0.4.15 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both | Linux kernel ABI (syscall numbers and structs) (Linux-syscall-note, permissive) |
| linux-raw-sys | 0.9.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both | Linux kernel ABI (syscall numbers and structs) (Linux-syscall-note, permissive) |
| log | 0.4.35 | MIT OR Apache-2.0 | permissive | both |  |
| loop9 | 0.1.5 | MIT | permissive | both |  |
| maybe-rayon | 0.1.1 | MIT | permissive | both |  |
| memchr | 2.8.3 | Unlicense OR MIT | permissive | both |  |
| memmap2 | 0.9.12 | MIT OR Apache-2.0 | permissive | both |  |
| memoffset | 0.9.1 | MIT | permissive | both |  |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 | permissive | both |  |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 | permissive | both |  |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 | permissive | both |  |
| new_debug_unreachable | 1.0.6 | MIT | permissive | both |  |
| no_std_io2 | 0.9.4 | Apache-2.0 OR MIT | permissive | both |  |
| nom | 8.0.0 | MIT | permissive | both |  |
| noop_proc_macro | 0.3.0 | MIT | permissive | both |  |
| num-bigint | 0.4.8 | MIT OR Apache-2.0 | permissive | both |  |
| num-complex | 0.4.6 | MIT OR Apache-2.0 | permissive | both |  |
| num-derive | 0.4.2 | MIT OR Apache-2.0 | permissive | both |  |
| num-integer | 0.1.47 | MIT OR Apache-2.0 | permissive | both |  |
| num-rational | 0.4.2 | MIT OR Apache-2.0 | permissive | both |  |
| num-traits | 0.2.19 | MIT OR Apache-2.0 | permissive | both |  |
| nvcodec-sys | 0.1.0 | MIT OR Apache-2.0 | permissive | both | NVIDIA NVENC (libnvidia-encode.so.1), framebuffer capture (libnvidia-fbc.so.1), and CUDA driver (libcuda.so.1) (proprietary driver libraries; nvEncodeAPI.h is MIT, the CUDA bindings are declarations generated from the CUDA toolkit headers, permissive) |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | permissive | both |  |
| openh264 | 0.9.9 | BSD-2-Clause | permissive | both |  |
| openh264-sys2 | 0.9.9 | BSD-2-Clause | permissive | both | Cisco OpenH264 2.6 (vendored source) (BSD-2-Clause, permissive) |
| ordered-stream | 0.2.0 | MIT OR Apache-2.0 | permissive | both |  |
| paste | 1.0.15 | MIT OR Apache-2.0 | permissive | both |  |
| pastey | 0.1.1 | MIT OR Apache-2.0 | permissive | both |  |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | permissive | both |  |
| pixelflux | 0.0.0 | MPL-2.0 | permissive (file-level copyleft) | both |  |
| pixman | 0.2.1 | MIT | permissive | both |  |
| pixman-sys | 0.1.0 | MIT | permissive | both | libpixman-1 (MIT, permissive) |
| png | 0.18.1 | MIT OR Apache-2.0 | permissive | both |  |
| polling | 3.11.0 | Apache-2.0 OR MIT | permissive | both |  |
| portable-atomic | 1.15.0 | Apache-2.0 OR MIT | permissive | both |  |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | permissive | both |  |
| profiling | 1.0.18 | MIT OR Apache-2.0 | permissive | both |  |
| profiling-procmacros | 1.0.18 | MIT OR Apache-2.0 | permissive | both |  |
| pulp | 0.22.3 | MIT | permissive | both |  |
| pulp-wasm-simd-flag | 0.1.1 | MIT | permissive | both |  |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 | permissive | both |  |
| qoi | 0.4.1 | MIT/Apache-2.0 | permissive | both |  |
| quick-error | 2.0.1 | MIT/Apache-2.0 | permissive | both |  |
| quick-xml | 0.41.0 | MIT | permissive | both |  |
| quote | 1.0.47 | MIT OR Apache-2.0 | permissive | both |  |
| rand | 0.10.3 | MIT OR Apache-2.0 | permissive | both |  |
| rand_core | 0.10.1 | MIT OR Apache-2.0 | permissive | both |  |
| rav1e | 0.8.1 | BSD-2-Clause | permissive | both |  |
| ravif | 0.13.0 | BSD-3-Clause | permissive | both |  |
| raw-cpuid | 11.6.0 | MIT | permissive | both |  |
| rayon | 1.12.0 | MIT OR Apache-2.0 | permissive | both |  |
| rayon-core | 1.13.0 | MIT OR Apache-2.0 | permissive | both |  |
| reborrow | 0.5.5 | MIT | permissive | both |  |
| reis | 0.7.1 | MIT | permissive | both |  |
| rgb | 0.8.53 | MIT | permissive | both |  |
| rustix | 0.38.44 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both |  |
| rustix | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | permissive | both |  |
| safe_arch | 1.2.0 | Zlib OR Apache-2.0 OR MIT | permissive | both |  |
| scoped-tls | 1.0.1 | MIT/Apache-2.0 | permissive | both |  |
| serde | 1.0.229 | MIT OR Apache-2.0 | permissive | both |  |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | permissive | both |  |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | permissive | both |  |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | permissive | both |  |
| serde_repr | 0.1.21 | MIT OR Apache-2.0 | permissive | both |  |
| simd-adler32 | 0.3.10 | MIT | permissive | both |  |
| simd_helpers | 0.1.0 | MIT | permissive | both |  |
| siphasher | 1.0.4 | MIT OR Apache-2.0 | permissive | both |  |
| smallvec | 1.16.5 | MIT OR Apache-2.0 | permissive | both |  |
| smithay | 0.7.0 | MIT | permissive | both |  |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | permissive | both |  |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 | permissive | both |  |
| syn | 2.0.119 | MIT OR Apache-2.0 | permissive | both |  |
| syn | 3.0.9 | MIT OR Apache-2.0 | permissive | both |  |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | permissive | both |  |
| thiserror | 1.0.69 | MIT OR Apache-2.0 | permissive | both |  |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | permissive | both |  |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 | permissive | both |  |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 | permissive | both |  |
| tiff | 0.11.3 | MIT | permissive | both |  |
| tracing | 0.1.44 | MIT | permissive | both |  |
| tracing-attributes | 0.1.31 | MIT | permissive | both |  |
| tracing-core | 0.1.36 | MIT | permissive | both |  |
| turbojpeg | 1.5.1 | Unlicense OR MIT | permissive | both |  |
| turbojpeg-sys | 1.2.0 | Unlicense OR MIT | permissive | both | libjpeg-turbo 3.1 (vendored source) (IJG AND BSD-3-Clause AND Zlib, permissive) |
| udev | 0.9.3 | MIT | permissive | both |  |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | permissive | both |  |
| uuid | 1.29.1 | Apache-2.0 OR MIT | permissive | both |  |
| v_frame | 0.3.9 | BSD-2-Clause | permissive | both |  |
| va-sys | 0.1.0 | MIT OR Apache-2.0 | permissive | both | libva (libva.so.2, libva-drm.so.2) (MIT, permissive) |
| wasm-bindgen | 0.2.130 | MIT OR Apache-2.0 | permissive | both |  |
| wasm-bindgen-macro | 0.2.130 | MIT OR Apache-2.0 | permissive | both |  |
| wasm-bindgen-macro-support | 0.2.130 | MIT OR Apache-2.0 | permissive | both |  |
| wasm-bindgen-shared | 0.2.130 | MIT OR Apache-2.0 | permissive | both |  |
| wayland-backend | 0.3.17 | MIT | permissive | both |  |
| wayland-client | 0.31.15 | MIT | permissive | both |  |
| wayland-protocols | 0.32.13 | MIT | permissive | both |  |
| wayland-protocols-misc | 0.3.12 | MIT | permissive | both |  |
| wayland-protocols-plasma | 0.3.12 | MIT | permissive | both |  |
| wayland-protocols-wlr | 0.3.12 | MIT | permissive | both |  |
| wayland-scanner | 0.31.11 | MIT | permissive | both |  |
| wayland-server | 0.31.14 | MIT | permissive | both |  |
| wayland-sys | 0.31.11 | MIT | permissive | both | libwayland-server (MIT, permissive) |
| weezl | 0.1.12 | MIT OR Apache-2.0 | permissive | both |  |
| wide | 1.7.1 | Zlib OR Apache-2.0 OR MIT | permissive | both |  |
| winnow | 1.0.5 | MIT | permissive | both |  |
| x11rb | 0.14.0 | MIT OR Apache-2.0 | permissive | both |  |
| x11rb-protocol | 0.14.0 | MIT OR Apache-2.0 | permissive | both |  |
| xcursor | 0.3.11 | MIT | permissive | both |  |
| xkbcommon | 0.9.0 | MIT | permissive | both | libxkbcommon (MIT, permissive) |
| xkeysym | 0.2.1 | MIT OR Apache-2.0 OR Zlib | permissive | both |  |
| xxhash-rust | 0.8.19 | BSL-1.0 | permissive | both |  |
| y4m | 0.8.0 | MIT | permissive | both |  |
| yuv | 0.8.20 | BSD-3-Clause OR Apache-2.0 | permissive | both |  |
| zbus | 6.2.1 | MIT | permissive | both |  |
| zbus_macros | 6.2.1 | MIT | permissive | both |  |
| zbus_utils | 6.1.0 | MIT | permissive | both |  |
| zcheapstr | 1.2.0 | MIT | permissive | both |  |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | permissive | both |  |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | permissive | both |  |
| zlib-rs | 0.6.8 | Zlib | permissive | both |  |
| zmij | 1.0.23 | MIT | permissive | both |  |
| zruntime | 0.2.2 | MIT | permissive | both |  |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib | permissive | both |  |
| zune-inflate | 0.2.54 | MIT OR Apache-2.0 OR Zlib | permissive | both |  |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib | permissive | both |  |
</details>

## What a non-GPL build contains

`PIXELFLUX_ENABLE_GPL=0` (`--no-default-features --features openh264`):

- the 245 crates above, all permissive (pixelflux itself MPL-2.0), with
  OpenH264 and libjpeg-turbo compiled from vendored BSD/IJG source;
- linked: kvazaar, libvpx, SVT-AV1, and dav1d (BSD), libde265
  (LGPL-3.0-or-later), libgbm, libpixman-1, libxkbcommon (MIT), and the C
  and C++ runtimes (glibc LGPL-2.1-or-later or musl MIT; libgcc_s/libstdc++
  with the GCC runtime exception);
- loaded at run time only when present: libwayland-server, libEGL,
  libpipewire-0.3, libva, and libva-drm (MIT), and the NVIDIA driver's
  libcuda/libnvidia-encode/libnvidia-fbc (proprietary, never shipped);
- no GPL code, whichever distribution's codec libraries a source build links:
  every library the build binds is BSD or LGPL, and a build without the `gpl`
  feature never links x265.

## What the GPL build adds

The default build (`gpl` feature, what the published wheels and
`pip install pixelflux` give you) swaps the software H.264 and H.265 encoders:

- links libx264 (GPL-2.0-or-later) for software H.264 (`codec-sys`'s `x264`
  feature); the GPL wheels bundle `libx264.so`;
- links x265 (GPL-2.0-or-later) for software H.265 in place of kvazaar
  (`codec-sys`'s `x265` feature); the GPL wheels bundle `libx265.so`;
- keeps the OpenH264 crates for the virtual camera's decoder; their encoder
  goes unused;
- the resulting binary is a combination of MPL-2.0, permissive, and GPL code and
  is therefore distributed under the GPL-2.0-or-later terms as a whole
  (MPL-2.0 is GPL-compatible through its secondary-license clause). setup.py
  prints which configuration is being built.

## Distribution notes

- manylinux and musllinux wheels (cibuildwheel, `pyproject.toml`): libvpx,
  SVT-AV1, dav1d, and libde265 — plus x264 and x265 for the GPL wheel,
  kvazaar for the non-GPL one — are built from source in the image;
  auditwheel bundles them (`libx264.so`, `libx265.so`, `libkvazaar.so`,
  `libvpx.so`, `libSvtAv1Enc.so`, `libdav1d.so`, `libde265.so`) into
  `pixelflux.libs/` and leaves libgbm, libEGL, libxkbcommon, libpixman-1,
  libX11/libxcb, zlib, liblzma, and the GCC runtime to the host
  (`repair-wheel-command` excludes). libva is never linked, so the host's
  own copy serves the driver it was built for.
- The wheels carry pixelflux's own LICENSE only. The statically linked OpenH264
  (BSD-2-Clause) and libjpeg-turbo (IJG/BSD-3-Clause/Zlib) and the bundled
  codec libraries' notices are not included in the wheel; this file is the
  inventory, their license texts live in the upstream sources named above.
- Selkies' server packages link the same crate directly; its test suites drive
  the standalone wheel's optional Python bridge.

## How this is enforced

- `scripts/check-licenses.py` resolves both configurations with `cargo
  metadata --locked` (no build, normal dependencies only), classifies every
  crate's SPDX expression, overlays the native libraries behind the binding
  crates (the `NATIVE` table in the script, the source of the component table
  above), and fails when a copyleft component appears in a configuration not
  listed for it in `ALLOWED_COPYLEFT` (`codec-sys` where its `x264` or `x265`
  feature is on, → `gpl` only), when a crate has no usable license
  metadata, or when a crate named like a native binding
  (`-sys`, `_sys`, `-ffi`) is not described in `NATIVE`. Run it from the
  repository root: `python3 scripts/check-licenses.py` (`--set non-gpl`,
  `--markdown`, `--metadata FILE` for a saved `cargo metadata` JSON). It
  reports `PASS`/`FAIL` per configuration and exits non-zero on a failure.
- `deny.toml` is the [cargo-deny](https://embarkstudios.github.io/cargo-deny/)
  policy: its graph is the non-GPL configuration and the allow list is
  permissive-only. The GPL libraries come in through `codec-sys`'s features,
  which crate licenses do not show, so the script above is the gate on them.
  From this directory: `cargo deny --exclude-dev check licenses bans sources`
  (non-GPL graph) and `cargo deny --exclude-dev --features gpl check licenses
  sources` (GPL build). `cargo deny list` ignores the `[graph]`
  section and needs `--no-default-features --features openh264` spelled out.
- The root license workflow inventories the workspace in both configurations;
  the cargo-deny invocations above additionally audit this crate standalone.
- Adding a dependency that links, loads, or vendors native code means adding it
  to `NATIVE` in the script and to the table above; adding a copyleft
  dependency means an `ALLOWED_COPYLEFT` entry, a `deny.toml` clarification
  and exception, and a row here. To exercise the gate itself, save the GPL
  graph (`cargo metadata --format-version 1 --manifest-path Cargo.toml
  > gpl.json`) and audit it under the non-GPL policy
  (`scripts/check-licenses.py --metadata gpl.json --set non-gpl`): it must
  fail on `codec-sys` for its `x264` and `x265` features.

