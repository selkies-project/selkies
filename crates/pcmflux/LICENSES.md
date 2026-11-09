# Third-party licenses

pcmflux itself is licensed under the [Mozilla Public License 2.0](LICENSE).
This file inventories what a built `pcmflux` extension contains or loads and
the license of each piece; `deny.toml` and the root license checks keep
the crate graph to the licenses listed here.

Categories: copyleft (GPL-style: the combined binary must follow the license),
weak copyleft (LGPL: the library stays under its license and must remain
replaceable, dynamic linking is fine), permissive (attribution only). MPL-2.0
is file-level copyleft and is grouped with the permissive licenses. pcmflux has
a single build configuration and contains no copyleft (GPL) component.

## Native libraries

"How used" is what was observed on the built extension (`readelf -d`, `ldd`)
and in the crates' build scripts.

| Component | License | Category | Build | How used | Notes |
| --- | --- | --- | --- | --- | --- |
| libpulse (PulseAudio client library, via `libpulse-sys`) | LGPL-2.1-or-later | weak copyleft | default | linked shared library (`NEEDED libpulse.so.0`), found with pkg-config; the manylinux wheels bundle it with the libraries it pulls in (below) | capture and playback through PulseAudio or PipeWire-Pulse |
| libopus (via `opusic-sys`) | BSD-3-Clause | permissive | default | built from the source the crate vendors, with cmake, and linked statically, so no `libopus.so` is loaded or bundled | Opus encoder, single-stream and multistream; `opusic-sys` itself is BSD-3-Clause |
| CPython (`libpython`) | PSF-2.0 | permissive | default | extension module: symbols come from the hosting interpreter, nothing is linked | `pyo3` with `extension-module` |
| glibc (`libc`, `libm`, `libdl`, `libpthread`) | LGPL-2.1-or-later | weak copyleft | default | linked shared libraries, as for every program; musllinux wheels use musl (MIT) | C runtime |
| libgcc_s | GPL-3.0-or-later WITH GCC-exception-3.1 | permissive in effect | default | linked shared library | GCC runtime |

## Rust crates

Resolved with `cargo metadata` (normal dependencies only): 20 crates, every one
permissive (MPL-2.0 for `pcmflux` itself); no crate is GPL, LGPL, AGPL, or
unlicensed. Build-only dependencies (`cmake`, `pkg-config`, `cc`,
`pyo3-build-config`, and their dependencies) are MIT/Apache-2.0 as well and are
covered by cargo-deny.

| Crate | Version | License (SPDX) | Category | Native library / note |
| --- | --- | --- | --- | --- |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | permissive |  |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT | permissive |  |
| heck | 0.5.0 | MIT OR Apache-2.0 | permissive |  |
| libc | 0.2.191 | MIT OR Apache-2.0 | permissive | C runtime glibc (LGPL-2.1-or-later) or musl (MIT), linked shared library |
| libpulse-binding | 2.30.1 | MIT OR Apache-2.0 | permissive |  |
| libpulse-sys | 1.23.0 | MIT OR Apache-2.0 | permissive | libpulse (PulseAudio client): LGPL-2.1-or-later (weak copyleft), linked shared library |
| num-derive | 0.4.2 | MIT OR Apache-2.0 | permissive |  |
| num-traits | 0.2.19 | MIT OR Apache-2.0 | permissive |  |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | permissive |  |
| opusic-sys | 0.7.6 | BSD-3-Clause | permissive | libopus: BSD-3-Clause (permissive), built from the vendored source with cmake and linked statically |
| pcmflux | 0.0.0 | MPL-2.0 | permissive (file-level copyleft) |  |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | permissive |  |
| pyo3 | 0.30.1 | MIT OR Apache-2.0 | permissive |  |
| pyo3-ffi | 0.30.1 | MIT OR Apache-2.0 | permissive | libpython: PSF-2.0, symbols resolved from the interpreter, not linked |
| pyo3-macros | 0.30.1 | MIT OR Apache-2.0 | permissive |  |
| pyo3-macros-backend | 0.30.1 | MIT OR Apache-2.0 | permissive |  |
| quote | 1.0.47 | MIT OR Apache-2.0 | permissive |  |
| syn | 2.0.119 | MIT OR Apache-2.0 | permissive |  |
| syn | 3.0.9 | MIT OR Apache-2.0 | permissive |  |
| unicode-ident | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | permissive |  |

## What the wheels bundle

- Every wheel (cibuildwheel, `pyproject.toml`): `scripts/build-libpulse.sh`
  builds PulseAudio 17.0's client libraries alone in the wheel image, with a
  libsndfile 1.2.2 built without codecs linked into libpulsecommon, and
  auditwheel bundles libpulse and libpulsecommon into `pcmflux.libs/`, both
  LGPL-2.1-or-later. No D-Bus, systemd, X11 client, or codec library comes
  along.
- musllinux wheels also bundle GNU gettext's libintl (LGPL-2.1-or-later), which
  the image's gettext links PulseAudio's translations through, and libgcc_s
  (GPL-3.0-or-later WITH GCC-exception-3.1), which the manylinux policy leaves
  to the host.
- Nothing in a wheel is GPL-only; the LGPL libraries stay separate `.so` files
  and can be replaced, which is what the LGPL asks for. libopus is in none of
  the bundled libraries: it is compiled from the source `opusic-sys` vendors and
  linked into the extension itself.
- The wheels carry pcmflux's own LICENSE only; this file is the inventory of
  the rest, whose license texts live in the upstream packages named above.

## How this is enforced

`deny.toml` is the [cargo-deny](https://embarkstudios.github.io/cargo-deny/)
policy: the allow list is permissive-only (MIT, Apache-2.0, Apache-2.0 WITH
LLVM-exception, ISC, Zlib, Unicode-3.0, MPL-2.0), so a GPL, LGPL, or unlicensed
crate fails `cargo deny --exclude-dev check licenses bans sources` (run from
this directory). The root license workflow inventories the same graph; this
command audits the standalone package. The native libraries above are outside what crate metadata describes:
a new binding crate means a new row here. `Cargo.toml` declares
`license = "MPL-2.0"`; `deny.toml` audits the rest of the crate graph.
