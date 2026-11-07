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

//! Raw FFI bindings for the CUDA driver API subset pixelflux's NVENC path links against,
//! bindgen-generated into `src/bindgen/cuda.rs` and pulled in verbatim below. Regenerated in
//! place by `build.rs` under the `regen` feature; a normal build compiles the committed file
//! directly and needs neither libclang nor the CUDA toolkit.
include!("bindgen/cuda.rs");
