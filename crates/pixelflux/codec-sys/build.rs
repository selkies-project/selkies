/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Locates each enabled codec library through pkg-config, links it, and generates its
//! bindings from the installed headers into `OUT_DIR`, one file per library. The SVT-AV1
//! release is read from its header, since the encoder's handle and option surface moved
//! between releases and the caller selects on it.

use std::env;
use std::io::Write;
use std::path::PathBuf;

/// One library: its pkg-config name, the oldest release the crate builds and runs with, the
/// header the bindings are generated from, and the items to keep (everything the header pulls
/// in otherwise doubles the bindings).
struct Lib {
    feature: &'static str,
    pkg: &'static str,
    min: &'static str,
    header: &'static str,
    allow: &'static [&'static str],
}

const LIBS: &[Lib] = &[
    Lib {
        feature: "vpx",
        pkg: "vpx",
        min: "1.11",
        header: "vpx.h",
        allow: &[
            "vpx_.*",
            "VPX_.*",
            "VP8.*",
            "VP9.*",
            "vp8e_.*",
            "vp9e_.*",
            "vp8_.*",
            "vpx_svc_.*",
        ],
    },
    Lib {
        feature: "x265",
        pkg: "x265",
        min: "3.5",
        header: "x265w.h",
        allow: &["x265_.*", "X265_.*"],
    },
    Lib {
        feature: "kvazaar",
        pkg: "kvazaar",
        min: "2.3.2",
        header: "kvazaar.h",
        allow: &["kvz_.*", "KVZ_.*"],
    },
    Lib {
        feature: "svtav1",
        pkg: "SvtAv1Enc",
        min: "1.0",
        header: "svtav1.h",
        allow: &[
            "svt_av1_.*",
            "Eb.*",
            "EB_.*",
            "SVT_AV1_.*",
            "Svt.*",
            "SvtAv1.*",
        ],
    },
    Lib {
        feature: "dav1d",
        pkg: "dav1d",
        min: "1.0",
        header: "dav1dw.h",
        allow: &["dav1d_.*", "Dav1d.*", "DAV1D_.*"],
    },
    Lib {
        feature: "de265",
        pkg: "libde265",
        min: "1.0.8",
        header: "de265w.h",
        allow: &["de265_.*", "DE265_.*", "LIBDE265_.*"],
    },
];

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(svtav1_handle_priv)");
    println!("cargo::rustc-check-cfg=cfg(svtav1_rtc)");
    println!("cargo::rustc-check-cfg=cfg(svtav1_events)");
    println!("cargo::rustc-check-cfg=cfg(x265_layer_pointers)");
    for lib in LIBS {
        if env::var(format!("CARGO_FEATURE_{}", lib.feature.to_uppercase())).is_err() {
            continue;
        }
        let probed = pkg_config::Config::new()
            .atleast_version(lib.min)
            .probe(lib.pkg)
            .unwrap_or_else(|e| {
                panic!(
                    "{}: the `{}` feature needs {} {} or newer (pkg-config): {e}",
                    lib.pkg, lib.feature, lib.pkg, lib.min
                )
            });
        let header = format!("headers/{}", lib.header);
        println!("cargo:rerun-if-changed={header}");
        let mut builder = bindgen::builder()
            .header(&header)
            .clang_args(probed.include_paths.iter().flat_map(|p| {
                // SVT-AV1's pkg-config file names the include directory with or without
                // its `svt-av1/` tail depending on who packaged it, so both are searched.
                [
                    format!("-I{}", p.display()),
                    format!("-I{}/svt-av1", p.display()),
                ]
            }))
            .size_t_is_usize(true)
            .layout_tests(false)
            .default_enum_style(bindgen::EnumVariation::Consts)
            .prepend_enum_name(false)
            .derive_default(true)
            .generate_comments(false);
        for pattern in lib.allow {
            builder = builder
                .allowlist_type(pattern)
                .allowlist_function(pattern)
                .allowlist_var(pattern);
        }
        let bindings = builder
            .generate()
            .unwrap_or_else(|e| panic!("{}: bindgen failed: {e}", lib.pkg));
        bindings
            .write_to_file(out.join(format!("{}.rs", lib.feature)))
            .unwrap_or_else(|e| panic!("{}: writing the bindings failed: {e}", lib.pkg));
        if lib.feature == "x265" {
            // The API table's entry point carries the build number in its name, so the shim
            // below names whichever one this header declares.
            let generated = std::fs::read_to_string(out.join("x265.rs")).unwrap();
            // Builds 210 to 212 hand encoder_encode an array of layer pointers rather than
            // an array of pictures.
            if generated.contains("pic_out: *mut *mut x265_picture") {
                println!("cargo:rustc-cfg=x265_layer_pointers");
            }
            let entry = generated
                .split_whitespace()
                .find_map(|word| {
                    word.strip_prefix("x265_api_get_").map(|rest| {
                        format!(
                            "x265_api_get_{}",
                            rest.trim_end_matches(|c: char| !c.is_ascii_digit())
                        )
                    })
                })
                .expect("x265.h declares no x265_api_get_<build>");
            std::fs::OpenOptions::new()
                .append(true)
                .open(out.join("x265.rs"))
                .unwrap()
                .write_all(
                    format!(
                        "\n/// The API table of the linked x265, by the build number its header names.\n\
                         pub unsafe fn x265_api_get(bit_depth: ::std::os::raw::c_int) -> *const x265_api {{\n    \
                         unsafe {{ {entry}(bit_depth) }}\n}}\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        if lib.feature == "svtav1" {
            let version = probed
                .version
                .split('.')
                .map(|v| v.parse::<u32>().unwrap_or(0))
                .collect::<Vec<_>>();
            let (major, minor) = (
                version.first().copied().unwrap_or(0),
                version.get(1).copied().unwrap_or(0),
            );
            if major < 3 {
                println!("cargo:rustc-cfg=svtav1_handle_priv");
            }
            if (major, minor) >= (3, 1) {
                println!("cargo:rustc-cfg=svtav1_rtc");
            }
            if std::fs::read_to_string(out.join("svtav1.rs"))
                .unwrap()
                .contains("REF_USE_EVENT")
            {
                println!("cargo:rustc-cfg=svtav1_events");
            }
        }
    }
}
