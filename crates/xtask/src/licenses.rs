/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The license gate for what the Selkies server links and the web client ships.
//!
//! It resolves the crate graph of each build configuration with `cargo metadata` (normal
//! dependencies only, the ones compiled into the executable, over the four Linux targets the
//! releases build), ranks every crate's SPDX license expression ([`spdx`]), and overlays the
//! native libraries the bindings link, load, or vendor, which crate metadata cannot see
//! (`codec-sys` is an MIT binding whose `x264` and `x265` features link the GPL-2.0-or-later
//! libx264 and x265). The npm side reads the license each package of the web packages'
//! lockfiles records, so it needs no `npm ci`. It fails on a copyleft component in a
//! configuration not allowed to carry it, on a crate or package without usable license
//! metadata, on a native binding, a direct dependency, or a vendored crate this file does not
//! describe, and on a `docs/licensing.md` whose generated sections differ from what the
//! graphs yield.
//!
//! ```text
//! cargo xtask licenses                  # audit both configurations and the page
//! cargo xtask licenses --write          # regenerate the page's sections
//! cargo xtask licenses --markdown       # print the sections
//! cargo xtask licenses --set non-gpl    # audit one configuration alone
//! cargo xtask licenses --metadata m.json --set gpl
//! ```
//!
//! The `gpl` configuration is the workspace as committed, pixelflux with its `gpl` feature,
//! which every release ships. `non-gpl` is the build an operator gets by naming pixelflux's
//! `openh264` feature in its place: Selkies has no feature of its own for the switch, so the
//! gate resolves a copy of the workspace manifest with that one feature swapped, beside a
//! copy of the committed `Cargo.lock` and the real `crates/` directory. Cargo drops packages
//! the copied configuration does not select; any package or version the copy gains beyond
//! the committed lockfile fails the check.
//!
//! The page sections sit between `check-licenses:begin` and `check-licenses:end` markers.
//! They name crates and licenses, never counts or versions beside a vendored crate's, and
//! name a crate of the wider graph only where MIT or Apache-2.0 alone is not among its
//! choices, so a lockfile bump changes the page only where a license changes, or a direct
//! dependency, a native binding, or a crate under other terms comes or goes. A page check
//! needs both configurations resolved by cargo, so `--set` and `--metadata` audit the graph
//! alone.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::{NoExpand, Regex};
use serde_json::Value;

use crate::repo;
use spdx::{Expr, Rank};

mod npm;
mod spdx;

const PAGE: &str = "docs/licensing.md";

/// The Linux targets the releases build: the glibc and musl executables and wheels on x86_64
/// and aarch64. The graph is their union, so the page reads the same on any host.
const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];
const SETS: [&str; 2] = ["gpl", "non-gpl"];
/// The workspace crates whose executables are shipped: the server, and the TURN REST service
/// of the `turn-rest` image.
const SHIPPED: [&str; 2] = ["selkies", "selkies-turn-rest"];

/// Copyleft components and the configurations allowed to contain them. Anything copyleft (or
/// LGPL, which a statically linked crate cannot honor) that is not listed here fails in every
/// configuration, so a GPL dependency is added here and in `docs/licensing.md` on purpose.
const ALLOWED_COPYLEFT: &[(&str, &[&str])] = &[("codec-sys", &["gpl"])];

/// A native library behind a crate that binds, vendors, or loads it.
struct Native {
    krate: &'static str,
    library: &'static str,
    license: &'static str,
    /// The category of the native code.
    rank: Rank,
    how: &'static str,
    /// The crate features that link a copyleft library in its place.
    copyleft_features: &'static [&'static str],
}

/// What a group of the workspace's direct dependencies does in the server.
struct Role {
    crates: &'static [&'static str],
    how: &'static str,
    notes: &'static str,
}

// NATIVE: every crate named like a native binding (-sys, _sys, -ffi) has to be described, and
// every entry has to be in a graph, so a new binding fails the check until it is inventoried
// and a dropped one until it is taken off the page.
// ROLES: every direct dependency of the server crates, in the order the page lists them; a new
// one fails the check until it is placed.
// VENDORED: the crates vendored under crates/*/vendor and what Selkies does with each.
const NATIVE: &[Native] = &[
    Native {
        krate: "codec-sys",
        library: "libvpx, SVT-AV1, dav1d, and libde265 in every build; libx264 and x265 in the GPL \
                  build, kvazaar in the non-GPL one",
        license: "BSD-3-Clause (libvpx, kvazaar; SVT-AV1 with the Alliance for Open Media patent \
                  license), BSD-2-Clause (dav1d), LGPL-3.0-or-later (libde265), GPL-2.0-or-later \
                  (libx264, x265)",
        rank: Rank::Weak,
        how: "linked shared libraries, through pixelflux; the software H.264 (GPL build), HEVC, \
              VP8, VP9, and AV1 encoders and the virtual camera's decoders",
        copyleft_features: &["x264", "x265"],
    },
    Native {
        krate: "openh264-sys2",
        library: "Cisco OpenH264, and libstdc++ for its C++",
        license: "BSD-2-Clause (OpenH264), GPL-3.0-or-later WITH GCC-exception-3.1 (libstdc++)",
        rank: Rank::Permissive,
        how: "compiled from the source vendored in the crate and linked statically, through \
              pixelflux, libstdc++ as a shared library; full-frame software H.264 of the non-GPL \
              build and the virtual camera's H.264 decoder",
        copyleft_features: &[],
    },
    Native {
        krate: "turbojpeg-sys",
        library: "libjpeg-turbo",
        license: "IJG AND BSD-3-Clause AND Zlib",
        rank: Rank::Permissive,
        how: "compiled from the source vendored in the crate and linked statically, through \
              pixelflux; JPEG",
        copyleft_features: &[],
    },
    Native {
        krate: "va-sys",
        library: "libva, libva-drm",
        license: "MIT",
        rank: Rank::Permissive,
        how: "`dlopen` at run time through pixelflux, never linked; VA-API encoding",
        copyleft_features: &[],
    },
    Native {
        krate: "nvcodec-sys",
        library: "NVENC, NvFBC, and the CUDA driver (`libnvidia-encode.so.1`, \
                  `libnvidia-fbc.so.1`, `libcuda.so.1`)",
        license: "proprietary (NVIDIA driver); the vendored headers MIT",
        rank: Rank::Permissive,
        how: "`dlopen` at run time through pixelflux, never linked or shipped",
        copyleft_features: &[],
    },
    Native {
        krate: "gbm-sys",
        library: "libgbm (Mesa)",
        license: "MIT",
        rank: Rank::Permissive,
        how: "linked shared library, through pixelflux",
        copyleft_features: &[],
    },
    Native {
        krate: "pixman-sys",
        library: "libpixman-1",
        license: "MIT",
        rank: Rank::Permissive,
        how: "linked shared library, through pixelflux's capture compositor",
        copyleft_features: &[],
    },
    Native {
        krate: "drm-sys",
        library: "libdrm headers",
        license: "MIT",
        rank: Rank::Permissive,
        how: "bindings generated from the headers, through pixelflux; no libdrm symbol is linked",
        copyleft_features: &[],
    },
    Native {
        krate: "drm-ffi",
        library: "Linux DRM ioctls (no library)",
        license: "MIT",
        rank: Rank::Permissive,
        how: "pure Rust ioctl wrappers, through pixelflux",
        copyleft_features: &[],
    },
    Native {
        krate: "wayland-sys",
        library: "libwayland-server",
        license: "MIT",
        rank: Rank::Permissive,
        how: "`dlopen` at run time through pixelflux's capture compositor, never linked",
        copyleft_features: &[],
    },
    Native {
        krate: "input-sys",
        library: "libinput",
        license: "MIT",
        rank: Rank::Permissive,
        how: "in the graph through smithay's libinput backend, which pixelflux does not use; \
              nothing is linked",
        copyleft_features: &[],
    },
    Native {
        krate: "libudev-sys",
        library: "libudev (systemd)",
        license: "LGPL-2.1-or-later",
        rank: Rank::Weak,
        how: "in the graph through smithay's udev backend, which pixelflux does not use; nothing \
              is linked",
        copyleft_features: &[],
    },
    Native {
        krate: "xkbcommon",
        library: "libxkbcommon",
        license: "MIT",
        rank: Rank::Permissive,
        how: "linked shared library; keymaps, in the server and in pixelflux",
        copyleft_features: &[],
    },
    Native {
        krate: "libpulse-sys",
        library: "libpulse",
        license: "LGPL-2.1-or-later",
        rank: Rank::Weak,
        how: "linked shared library, replaceable; the sound-server control plane \
              (`libpulse-binding`) and pcmflux's capture",
        copyleft_features: &[],
    },
    Native {
        krate: "opusic-sys",
        library: "libopus",
        license: "BSD-3-Clause",
        rank: Rank::Permissive,
        how: "compiled from the source bundled in the crate and linked statically, through \
              pcmflux; Opus",
        copyleft_features: &[],
    },
    Native {
        krate: "aws-lc-sys",
        library: "AWS-LC",
        license: "ISC AND Apache-2.0 AND MIT AND BSD-3-Clause",
        rank: Rank::Permissive,
        how: "compiled from the source vendored in the crate and linked statically; the HTTPS \
              server's and outgoing requests' cryptography (`aws-lc-rs`)",
        copyleft_features: &[],
    },
    Native {
        krate: "nvml-wrapper-sys",
        library: "NVML (`libnvidia-ml.so.1`)",
        license: "proprietary (NVIDIA driver)",
        rank: Rank::Permissive,
        how: "`dlopen` at run time when present, never linked or shipped; GPU statistics",
        copyleft_features: &[],
    },
    Native {
        krate: "linux-raw-sys",
        library: "Linux kernel ABI (no library)",
        license: "GPL-2.0-only WITH Linux-syscall-note (the kernel's UAPI headers)",
        rank: Rank::Permissive,
        how: "syscall numbers and structures as Rust constants (rustix)",
        copyleft_features: &[],
    },
    Native {
        krate: "libc",
        library: "C runtime: glibc and libgcc_s, or musl in the musl builds",
        license: "LGPL-2.1-or-later (glibc), GPL-3.0-or-later WITH GCC-exception-3.1 (libgcc_s), \
                  MIT (musl)",
        rank: Rank::Weak,
        how: "linked shared libraries (glibc, libgcc_s) or statically (musl), as for every program",
        copyleft_features: &[],
    },
    Native {
        krate: "libloading",
        library: "the driver and desktop libraries above that are opened at run time",
        license: "MIT and proprietary",
        rank: Rank::Permissive,
        how: "`dlopen`, never linked",
        copyleft_features: &[],
    },
];

const ROLES: &[Role] = &[
    Role {
        crates: &["pixelflux"],
        how: "screen capture and video encoding, over the native libraries below",
        notes: "see [pixelflux's \
                LICENSES.md](https://github.com/selkies-project/selkies/blob/main/crates/pixelflux/LICENSES.md)",
    },
    Role {
        crates: &["pcmflux"],
        how: "audio capture and Opus encoding, over libpulse and libopus",
        notes: "see [pcmflux's \
                LICENSES.md](https://github.com/selkies-project/selkies/blob/main/crates/pcmflux/LICENSES.md)",
    },
    Role {
        crates: &["selkies-rtc"],
        how: "the WebRTC stack, in this repository under `crates/selkies-rtc`",
        notes: "",
    },
    Role {
        crates: &["selkies-wm"],
        how: "the window manager and desktop session `--wm` and `--desktop` run, in this \
              repository under `crates/selkies-wm`",
        notes: "",
    },
    Role {
        crates: &["selkies-draw"],
        how: "the desktop entries the window manager's menus launch, in this repository under \
              `crates/selkies-draw`",
        notes: "",
    },
    Role {
        crates: &["pelorus"],
        how: "the desktop described as text for `--pelorus`, over the accessibility bus, in this \
              repository under `crates/pelorus`",
        notes: "",
    },
    Role {
        crates: &[
            "rtc-crypto",
            "rtc-dtls",
            "rtc-ice",
            "rtc-mdns",
            "rtc-sctp",
            "rtc-sdp",
            "rtc-shared",
            "rtc-srtp",
            "rtc-stun",
            "rtc-turn",
            "sansio",
        ],
        how: "the [webrtc-rs](https://github.com/webrtc-rs/rtc) protocol crates `selkies-rtc` is \
              built over",
        notes: "`rtc-dtls`, `rtc-sctp`, and `rtc-srtp` vendored (below)",
    },
    Role {
        crates: &["tokio", "tokio-util", "futures", "async-trait", "bytes"],
        how: "async runtime",
        notes: "",
    },
    Role {
        crates: &[
            "axum",
            "tokio-websockets",
            "hyper",
            "hyper-util",
            "tower",
            "tower-http",
            "http",
            "http-body-util",
        ],
        how: "HTTP and WebSocket server",
        notes: "",
    },
    Role {
        crates: &[
            "rustls",
            "tokio-rustls",
            "hyper-rustls",
            "rustls-native-certs",
        ],
        how: "HTTPS, TLS to TURN servers, and the outgoing requests of the TURN REST and \
              Cloudflare sources",
        notes: "cryptography from `aws-lc-rs`",
    },
    Role {
        crates: &["rcgen"],
        how: "self-signed certificates",
        notes: "",
    },
    Role {
        crates: &["aws-lc-rs", "rand"],
        how: "hashes, HMAC credentials, constant-time comparison, random identifiers",
        notes: "",
    },
    Role {
        crates: &["x11rb"],
        how: "X11 input injection (XTEST), the clipboard (XFixes), and the display's outputs \
              (RandR)",
        notes: "speaks the X protocol itself, no libxcb",
    },
    Role {
        crates: &["xkbcommon"],
        how: "keymaps",
        notes: "",
    },
    Role {
        crates: &["libpulse-binding"],
        how: "the sound-server control plane",
        notes: "",
    },
    Role {
        crates: &["nvml-wrapper"],
        how: "GPU statistics",
        notes: "",
    },
    Role {
        crates: &["sysinfo"],
        how: "system and process statistics",
        notes: "",
    },
    Role {
        crates: &["prometheus-client"],
        how: "metrics endpoint",
        notes: "",
    },
    Role {
        crates: &["image"],
        how: "clipboard images and cursors",
        notes: "",
    },
    Role {
        crates: &["zip", "flate2"],
        how: "folder downloads, compressed messages",
        notes: "",
    },
    Role {
        crates: &[
            "serde",
            "serde_json",
            "rmp-serde",
            "base64",
            "percent-encoding",
            "url",
        ],
        how: "JSON messages, MessagePack to the uinput mouse socket, encodings",
        notes: "",
    },
    Role {
        crates: &["libc", "nix", "socket2"],
        how: "system calls: sockets, descriptors, signals, processes",
        notes: "",
    },
    Role {
        crates: &[
            "log",
            "thiserror",
            "parking_lot",
            "regex",
            "memchr",
            "mime_guess",
            "chrono",
            "shlex",
            "tempfile",
        ],
        how: "logging, errors, locks, text, MIME types, time, and temporary files",
        notes: "",
    },
];

const VENDORED: &[(&str, &str)] = &[
    (
        "rtc-dtls",
        "the workspace patches the published crate with it; `rtc-dtls-selkies.patch` beside it is \
         the diff, the fix and test of webrtc-rs/rtc#288 under the crate's own license and the \
         Selkies code-scanning fixes, which are MPL-2.0",
    ),
    (
        "rtc-sctp",
        "the workspace patches the published crate with it; `rtc-sctp-selkies.patch` beside it is \
         the diff, whose changes are MPL-2.0",
    ),
    (
        "rtc-srtp",
        "the workspace patches the published crate with it; `rtc-srtp-selkies.patch` beside it is \
         the diff, whose changes are MPL-2.0",
    ),
];

static NATIVE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(-|_)sys\d*$|-ffi$").expect("a valid pattern"));

fn native(name: &str) -> Option<&'static Native> {
    NATIVE.iter().find(|native| native.krate == name)
}

fn copyleft_allowed(name: &str, set: &str) -> bool {
    ALLOWED_COPYLEFT
        .iter()
        .any(|(krate, sets)| *krate == name && sets.contains(&set))
}

fn set_label(set: &str) -> &'static str {
    if set == "gpl" { "GPL" } else { "non-GPL" }
}

/// One crate of a configuration's graph, with its license as parsed.
struct Crate {
    name: String,
    version: String,
    license: String,
    expr: Option<Expr>,
    rank: Rank,
    native_rank: Rank,
    manifest: String,
}

/// The graphs audited, by configuration in the order they were asked for.
type Graphs = Vec<(&'static str, Vec<Crate>)>;

/// Resolve the graph of `manifest` over the release targets from the lockfile beside it.
fn cargo_metadata(
    manifest: &Path,
    locked: bool,
    no_default_features: bool,
) -> Result<Value, String> {
    let mut cargo = Command::new("cargo");
    cargo
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest);
    if locked {
        cargo.arg("--locked");
    }
    if no_default_features {
        cargo.arg("--no-default-features");
    }
    for target in TARGETS {
        cargo.args(["--filter-platform", target]);
    }
    let output = cargo.output().map_err(|e| format!("cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

/// The workspace manifest with pixelflux's `gpl` feature replaced by `openh264`.
///
/// Fails when the pixelflux dependency is not one line naming `gpl` among its features, so a
/// reshaped manifest cannot leave the non-GPL audit resolving the GPL graph.
fn non_gpl_manifest(manifest: &str) -> Result<String, String> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^(pixelflux\s*=\s*\{.*\bfeatures\s*=\s*\[[^\]]*)"gpl"([^\]]*\].*)$"#)
            .expect("a valid pattern")
    });
    if LINE.find_iter(manifest).count() != 1 {
        return Err("Cargo.toml: expected one `pixelflux = { ... features = [... \"gpl\" ...] }` line to swap \
                    for the non-GPL configuration"
            .into());
    }
    Ok(LINE.replace(manifest, "${1}\"openh264\"${2}").into_owned())
}

/// The name, version, and source of every package a `Cargo.lock` pins.
fn lock_packages(path: &Path) -> Result<BTreeSet<(String, String, String)>, String> {
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^(name|version|source) = "([^"]*)"$"#).expect("a valid pattern")
    });
    let lock = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(lock
        .split("[[package]]")
        .skip(1)
        .map(|block| {
            let fields: HashMap<&str, &str> = FIELD
                .captures_iter(block)
                .map(|field| {
                    let (_, [key, value]) = field.extract();
                    (key, value)
                })
                .collect();
            let field = |name: &str| fields.get(name).copied().unwrap_or_default().to_string();
            (field("name"), field("version"), field("source"))
        })
        .collect())
}

/// A scratch directory that is removed when it goes out of scope.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Resolve one configuration's graph with cargo.
fn resolve(set: &str) -> Result<Value, String> {
    let repo = repo();
    if set == "gpl" {
        return cargo_metadata(&repo.join("Cargo.toml"), true, false);
    }
    let manifest =
        fs::read_to_string(repo.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    let swapped = non_gpl_manifest(&manifest)?;
    let scratch =
        Scratch(std::env::temp_dir().join(format!("selkies-licenses-{}", std::process::id())));
    let io = |e: std::io::Error| format!("{}: {e}", scratch.0.display());
    fs::create_dir(&scratch.0).map_err(io)?;
    fs::write(scratch.0.join("Cargo.toml"), swapped).map_err(io)?;
    fs::copy(repo.join("Cargo.lock"), scratch.0.join("Cargo.lock")).map_err(io)?;
    std::os::unix::fs::symlink(repo.join("crates"), scratch.0.join("crates")).map_err(io)?;
    let meta = cargo_metadata(&scratch.0.join("Cargo.toml"), false, true)?;
    let unpinned: Vec<String> = lock_packages(&scratch.0.join("Cargo.lock"))?
        .difference(&lock_packages(&repo.join("Cargo.lock"))?)
        .map(|(name, version, _)| format!("{name} {version}"))
        .collect();
    let root = meta["workspace_root"].as_str().unwrap_or_default();
    if fs::canonicalize(root).ok() == fs::canonicalize(&repo).ok() {
        return Err("cargo resolved the committed workspace in place of the non-GPL copy".into());
    }
    if !unpinned.is_empty() {
        return Err(format!(
            "the non-GPL configuration resolves packages Cargo.lock does not pin: {}",
            unpinned.join(", ")
        ));
    }
    Ok(meta)
}

/// The packages and resolve nodes of a `cargo metadata` document, by package id.
struct Graph<'a> {
    packages: HashMap<&'a str, &'a Value>,
    nodes: HashMap<&'a str, &'a Value>,
    members: Vec<&'a str>,
}

impl<'a> Graph<'a> {
    fn new(meta: &'a Value) -> Self {
        let by_id = |list: &'a Value| -> HashMap<&'a str, &'a Value> {
            list.as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| Some((item["id"].as_str()?, item)))
                .collect()
        };
        Self {
            packages: by_id(&meta["packages"]),
            nodes: by_id(&meta["resolve"]["nodes"]),
            members: meta["workspace_members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect(),
        }
    }

    fn name(&self, id: &str) -> &'a str {
        self.packages
            .get(id)
            .and_then(|package| package["name"].as_str())
            .unwrap_or_default()
    }

    /// The ids `id` depends on as normal dependencies, the ones compiled into it.
    fn normal_deps(&self, id: &str) -> Vec<&'a str> {
        let deps = self.nodes.get(id).and_then(|node| node["deps"].as_array());
        deps.into_iter()
            .flatten()
            .filter(|dep| {
                dep["dep_kinds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|kind| kind["kind"].is_null())
            })
            .filter_map(|dep| dep["pkg"].as_str())
            .collect()
    }

    /// The packages the shipped executables link, without feature-unifying other workspace
    /// members.
    fn normal_closure(&self) -> Vec<&'a Value> {
        let mut seen: HashSet<&str> = self
            .members
            .iter()
            .copied()
            .filter(|id| SHIPPED.contains(&self.name(id)))
            .collect();
        let mut stack: Vec<&str> = seen.iter().copied().collect();
        while let Some(id) = stack.pop() {
            for dep in self.normal_deps(id) {
                if seen.insert(dep) {
                    stack.push(dep);
                }
            }
        }
        let mut closure: Vec<&Value> = seen
            .iter()
            .filter_map(|id| self.packages.get(id).copied())
            .collect();
        closure.sort_by_key(|package| (package["name"].as_str(), package["version"].as_str()));
        closure
    }

    /// The server crates' normal dependencies, whose roles the page describes.
    fn direct_dependencies(&self) -> BTreeSet<String> {
        self.members
            .iter()
            .filter(|id| matches!(self.name(id), "selkies" | "selkies-rtc"))
            .flat_map(|id| self.normal_deps(id))
            .map(|dep| self.name(dep).to_string())
            .collect()
    }
}

/// Rank every crate of one configuration and collect the violations.
fn audit(meta: &Value, set: &str) -> (Vec<Crate>, Vec<String>) {
    let graph = Graph::new(meta);
    let mut crates = Vec::new();
    let mut problems = Vec::new();
    for package in graph.normal_closure() {
        let text = |key: &str| package[key].as_str().unwrap_or_default();
        let (name, version) = (text("name"), text("version"));
        let mut published = text("license").to_string();
        let mut expr = None;
        let mut rank = Rank::Unknown;
        if published.is_empty() {
            published = match text("license_file") {
                "" => "none".into(),
                file => format!("license-file only: {file}"),
            };
            problems.push(format!(
                "{name} {version}: no machine-readable license ({published})"
            ));
        } else {
            match spdx::parse(&published) {
                Ok(parsed) => {
                    rank = parsed.rank();
                    expr = Some(parsed);
                }
                Err(error) => problems.push(format!("{name} {version}: {error}")),
            }
            if rank == Rank::Unknown && expr.is_some() {
                problems.push(format!(
                    "{name} {version}: license '{published}' is not classified"
                ));
            } else if matches!(rank, Rank::Weak | Rank::Copyleft) && !copyleft_allowed(name, set) {
                problems.push(format!(
                    "{name} {version}: {} crate license '{published}' in the {set} set",
                    rank.label()
                ));
            }
        }
        let mut native_rank = Rank::Permissive;
        if let Some(native) = native(name) {
            native_rank = native.rank;
            let features = graph
                .nodes
                .get(text("id"))
                .and_then(|node| node["features"].as_array());
            if features
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|f| native.copyleft_features.contains(&f))
            {
                native_rank = Rank::Copyleft;
            }
            if native_rank == Rank::Copyleft && !copyleft_allowed(name, set) {
                problems.push(format!(
                    "{name} {version}: links {} ({}) in the {set} set",
                    native.library, native.license
                ));
            }
        } else if NATIVE_NAME.is_match(name) {
            problems.push(format!(
                "{name} {version}: native binding not described in NATIVE (add it there, then --write)"
            ));
        }
        crates.push(Crate {
            name: name.into(),
            version: version.into(),
            license: published,
            expr,
            rank,
            native_rank,
            manifest: text("manifest_path").into(),
        });
    }
    (crates, problems)
}

/// The aligned per-crate listing printed for one configuration.
fn listing(crates: &[Crate], set: &str) -> String {
    let worst = crates
        .iter()
        .flat_map(|c| [c.rank, c.native_rank])
        .max()
        .unwrap_or(Rank::Permissive);
    let mut lines = vec![format!(
        "{set} set: {} crates, worst category {}",
        crates.len(),
        worst.label()
    )];
    for c in crates {
        let mut line = format!(
            "{:<28} {:<12} {:<50} {}",
            c.name,
            c.version,
            c.license,
            c.rank.label()
        );
        if let Some(native) = native(&c.name) {
            line += &format!("  [native: {}, {}]", native.library, c.native_rank.label());
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Items joined as an English list, with the serial comma.
fn series(items: &[String]) -> String {
    match items {
        [head @ .., last] if items.len() >= 3 => format!("{}, and {last}", head.join(", ")),
        _ => items.join(" and "),
    }
}

/// A Markdown table, the pipes of its cells escaped.
fn table(head: &[&str], rows: &[Vec<String>]) -> String {
    let mut lines = vec![
        format!("| {} |", head.join(" | ")),
        format!("| {} |", vec!["---"; head.len()].join(" | ")),
    ];
    for row in rows {
        let cells: Vec<String> = row.iter().map(|cell| cell.replace('|', "\\|")).collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

/// Which configurations contain a crate, for a table's Build column.
fn build_label(present: &BTreeSet<&str>) -> String {
    if SETS.iter().all(|set| present.contains(set)) {
        return "both".into();
    }
    let labels: Vec<&str> = SETS
        .iter()
        .filter(|set| present.contains(*set))
        .map(|set| set_label(set))
        .collect();
    format!("{} only", labels.join(" and "))
}

/// The distinct expressions of several crates in first-seen order, and their worst rank.
fn licenses_of<'a>(crates: impl IntoIterator<Item = &'a Crate>) -> (String, Rank) {
    let mut seen: Vec<String> = Vec::new();
    let mut worst = Rank::Permissive;
    for c in crates {
        let text = c
            .expr
            .as_ref()
            .map_or_else(|| c.license.clone(), Expr::render);
        if !seen.contains(&text) {
            seen.push(text);
        }
        worst = worst.max(c.rank);
    }
    (seen.join("; "), worst)
}

fn graph<'a>(graphs: &'a Graphs, set: &str) -> &'a [Crate] {
    graphs
        .iter()
        .find(|(name, _)| *name == set)
        .map_or(&[], |(_, crates)| crates)
}

/// The table of the workspace's direct dependencies and what each does.
fn render_rust_direct(
    graphs: &Graphs,
    problems: &mut Vec<String>,
    direct: &BTreeSet<String>,
) -> String {
    let placed: Vec<&str> = ROLES.iter().flat_map(|role| role.crates).copied().collect();
    for name in direct
        .iter()
        .filter(|name| !placed.contains(&name.as_str()))
    {
        problems.push(format!(
            "{name}: a direct dependency ROLES does not describe (add it there, then --write)"
        ));
    }
    let placed_set: BTreeSet<&str> = placed.iter().copied().collect();
    for name in placed_set.iter().filter(|name| !direct.contains(**name)) {
        problems.push(format!(
            "{name}: ROLES describes it, but no workspace crate depends on it"
        ));
    }
    for name in placed_set
        .iter()
        .filter(|name| placed.iter().filter(|other| other == name).count() > 1)
    {
        problems.push(format!("{name}: placed in ROLES twice"));
    }
    let rows: Vec<Vec<String>> = ROLES
        .iter()
        .map(|role| {
            let members = role
                .crates
                .iter()
                .flat_map(|name| graph(graphs, "gpl").iter().filter(move |c| c.name == *name));
            let (license, worst) = licenses_of(members);
            let names: Vec<String> = role.crates.iter().map(|name| format!("`{name}`")).collect();
            vec![
                names.join(", "),
                license,
                worst.label().into(),
                role.how.into(),
                role.notes.into(),
            ]
        })
        .collect();
    table(
        &["Crate", "License", "Category", "How used", "Notes"],
        &rows,
    )
}

/// The table of the native libraries behind the graph's bindings.
fn render_native(graphs: &Graphs, problems: &mut Vec<String>) -> String {
    let mut rows = Vec::new();
    for native in NATIVE {
        let present: BTreeSet<&str> = graphs
            .iter()
            .filter(|(_, crates)| crates.iter().any(|c| c.name == native.krate))
            .map(|(set, _)| *set)
            .collect();
        if present.is_empty() {
            problems.push(format!(
                "{}: NATIVE describes it, but neither configuration's graph contains it",
                native.krate
            ));
            continue;
        }
        let mut category = native.rank.label().to_string();
        if !native.copyleft_features.is_empty() {
            category += " (copyleft in the GPL build)";
        }
        rows.push(vec![
            native.library.into(),
            format!("`{}`", native.krate),
            native.license.into(),
            category,
            build_label(&present),
            native.how.into(),
        ]);
    }
    table(
        &[
            "Native library",
            "Crate",
            "License",
            "Category",
            "Build",
            "How used",
        ],
        &rows,
    )
}

/// The table of the crates vendored into this repository.
fn render_vendored(graphs: &Graphs, problems: &mut Vec<String>) -> String {
    let repo = fs::canonicalize(repo()).unwrap_or_else(|_| repo());
    let mut found: BTreeMap<&str, (&Crate, PathBuf)> = BTreeMap::new();
    for c in graphs.iter().flat_map(|(_, crates)| crates) {
        let Ok(manifest) = fs::canonicalize(&c.manifest) else {
            continue;
        };
        if manifest.starts_with(repo.join("crates"))
            && manifest
                .components()
                .any(|part| part.as_os_str() == "vendor")
        {
            found.insert(&c.name, (c, manifest));
        }
    }
    let names: BTreeSet<&str> = found
        .keys()
        .copied()
        .chain(VENDORED.iter().map(|(name, _)| *name))
        .collect();
    let mut rows = Vec::new();
    for name in names {
        let Some((c, manifest)) = found.get(name) else {
            problems.push(format!(
                "{name}: VENDORED describes it, but no vendored crate of that name is in the graph"
            ));
            continue;
        };
        let Some((_, notes)) = VENDORED.iter().find(|(vendored, _)| *vendored == name) else {
            problems.push(format!(
                "{name}: a vendored crate VENDORED does not describe (add it there, then --write)"
            ));
            continue;
        };
        let place = manifest
            .parent()
            .and_then(|dir| dir.strip_prefix(&repo).ok())
            .unwrap_or(manifest);
        let (license, rank) = licenses_of([*c]);
        rows.push(vec![
            format!("`{name}`"),
            c.version.clone(),
            format!("`{}`", place.display()),
            license,
            rank.label().into(),
            (*notes).into(),
        ]);
    }
    table(
        &["Crate", "Version", "Where", "License", "Category", "Notes"],
        &rows,
    )
}

/// Every license expression in the graphs, naming the crates that do not offer MIT or
/// Apache-2.0 alone.
fn render_rust_other(graphs: &Graphs) -> String {
    let mut present: BTreeMap<(String, &str), BTreeSet<&str>> = BTreeMap::new();
    let mut exprs: BTreeMap<String, (&Expr, Rank)> = BTreeMap::new();
    for (set, crates) in graphs {
        for c in crates {
            let Some(expr) = &c.expr else { continue };
            exprs.insert(expr.render(), (expr, c.rank));
            present
                .entry((expr.render(), &c.name))
                .or_default()
                .insert(set);
        }
    }
    let common: Vec<String> = exprs
        .iter()
        .filter(|(_, (expr, _))| expr.offers_default_terms())
        .map(|(text, _)| format!("`{text}`"))
        .collect();
    let mut others: Vec<(&String, Rank)> = exprs
        .iter()
        .filter(|(_, (expr, _))| !expr.offers_default_terms())
        .map(|(text, (_, rank))| (text, *rank))
        .collect();
    others.sort_by_key(|(text, rank)| (*rank, *text));
    let rows: Vec<Vec<String>> = others
        .into_iter()
        .map(|(text, rank)| {
            let names: Vec<String> = present
                .iter()
                .filter(|((expr, _), _)| expr == text)
                .map(|((_, name), sets)| match build_label(sets).as_str() {
                    "both" => format!("`{name}`"),
                    label => format!("`{name}` ({label})"),
                })
                .collect();
            vec![text.clone(), rank.label().into(), names.join(", ")]
        })
        .collect();
    format!(
        "Most crates in either graph, the dependencies of pixelflux and pcmflux included, can be taken under \
         MIT or under Apache-2.0 alone; the expressions they publish, spellings of one expression merged, are \
         {}. Every other crate, by expression:\n\n{}",
        series(&common),
        table(&["License", "Category", "Crates"], &rows)
    )
}

/// The generated sections of the page, by marker name in the page's order of generation.
fn page_sections(
    graphs: &Graphs,
    direct: &BTreeSet<String>,
    problems: &mut Vec<String>,
) -> Result<Vec<(&'static str, String)>, String> {
    let (npm_direct, npm_scopes) = npm::audit(problems)?;
    Ok(vec![
        (
            "rust-dependencies",
            render_rust_direct(graphs, problems, direct),
        ),
        ("rust-licenses", render_rust_other(graphs)),
        ("native-libraries", render_native(graphs, problems)),
        ("vendored-crates", render_vendored(graphs, problems)),
        ("npm-dependencies", npm_direct),
        ("npm-licenses", npm_scopes),
    ])
}

/// The HTML comment that opens or closes a generated section of the page.
fn marker(name: &str, end: bool) -> String {
    if end {
        return format!("<!-- check-licenses:end {name} -->");
    }
    format!(
        "<!-- check-licenses:begin {name}: generated by `cargo xtask licenses`; rerun it with --write rather \
         than editing this section by hand -->"
    )
}

/// A section between its markers, as the page carries it.
fn framed(name: &str, body: &str) -> String {
    format!(
        "{}\n\n{body}\n\n{}",
        marker(name, false),
        marker(name, true)
    )
}

/// The page with each generated section replaced, and the names of the sections it lacks.
fn splice(page: &str, sections: &[(&'static str, String)]) -> (String, Vec<&'static str>) {
    let mut page = page.to_string();
    let mut missing = Vec::new();
    for (name, body) in sections {
        let pattern = format!(
            "(?s){}.*?{}",
            regex::escape(&format!("<!-- check-licenses:begin {name}")),
            regex::escape(&marker(name, true))
        );
        let pattern = Regex::new(&pattern).expect("an escaped pattern");
        if pattern.find_iter(&page).count() != 1 {
            missing.push(*name);
        }
        page = pattern
            .replace_all(&page, NoExpand(&framed(name, body)))
            .into_owned();
    }
    (page, missing)
}

/// The first line at which two texts differ, for a page that is stale.
fn first_difference(current: &str, rendered: &str) -> String {
    let mut lines = current.lines().zip(rendered.lines()).enumerate();
    match lines.find(|(_, (old, new))| old != new) {
        Some((at, (old, new))) => format!("line {}:\n-{old}\n+{new}", at + 1),
        None => "the pages differ in length".into(),
    }
}

/// The command line: the configurations to audit and what to do with the page.
struct Options {
    sets: Vec<&'static str>,
    metadata: Option<String>,
    /// The page to check or write where it is not `docs/licensing.md`, as given.
    page: Option<String>,
    markdown: bool,
    write: bool,
    quiet: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            sets: Vec::new(),
            metadata: None,
            page: None,
            markdown: false,
            write: false,
            quiet: false,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match arg.as_str() {
                "--set" => {
                    let set = value("--set")?;
                    let known = SETS.iter().find(|known| **known == set);
                    options
                        .sets
                        .push(known.ok_or_else(|| format!("--set {set}: gpl or non-gpl"))?);
                }
                "--metadata" => options.metadata = Some(value("--metadata")?),
                "--page" => options.page = Some(value("--page")?),
                "--markdown" => options.markdown = true,
                "--write" => options.write = true,
                "--quiet" => options.quiet = true,
                other => return Err(format!("unexpected argument: {other}")),
            }
        }
        if options.sets.is_empty() {
            options.sets = SETS.to_vec();
        }
        if options.metadata.is_some() && options.sets.len() != 1 {
            return Err("--metadata needs exactly one --set".into());
        }
        if (options.markdown || options.write) && !options.whole() {
            return Err("--markdown and --write need both configurations resolved by cargo".into());
        }
        Ok(options)
    }

    /// Whether both configurations are resolved by cargo, which a page check needs.
    fn whole(&self) -> bool {
        self.sets == SETS && self.metadata.is_none()
    }
}

/// Run the gate; `Ok(false)` when it found a violation or a stale page.
pub fn run(args: &[String]) -> Result<bool, String> {
    let options = Options::parse(args)?;
    let mut failed = false;
    let mut graphs: Graphs = Vec::new();
    let mut direct = BTreeSet::new();
    for set in &options.sets {
        let meta = match &options.metadata {
            Some(path) => {
                let fail = |e: &dyn std::fmt::Display| format!("--metadata {path}: {e}");
                let text = fs::canonicalize(path)
                    .and_then(fs::read_to_string)
                    .map_err(|e| fail(&e))?;
                serde_json::from_str(&text).map_err(|e| fail(&e))?
            }
            None => resolve(set)?,
        };
        let (crates, problems) = audit(&meta, set);
        if *set == "gpl" {
            direct = Graph::new(&meta).direct_dependencies();
        }
        if !options.markdown {
            let text = listing(&crates, set);
            println!(
                "{}",
                if options.quiet {
                    text.lines().next().unwrap_or_default()
                } else {
                    &text
                }
            );
        }
        for problem in &problems {
            println!("FAIL {set}: {problem}");
        }
        failed |= !problems.is_empty();
        if problems.is_empty() && !options.markdown {
            println!("PASS {set}: no copyleft component outside the allowed list\n");
        }
        graphs.push((set, crates));
    }
    if !options.whole() {
        return Ok(!failed);
    }

    let mut problems = Vec::new();
    let sections = page_sections(&graphs, &direct, &mut problems)?;
    for problem in &problems {
        println!("FAIL page: {problem}");
    }
    failed |= !problems.is_empty();
    if options.markdown {
        for (name, body) in &sections {
            println!("{}\n", framed(name, body));
        }
        return Ok(!failed);
    }

    let root = fs::canonicalize(repo()).map_err(|e| format!("the repository: {e}"))?;
    let path = options
        .page
        .as_ref()
        .map_or_else(|| root.join(PAGE), PathBuf::from);
    let path = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let page = &path
        .strip_prefix(&root)
        .unwrap_or(&path)
        .display()
        .to_string();
    let fix = match &options.page {
        None => "cargo xtask licenses --write".to_string(),
        Some(_) => format!("cargo xtask licenses --write --page {page}"),
    };
    let current = fs::read_to_string(&path).map_err(|e| format!("{page}: {e}"))?;
    let (rendered, missing) = splice(&current, &sections);
    for name in &missing {
        println!("FAIL page: {page} has no `check-licenses` section named {name}");
        failed = true;
    }
    if options.write {
        if rendered == current {
            println!("{page} is up to date");
        } else {
            fs::write(&path, rendered).map_err(|e| format!("{page}: {e}"))?;
            println!("wrote {page}");
        }
    } else if rendered != current {
        println!("{}", first_difference(&current, &rendered));
        println!("FAIL page: {page} differs from the graphs; run {fix}");
        failed = true;
    } else if missing.is_empty() {
        println!("PASS page: {page} matches the graphs");
    }
    Ok(!failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_non_gpl_manifest_swaps_the_one_feature() {
        let manifest = "[workspace.dependencies]\npixelflux = { path = \"p\", features = [\"gpl\"] }\npcmflux = { path = \"q\" }\n";
        let swapped = non_gpl_manifest(manifest).unwrap();
        assert_eq!(swapped, manifest.replace("\"gpl\"", "\"openh264\""));
        assert!(non_gpl_manifest("pixelflux = { path = \"p\" }\n").is_err());
        assert!(non_gpl_manifest(&manifest.repeat(2)).is_err());
    }

    #[test]
    fn a_section_is_replaced_between_its_markers_and_a_missing_one_is_named() {
        let page = format!(
            "intro\n{}\nold\n{}\noutro\n",
            marker("a", false),
            marker("a", true)
        );
        let sections = [("a", "new $1".to_string()), ("b", "other".to_string())];
        let (spliced, missing) = splice(&page, &sections);
        assert_eq!(
            spliced,
            format!("intro\n{}\noutro\n", framed("a", "new $1"))
        );
        assert_eq!(missing, ["b"]);
    }

    #[test]
    fn lists_and_build_labels_read_as_prose() {
        let items = |n: usize| {
            ["a", "b", "c"][..n]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (series(&items(1)), series(&items(2)), series(&items(3))),
            ("a".into(), "a and b".into(), "a, b, and c".into())
        );
        assert_eq!(build_label(&BTreeSet::from(["gpl", "non-gpl"])), "both");
        assert_eq!(build_label(&BTreeSet::from(["non-gpl"])), "non-GPL only");
        assert_eq!(
            table(&["A", "B"], &[vec!["x|y".into(), "z".into()]]),
            "| A | B |\n| --- | --- |\n| x\\|y | z |"
        );
    }

    #[test]
    fn a_copyleft_crate_fails_outside_the_configuration_allowed_to_carry_it() {
        let meta: Value = serde_json::from_str(
            r#"{"workspace_members": ["s"], "packages": [
                {"id": "s", "name": "selkies", "version": "1.0.0", "license": "MPL-2.0", "manifest_path": "/s"},
                {"id": "g", "name": "gnu", "version": "2.0.0", "license": "GPL-3.0-only", "manifest_path": "/g"},
                {"id": "c", "name": "codec-sys", "version": "0.1.0", "license": "MIT", "manifest_path": "/c"},
                {"id": "d", "name": "dev", "version": "0.1.0", "license": null, "manifest_path": "/d"}],
              "resolve": {"nodes": [
                {"id": "s", "features": [], "deps": [
                    {"pkg": "g", "dep_kinds": [{"kind": null}]},
                    {"pkg": "c", "dep_kinds": [{"kind": "build"}, {"kind": null}]},
                    {"pkg": "d", "dep_kinds": [{"kind": "dev"}]}]},
                {"id": "g", "features": [], "deps": []},
                {"id": "c", "features": ["x264"], "deps": []},
                {"id": "d", "features": [], "deps": []}]}}"#,
        )
        .unwrap();
        let (crates, problems) = audit(&meta, "non-gpl");
        let names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["codec-sys", "gnu", "selkies"]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("codec-sys 0.1.0: links "));
        assert_eq!(
            problems[1],
            "gnu 2.0.0: copyleft crate license 'GPL-3.0-only' in the non-gpl set"
        );
        let (_, problems) = audit(&meta, "gpl");
        assert_eq!(
            problems,
            ["gnu 2.0.0: copyleft crate license 'GPL-3.0-only' in the gpl set"]
        );
        assert_eq!(
            Graph::new(&meta).direct_dependencies(),
            BTreeSet::from(["codec-sys".to_string(), "gnu".to_string()])
        );
    }
}
