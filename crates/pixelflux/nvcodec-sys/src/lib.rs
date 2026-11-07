/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! FFI bindings for the NVIDIA Video Codec SDK (NVENC) and the CUDA driver subset pixelflux uses.
//!
//! The NVENC bindings are generated from the bundled `headers/nvEncodeAPI.h` (NVENCAPI 13.0); the
//! CUDA bindings are generated from the CUDA toolkit's `$CUDA_PATH/include/cuda.h` (not bundled --
//! it's large and ships with the toolkit). Both are committed under `src/bindgen/`, so a normal
//! build needs no libclang or CUDA toolkit. Regenerate after a header bump with
//! `cargo build --features regen` (NVENC needs libclang; CUDA also needs CUDA_PATH).

mod nvenc;
pub use nvenc::*;

#[cfg(feature = "cuda")]
pub mod cuda;
