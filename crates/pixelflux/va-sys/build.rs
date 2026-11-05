/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regenerates the committed VA-API bindings from the bundled libva headers in `headers/`.
//! Runs ONLY under the `regen` feature (which needs libclang); a normal build is a no-op and
//! compiles the checked-in `src/bindgen/va.rs`, so end-user builds need no libclang.
//!
//! Only types, enums, and constants are generated: the entry points are loaded from the host's
//! `libva.so.2` at run time through the function table `src/lib.rs` declares by hand, which is
//! also what a test substitutes its own functions into.

#[cfg(feature = "regen")]
fn main() -> std::io::Result<()> {
    let out = std::path::PathBuf::from("src/bindgen");
    std::fs::create_dir_all(&out)?;
    bindgen::builder()
        .header("headers/wrapper.h")
        .clang_arg("-Iheaders")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_file(".*/va_(enc_h264|enc_hevc|enc_vp8|enc_vp9|enc_av1|vpp|drmcommon)\\.h")
        .allowlist_type(
            "^_?VA(Status|Display|Profile|Entrypoint|ConfigAttrib\\w*|GenericID|ConfigID|ContextID|\
             SurfaceID|BufferID|ImageID|SurfaceAttrib\\w*|GenericValue\\w*|GenericFunc|ImageFormat|\
             Image|BufferType|CodedBufferSegment|Rectangle|MotionVector|PictureH264|PictureHEVC|\
             EncMisc\\w*|EncPackedHeader\\w*|MessageCallback|SurfaceStatus|EncROI)$",
        )
        .allowlist_var("^VA_.*")
        .ignore_functions()
        .size_t_is_usize(true)
        .layout_tests(true)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .derive_debug(false)
        .generate_comments(false)
        .generate()
        .expect("Unable to generate VA-API bindings")
        .write_to_file(out.join("va.rs"))
        .expect("Unable to write va.rs");
    println!("cargo:rerun-if-changed=headers/wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// No-op build script for ordinary builds: the committed `src/bindgen/va.rs` bindings are
/// compiled as-is without invoking bindgen.
#[cfg(not(feature = "regen"))]
fn main() {}
