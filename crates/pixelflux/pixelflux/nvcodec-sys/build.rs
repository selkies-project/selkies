/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regenerates the committed NVENC + CUDA bindings from the bundled NVIDIA SDK headers in
//! `headers/`. Runs ONLY under the `regen` feature (which needs libclang); a normal build is a
//! no-op and compiles the checked-in `src/bindgen/*.rs`, so end-user builds need no libclang.
//!
//! bindgen does not emit function-like macros, so the NVENC struct-version constants
//! (`NV_ENC_*_VER`) and the `static const GUID` codec/preset GUIDs are extracted from the header
//! with regexes and appended -- mirroring how the upstream SDK defines them.

/// @brief Regenerates the committed `src/bindgen/{nvenc,cuda}.rs` FFI bindings from the NVIDIA
/// SDK and CUDA toolkit headers; this is the only path in the crate that needs bindgen or
/// libclang, and it only runs under the `regen` feature.
///
/// 1. **NVENC bindings**: runs bindgen over the bundled `headers/nvEncodeAPI.h`, allowlisting the
///    `NV.*` types, `Nv.*` functions, and `NVENC.*` / `NV_MAX.*` vars, and writes the result to
///    `src/bindgen/nvenc.rs`. A handful of driver-facing enums (`_NVENCSTATUS`,
///    `_NV_ENC_PIC_TYPE`, `_NV_ENC_PIC_STRUCT`, `_NV_ENC_PARAMS_FRAME_FIELD_MODE`,
///    `_NV_ENC_PARAMS_RC_MODE`, `_NV_ENC_MULTI_PASS`, `_NV_ENC_MV_PRECISION`) are newtyped rather
///    than rustified: their values arrive from the installed driver -- as return codes, or written
///    into structs pixelflux later reads -- so an out-of-range discriminant (legal from a driver
///    newer than this header) would be UB in a rustified enum, and some of these have no zero
///    variant for the zero-filling `Default` bindgen derives for rustified enums.
///
/// 2. **NVENC struct-version constants and GUIDs**: bindgen does not expand function-like macros,
///    so the `NV_ENC_*_VER` struct-version constants and the `static const GUID` codec/preset
///    GUIDs are recovered by regexing the raw header text and appended to `nvenc.rs` as plain
///    Rust `const`s, mirroring how the SDK headers define them. The struct-version regex captures
///    just the shift count out of the header's `( 1u<<31 )` high-bit term, since that C literal
///    (the `1u` suffix) isn't valid Rust and only the shift is needed to re-emit it.
///
/// 3. **CUDA bindings**: regenerated only when `CUDA_PATH` is set, since the toolkit's `cuda.h` is
///    large, version-specific, and not bundled; without it the already-committed `cuda.rs` is left
///    untouched. Generation is restricted to the 31 functions pixelflux's NVENC path actually
///    calls, and applies the same newtype treatment -- and for the same UB-avoidance reason -- to
///    `cudaError_enum` and `CUmemorytype_enum`.
///
/// 4. **Cargo directives**: reruns on changes to the NVENC header, this build script, or the
///    `CUDA_PATH` environment variable, and emits a `cargo:warning` when `CUDA_PATH` is unset so
///    it's visible that the CUDA bindings stayed on the committed version.
/// Marks the generated `extern "C"` blocks `unsafe`, which this crate's edition requires and
/// the pinned bindgen predates.
#[cfg(feature = "regen")]
fn mark_extern_blocks_unsafe(path: &std::path::Path) -> std::io::Result<()> {
    let src = std::fs::read_to_string(path)?;
    std::fs::write(
        path,
        src.replace("\nextern \"C\" {", "\nunsafe extern \"C\" {"),
    )
}

#[cfg(feature = "regen")]
fn main() -> std::io::Result<()> {
    use std::io::Write;
    let out = std::path::PathBuf::from("src/bindgen");
    std::fs::create_dir_all(&out)?;

    let nvenc_header = "headers/nvEncodeAPI.h";
    let nvenc_out = out.join("nvenc.rs");
    bindgen::builder()
        .header(nvenc_header)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_type("NV.*")
        .allowlist_function("Nv.*")
        .allowlist_var("NVENC.*")
        .allowlist_var("NV_MAX.*")
        .size_t_is_usize(true)
        .layout_tests(true)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        })
        .newtype_enum("_NVENCSTATUS")
        .newtype_enum("_NV_ENC_PIC_TYPE")
        .newtype_enum("_NV_ENC_PIC_STRUCT")
        .newtype_enum("_NV_ENC_PARAMS_FRAME_FIELD_MODE")
        .newtype_enum("_NV_ENC_PARAMS_RC_MODE")
        .newtype_enum("_NV_ENC_MULTI_PASS")
        .newtype_enum("_NV_ENC_MV_PRECISION")
        .derive_default(true)
        .derive_eq(true)
        .derive_hash(true)
        .derive_ord(true)
        .generate()
        .expect("Unable to generate NVENC bindings")
        .write_to_file(&nvenc_out)
        .expect("Unable to write nvenc.rs");
    mark_extern_blocks_unsafe(&nvenc_out)?;

    let hdr = std::fs::read_to_string(nvenc_header)?;
    let mut extra = String::from(
        "\nconst fn nv_struct_version(ver: u32) -> u32 {\n    NVENCAPI_VERSION | ((ver) << 16) | (0x7 << 28)\n}\n",
    );
    let ver_re = regex::Regex::new(
        r"#define\s+([A-Z_]+)\s+\(?NVENCAPI_STRUCT_VERSION\((\d+)\)(?:\s*\|\s*\(\s*1u?\s*<<\s*(\d+)\s*\))?\s*\)?",
    )
    .unwrap();
    for c in ver_re.captures_iter(&hdr) {
        let (name, ver) = (&c[1], &c[2]);
        match c.get(3) {
            Some(shift) => extra.push_str(&format!(
                "pub const {}: u32 = nv_struct_version({}) | (1u32 << {});\n",
                name,
                ver,
                shift.as_str()
            )),
            None => extra.push_str(&format!(
                "pub const {}: u32 = nv_struct_version({});\n",
                name, ver
            )),
        }
    }
    let guid_re = regex::Regex::new(
        r"static\s+const\s+GUID\s+([A-Z_\d]+)\s*=\s*\r?\n\{\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*\{\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*,\s*(0[xX][0-9a-fA-F]+)\s*\}\s*\}\s*;",
    )
    .unwrap();
    for c in guid_re.captures_iter(&hdr) {
        extra.push_str(&format!(
            "pub const {}: GUID = GUID {{\n    Data1: {},\n    Data2: {},\n    Data3: {},\n    Data4: [{}, {}, {}, {}, {}, {}, {}, {}],\n}};\n",
            &c[1], &c[2], &c[3], &c[4], &c[5], &c[6], &c[7], &c[8], &c[9], &c[10], &c[11], &c[12]
        ));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(&nvenc_out)?
        .write_all(extra.as_bytes())?;

    if let Ok(cuda_path) = std::env::var("CUDA_PATH") {
        let cuda_header = format!("{}/include/cuda.h", cuda_path);
        let cuda_funcs = [
            "cuGetErrorString",
            "cuGetErrorName",
            "cuInit",
            "cuDeviceGetCount",
            "cuDeviceGet",
            "cuDeviceGetName",
            "cuDeviceGetUuid",
            "cuCtxCreate_v2",
            "cuCtxDestroy_v2",
            "cuCtxPushCurrent_v2",
            "cuCtxPopCurrent_v2",
            "cuStreamCreate",
            "cuStreamDestroy_v2",
            "cuMemAllocHost_v2",
            "cuMemAllocPitch_v2",
            "cuMemFree_v2",
            "cuMemFreeHost",
            "cuMemcpy2D_v2",
            "cuMemcpy2DUnaligned_v2",
            "cuMemcpy2DAsync_v2",
            "cuMemcpyDtoH_v2",
            "cuImportExternalMemory",
            "cuImportExternalSemaphore",
            "cuExternalMemoryGetMappedBuffer",
            "cuExternalMemoryGetMappedMipmappedArray",
            "cuMipmappedArrayGetLevel",
            "cuMipmappedArrayDestroy",
            "cuDestroyExternalMemory",
            "cuDestroyExternalSemaphore",
            "cuWaitExternalSemaphoresAsync",
            "cuSignalExternalSemaphoresAsync",
            "cuTexObjectCreate",
            "cuTexObjectDestroy",
        ];
        let mut cuda_builder = bindgen::builder()
            .header(&cuda_header)
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .size_t_is_usize(true)
            .layout_tests(true)
            .default_enum_style(bindgen::EnumVariation::Rust {
                non_exhaustive: false,
            })
            .newtype_enum("cudaError_enum")
            .newtype_enum("CUmemorytype_enum")
            .generate_comments(false)
            .derive_default(true)
            .derive_eq(true)
            .derive_hash(true)
            .derive_ord(true);
        for f in cuda_funcs {
            cuda_builder = cuda_builder.allowlist_function(f);
        }
        cuda_builder
            .generate()
            .expect("Unable to generate CUDA bindings")
            .write_to_file(out.join("cuda.rs"))
            .expect("Unable to write cuda.rs");
        mark_extern_blocks_unsafe(&out.join("cuda.rs"))?;
        println!("cargo:rerun-if-env-changed=CUDA_PATH");
    } else {
        println!(
            "cargo:warning=CUDA_PATH unset: kept committed src/bindgen/cuda.rs (regenerated NVENC only). Set CUDA_PATH to rebind CUDA."
        );
    }

    println!("cargo:rerun-if-changed=headers/nvEncodeAPI.h");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// @brief No-op build script for ordinary builds: the committed `src/bindgen/*.rs` bindings are
/// compiled as-is without invoking bindgen.
#[cfg(not(feature = "regen"))]
fn main() {}
