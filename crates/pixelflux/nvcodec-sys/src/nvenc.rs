/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(deref_nullptr)]
#![allow(clippy::all)]
#![allow(unknown_lints)]
#![allow(unpredictable_function_pointer_comparisons)]
#![allow(unnecessary_transmutes)]

//! Raw FFI bindings for the NVIDIA Video Codec SDK (NVENC): the bindgen-generated types, enums,
//! and function signatures, plus the `NV_ENC_*_VER` struct-version constants and codec/preset
//! GUIDs that bindgen can't derive from macros and that `build.rs` recovers by regexing the
//! header instead. Regenerated in place under the `regen` feature; a normal build compiles the
//! committed `src/bindgen/nvenc.rs` directly and needs no libclang.
include!("bindgen/nvenc.rs");

/// @brief dlsym symbol name for `NvEncodeAPIGetMaxSupportedVersion`, the entry point pixelflux
/// calls to negotiate the NVENC API version against the installed driver. bindgen emits the
/// struct and function signatures from the header but not this loader glue, so the symbol name
/// (and its function-pointer type below) are hand-written for manual dynamic loading.
pub const NV_ENCODE_API_GET_MAX_SUPPORTED_VERSION_FN_NAME: &[u8] =
    b"NvEncodeAPIGetMaxSupportedVersion\0";
/// @brief Function-pointer type the symbol above is cast to once resolved.
pub type NvEncodeApiGetMaxSupportedVersionFn =
    unsafe extern "C" fn(version: *mut u32) -> NVENCSTATUS;

/// @brief dlsym symbol name for `NvEncodeAPICreateInstance`, the entry point pixelflux calls to
/// obtain the driver's NVENC function table.
pub const NV_ENCODE_API_CREATE_INSTANCE_FN_NAME: &[u8] = b"NvEncodeAPICreateInstance\0";
/// @brief Function-pointer type the symbol above is cast to once resolved.
pub type NvEncodeApiCreateInstanceFn =
    unsafe extern "C" fn(functionList: *mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

/// @brief Runtime shared-library filename for the NVENC driver API on Linux, passed to `dlopen`.
#[cfg(not(windows))]
pub const NVENC_DLL_NAME: &str = "libnvidia-encode.so.1";
/// @brief Runtime shared-library filename for the NVENC driver API on Windows, passed to
/// `LoadLibrary`.
#[cfg(windows)]
pub const NVENC_DLL_NAME: &str = "nvEncodeAPI64.dll";
