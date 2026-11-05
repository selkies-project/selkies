/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Run-time binding to `libpipewire-0.3` and the SPA pod encoding shared by every PipeWire user
//! in pixelflux: the webcam sink (a `Video/Source` node) and the portal screen capture (a
//! `Video/Input` stream). The library is loaded with dlopen, so pixelflux keeps no build-time or
//! load-time dependency on PipeWire and each user simply stays off where the library or the
//! daemon is absent. Pods are built and read by hand against the SPA wire layout: 8-byte aligned
//! `size, type, payload` cells, objects carrying `key, flags, cell` properties, and a choice
//! cell whose first value is the default (and, once fixated, the only) value.

use std::ffi::{c_char, c_int, c_void};
use std::mem;
use std::ptr;

use libloading::Library;

pub const SPA_TYPE_ID: u32 = 3;
pub const SPA_TYPE_INT: u32 = 4;
pub const SPA_TYPE_LONG: u32 = 5;
pub const SPA_TYPE_RECTANGLE: u32 = 10;
pub const SPA_TYPE_FRACTION: u32 = 11;
pub const SPA_TYPE_OBJECT: u32 = 15;
pub const SPA_TYPE_CHOICE: u32 = 19;
pub const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
pub const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;
pub const SPA_TYPE_OBJECT_PARAM_META: u32 = 0x40005;
pub const SPA_PARAM_ENUM_FORMAT: u32 = 3;
pub const SPA_PARAM_FORMAT: u32 = 4;
pub const SPA_PARAM_BUFFERS: u32 = 5;
pub const SPA_PARAM_META: u32 = 6;
pub const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
pub const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
pub const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
pub const SPA_FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
pub const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
pub const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
pub const SPA_FORMAT_VIDEO_MAX_FRAMERATE: u32 = 0x20005;
pub const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
pub const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
pub const SPA_MEDIA_SUBTYPE_MJPG: u32 = 0x20002;
pub const SPA_VIDEO_FORMAT_I420: u32 = 2;
pub const SPA_VIDEO_FORMAT_YUY2: u32 = 4;
pub const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
pub const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
pub const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
pub const SPA_VIDEO_FORMAT_BGRA: u32 = 12;
pub const SPA_VIDEO_FORMAT_ARGB: u32 = 13;
pub const SPA_VIDEO_FORMAT_ABGR: u32 = 14;
pub const SPA_VIDEO_FORMAT_NV12: u32 = 23;
pub const SPA_PARAM_BUFFERS_BUFFERS: u32 = 1;
pub const SPA_PARAM_BUFFERS_BLOCKS: u32 = 2;
pub const SPA_PARAM_BUFFERS_SIZE: u32 = 3;
pub const SPA_PARAM_BUFFERS_STRIDE: u32 = 4;
pub const SPA_PARAM_BUFFERS_ALIGN: u32 = 5;
pub const SPA_PARAM_BUFFERS_DATATYPE: u32 = 6;
pub const SPA_PARAM_META_TYPE: u32 = 1;
pub const SPA_PARAM_META_SIZE: u32 = 2;
pub const SPA_META_HEADER: u32 = 1;
pub const SPA_META_VIDEO_CROP: u32 = 2;
pub const SPA_META_VIDEO_DAMAGE: u32 = 3;
pub const SPA_META_CURSOR: u32 = 5;
pub const SPA_META_HEADER_SIZE: u32 = 32;
pub const SPA_META_REGION_SIZE: u32 = 16;
pub const SPA_DATA_MEMPTR: u32 = 1;
pub const SPA_DATA_MEMFD: u32 = 2;
pub const SPA_DATA_DMABUF: u32 = 3;
pub const SPA_CHUNK_FLAG_CORRUPTED: i32 = 1;
pub const SPA_CHOICE_NONE: u32 = 0;
pub const SPA_CHOICE_RANGE: u32 = 1;
pub const SPA_CHOICE_ENUM: u32 = 3;
pub const SPA_CHOICE_FLAGS: u32 = 4;
pub const SPA_POD_PROP_FLAG_MANDATORY: u32 = 1 << 3;
pub const SPA_POD_PROP_FLAG_DONT_FIXATE: u32 = 1 << 4;
pub const PW_DIRECTION_INPUT: c_int = 0;
pub const PW_DIRECTION_OUTPUT: c_int = 1;
pub const PW_ID_ANY: u32 = 0xFFFF_FFFF;
pub const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1;
pub const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 4;
pub const PW_STREAM_FLAG_DRIVER: u32 = 8;
pub const PW_STREAM_STATE_ERROR: c_int = -1;
pub const PW_STREAM_STATE_UNCONNECTED: c_int = 0;
pub const PW_STREAM_STATE_PAUSED: c_int = 2;
pub const PW_STREAM_STATE_STREAMING: c_int = 3;
pub const PW_VERSION_STREAM_EVENTS: u32 = 2;

#[repr(C)]
pub struct PwStreamEvents {
    pub version: u32,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    pub state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    pub control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    pub io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    pub param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    pub add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    pub remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    pub process: Option<unsafe extern "C" fn(*mut c_void)>,
    pub drained: Option<unsafe extern "C" fn(*mut c_void)>,
    pub command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    pub trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl PwStreamEvents {
    /// An event table with every callback unset; users fill in the ones they serve.
    pub const fn empty() -> Self {
        Self {
            version: PW_VERSION_STREAM_EVENTS,
            destroy: None,
            state_changed: None,
            control_info: None,
            io_changed: None,
            param_changed: None,
            add_buffer: None,
            remove_buffer: None,
            process: None,
            drained: None,
            command: None,
            trigger_done: None,
        }
    }
}

#[repr(C)]
pub struct PwBuffer {
    pub buffer: *mut SpaBuffer,
    pub user_data: *mut c_void,
    pub size: u64,
    pub requested: u64,
    pub time: u64,
}

#[repr(C)]
pub struct SpaBuffer {
    pub n_metas: u32,
    pub n_datas: u32,
    pub metas: *mut SpaMeta,
    pub datas: *mut SpaData,
}

#[repr(C)]
pub struct SpaMeta {
    pub type_: u32,
    pub size: u32,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct SpaData {
    pub type_: u32,
    pub flags: u32,
    pub fd: i64,
    pub mapoffset: u32,
    pub maxsize: u32,
    pub data: *mut c_void,
    pub chunk: *mut SpaChunk,
}

#[repr(C)]
pub struct SpaChunk {
    pub offset: u32,
    pub size: u32,
    pub stride: i32,
    pub flags: i32,
}

#[repr(C)]
pub struct SpaMetaHeader {
    pub flags: u32,
    pub offset: u32,
    pub pts: i64,
    pub dts_offset: i64,
    pub seq: u64,
}

/// `spa_meta_region`: one damage or crop rectangle; a zero-sized one ends a damage array.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpaMetaRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
pub struct SpaMetaCursor {
    pub id: u32,
    pub flags: u32,
    pub x: i32,
    pub y: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub bitmap_offset: u32,
}

#[repr(C)]
pub struct SpaMetaBitmap {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub stride: i32,
    pub offset: u32,
}

/// Bytes a cursor meta needs to carry a `w`x`h` sprite (`CURSOR_META_SIZE`).
pub const fn cursor_meta_size(w: u32, h: u32) -> u32 {
    (mem::size_of::<SpaMetaCursor>() + mem::size_of::<SpaMetaBitmap>()) as u32 + w * h * 4
}

type PwInit = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type PwThreadLoopNew = unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void;
type PwThreadLoopGetLoop = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type PwThreadLoopInt = unsafe extern "C" fn(*mut c_void) -> c_int;
type PwThreadLoopVoid = unsafe extern "C" fn(*mut c_void);
type PwContextNew = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextConnect = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextConnectFd =
    unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void;
type PwContextDestroy = unsafe extern "C" fn(*mut c_void);
type PwCoreDisconnect = unsafe extern "C" fn(*mut c_void) -> c_int;
type PwPropertiesNew = unsafe extern "C" fn(*const c_char, ...) -> *mut c_void;
type PwPropertiesSet = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type PwStreamNew = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void;
type PwStreamAddListener =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const PwStreamEvents, *mut c_void);
type PwStreamConnect =
    unsafe extern "C" fn(*mut c_void, c_int, u32, u32, *mut *const c_void, u32) -> c_int;
type PwStreamUpdateParams = unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int;
type PwStreamDequeueBuffer = unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer;
type PwStreamQueueBuffer = unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int;
type PwStreamTriggerProcess = unsafe extern "C" fn(*mut c_void) -> c_int;
type PwStreamSetActive = unsafe extern "C" fn(*mut c_void, bool) -> c_int;
type PwStreamVoid = unsafe extern "C" fn(*mut c_void);
type PwStreamInt = unsafe extern "C" fn(*mut c_void) -> c_int;

/// Entry points resolved from `libpipewire-0.3.so.0`.
#[derive(Clone, Copy)]
pub struct Api {
    pub thread_loop_new: PwThreadLoopNew,
    pub thread_loop_get_loop: PwThreadLoopGetLoop,
    pub thread_loop_start: PwThreadLoopInt,
    pub thread_loop_stop: PwThreadLoopVoid,
    pub thread_loop_lock: PwThreadLoopVoid,
    pub thread_loop_unlock: PwThreadLoopVoid,
    pub thread_loop_destroy: PwThreadLoopVoid,
    pub context_new: PwContextNew,
    pub context_connect: PwContextConnect,
    pub context_connect_fd: PwContextConnectFd,
    pub context_destroy: PwContextDestroy,
    pub core_disconnect: PwCoreDisconnect,
    pub properties_new: PwPropertiesNew,
    pub properties_set: PwPropertiesSet,
    pub stream_new: PwStreamNew,
    pub stream_add_listener: PwStreamAddListener,
    pub stream_connect: PwStreamConnect,
    pub stream_update_params: PwStreamUpdateParams,
    pub stream_dequeue_buffer: PwStreamDequeueBuffer,
    pub stream_queue_buffer: PwStreamQueueBuffer,
    pub stream_trigger_process: PwStreamTriggerProcess,
    pub stream_set_active: PwStreamSetActive,
    pub stream_disconnect: PwStreamInt,
    pub stream_destroy: PwStreamVoid,
}

/// The library handle is kept for the life of the process: PipeWire's own globals (pw_init) make
/// unloading it unsafe, and every later user reuses it.
pub fn api() -> Result<&'static Api, String> {
    static API: std::sync::OnceLock<Result<Api, String>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = Library::new("libpipewire-0.3.so.0")
            .map_err(|e| format!("libpipewire-0.3 not available: {}", e))?;
        macro_rules! sym {
            ($name:literal, $t:ty) => {
                *lib.get::<$t>(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("{}: {}", $name, e))?
            };
        }
        let init: PwInit = sym!("pw_init", PwInit);
        let api = Api {
            thread_loop_new: sym!("pw_thread_loop_new", PwThreadLoopNew),
            thread_loop_get_loop: sym!("pw_thread_loop_get_loop", PwThreadLoopGetLoop),
            thread_loop_start: sym!("pw_thread_loop_start", PwThreadLoopInt),
            thread_loop_stop: sym!("pw_thread_loop_stop", PwThreadLoopVoid),
            thread_loop_lock: sym!("pw_thread_loop_lock", PwThreadLoopVoid),
            thread_loop_unlock: sym!("pw_thread_loop_unlock", PwThreadLoopVoid),
            thread_loop_destroy: sym!("pw_thread_loop_destroy", PwThreadLoopVoid),
            context_new: sym!("pw_context_new", PwContextNew),
            context_connect: sym!("pw_context_connect", PwContextConnect),
            context_connect_fd: sym!("pw_context_connect_fd", PwContextConnectFd),
            context_destroy: sym!("pw_context_destroy", PwContextDestroy),
            core_disconnect: sym!("pw_core_disconnect", PwCoreDisconnect),
            properties_new: sym!("pw_properties_new", PwPropertiesNew),
            properties_set: sym!("pw_properties_set", PwPropertiesSet),
            stream_new: sym!("pw_stream_new", PwStreamNew),
            stream_add_listener: sym!("pw_stream_add_listener", PwStreamAddListener),
            stream_connect: sym!("pw_stream_connect", PwStreamConnect),
            stream_update_params: sym!("pw_stream_update_params", PwStreamUpdateParams),
            stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer", PwStreamDequeueBuffer),
            stream_queue_buffer: sym!("pw_stream_queue_buffer", PwStreamQueueBuffer),
            stream_trigger_process: sym!("pw_stream_trigger_process", PwStreamTriggerProcess),
            stream_set_active: sym!("pw_stream_set_active", PwStreamSetActive),
            stream_disconnect: sym!("pw_stream_disconnect", PwStreamInt),
            stream_destroy: sym!("pw_stream_destroy", PwStreamVoid),
        };
        init(ptr::null_mut(), ptr::null_mut());
        mem::forget(lib);
        Ok(api)
    })
    .as_ref()
    .map_err(|e| e.clone())
}

// --- SPA pod construction ---------------------------------------------------------------------

pub fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_ne_bytes());
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// One `size, type, payload` cell, padded to 8 bytes.
pub fn pod_prim(v: &mut Vec<u8>, ty: u32, payload: &[u8]) {
    push_u32(v, payload.len() as u32);
    push_u32(v, ty);
    v.extend_from_slice(payload);
    pad8(v);
}

pub fn pod_id(v: &mut Vec<u8>, x: u32) {
    pod_prim(v, SPA_TYPE_ID, &x.to_ne_bytes());
}

pub fn pod_int(v: &mut Vec<u8>, x: i32) {
    pod_prim(v, SPA_TYPE_INT, &x.to_ne_bytes());
}

pub fn pod_rect(v: &mut Vec<u8>, w: u32, h: u32) {
    let mut p = Vec::new();
    push_u32(&mut p, w);
    push_u32(&mut p, h);
    pod_prim(v, SPA_TYPE_RECTANGLE, &p);
}

pub fn pod_frac(v: &mut Vec<u8>, num: u32, den: u32) {
    let mut p = Vec::new();
    push_u32(&mut p, num);
    push_u32(&mut p, den);
    pod_prim(v, SPA_TYPE_FRACTION, &p);
}

/// A choice cell over fixed-size values of `child_ty`: `values` holds the default first, then
/// the alternatives (Enum), the bounds (Range), or the flag words (Flags), each `child_size` bytes.
fn pod_choice(v: &mut Vec<u8>, choice: u32, child_ty: u32, child_size: u32, values: &[u8]) {
    let mut body = Vec::new();
    push_u32(&mut body, choice);
    push_u32(&mut body, 0);
    push_u32(&mut body, child_size);
    push_u32(&mut body, child_ty);
    body.extend_from_slice(values);
    pod_prim(v, SPA_TYPE_CHOICE, &body);
}

pub fn pod_choice_int(v: &mut Vec<u8>, choice: u32, values: &[i32]) {
    let bytes: Vec<u8> = values.iter().flat_map(|x| x.to_ne_bytes()).collect();
    pod_choice(v, choice, SPA_TYPE_INT, 4, &bytes);
}

pub fn pod_choice_id(v: &mut Vec<u8>, choice: u32, values: &[u32]) {
    let bytes: Vec<u8> = values.iter().flat_map(|x| x.to_ne_bytes()).collect();
    pod_choice(v, choice, SPA_TYPE_ID, 4, &bytes);
}

pub fn pod_choice_long(v: &mut Vec<u8>, choice: u32, values: &[i64]) {
    let bytes: Vec<u8> = values.iter().flat_map(|x| x.to_ne_bytes()).collect();
    pod_choice(v, choice, SPA_TYPE_LONG, 8, &bytes);
}

/// A rectangle choice; `values` are `(w, h)` pairs, default first.
pub fn pod_choice_rect(v: &mut Vec<u8>, choice: u32, values: &[(u32, u32)]) {
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|(w, h)| [w.to_ne_bytes(), h.to_ne_bytes()].concat())
        .collect();
    pod_choice(v, choice, SPA_TYPE_RECTANGLE, 8, &bytes);
}

/// A fraction choice; `values` are `(num, den)` pairs, default first.
pub fn pod_choice_frac(v: &mut Vec<u8>, choice: u32, values: &[(u32, u32)]) {
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|(n, d)| [n.to_ne_bytes(), d.to_ne_bytes()].concat())
        .collect();
    pod_choice(v, choice, SPA_TYPE_FRACTION, 8, &bytes);
}

/// One object property: key, flags, then the value cell `value` appends.
pub fn prop_flags(v: &mut Vec<u8>, key: u32, flags: u32, value: impl FnOnce(&mut Vec<u8>)) {
    push_u32(v, key);
    push_u32(v, flags);
    value(v);
}

pub fn prop(v: &mut Vec<u8>, key: u32, value: impl FnOnce(&mut Vec<u8>)) {
    prop_flags(v, key, 0, value);
}

/// An object pod of type `ty` carrying param `id` and the properties `props` appends.
pub fn object(ty: u32, id: u32, props: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut body = Vec::new();
    push_u32(&mut body, ty);
    push_u32(&mut body, id);
    props(&mut body);
    let mut v = Vec::new();
    pod_prim(&mut v, SPA_TYPE_OBJECT, &body);
    v
}

// --- SPA pod reading --------------------------------------------------------------------------

/// One property of an object pod: its key, flags, and value cell.
#[derive(Clone, Copy, Debug)]
pub struct PodProp<'a> {
    pub key: u32,
    pub flags: u32,
    pub ty: u32,
    pub payload: &'a [u8],
}

fn u32_at(v: &[u8], off: usize) -> Option<u32> {
    v.get(off..off + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
}

/// The bytes of the pod a PipeWire callback points at: its header names the payload size.
///
/// # Safety
/// `pod` must point at a complete SPA pod that outlives the returned slice.
pub unsafe fn pod_bytes<'a>(pod: *const c_void) -> &'a [u8] {
    let size = unsafe { *(pod as *const u32) } as usize;
    unsafe { std::slice::from_raw_parts(pod as *const u8, size + 8) }
}

/// Walk an object pod: `(object type, param id, properties)`; `None` for anything else.
pub fn object_props(pod: &[u8]) -> Option<(u32, u32, Vec<PodProp<'_>>)> {
    let size = u32_at(pod, 0)? as usize;
    if u32_at(pod, 4)? != SPA_TYPE_OBJECT || pod.len() < size + 8 {
        return None;
    }
    let body = &pod[8..8 + size];
    let ty = u32_at(body, 0)?;
    let id = u32_at(body, 4)?;
    let mut props = Vec::new();
    let mut off = 8;
    while off + 16 <= body.len() {
        let key = u32_at(body, off)?;
        let flags = u32_at(body, off + 4)?;
        let vsize = u32_at(body, off + 8)? as usize;
        let vty = u32_at(body, off + 12)?;
        let payload = body.get(off + 16..off + 16 + vsize)?;
        props.push(PodProp {
            key,
            flags,
            ty: vty,
            payload,
        });
        off += 16 + vsize.div_ceil(8) * 8;
    }
    Some((ty, id, props))
}

/// A cell's first value: a plain cell as it is, a choice cell's default (its only value once
/// fixated). Returns the value type and bytes.
pub fn pod_value(ty: u32, payload: &[u8]) -> Option<(u32, &[u8])> {
    if ty != SPA_TYPE_CHOICE {
        return Some((ty, payload));
    }
    let child_size = u32_at(payload, 8)? as usize;
    let child_ty = u32_at(payload, 12)?;
    Some((child_ty, payload.get(16..16 + child_size)?))
}

impl PodProp<'_> {
    pub fn as_u32(&self) -> Option<u32> {
        let (ty, b) = pod_value(self.ty, self.payload)?;
        (ty == SPA_TYPE_ID || ty == SPA_TYPE_INT)
            .then(|| u32_at(b, 0))
            .flatten()
    }

    pub fn as_i64(&self) -> Option<i64> {
        let (ty, b) = pod_value(self.ty, self.payload)?;
        (ty == SPA_TYPE_LONG)
            .then(|| {
                b.get(..8)
                    .map(|x| i64::from_ne_bytes(x.try_into().unwrap()))
            })
            .flatten()
    }

    /// A rectangle or fraction pair.
    pub fn as_pair(&self) -> Option<(u32, u32)> {
        let (ty, b) = pod_value(self.ty, self.payload)?;
        (ty == SPA_TYPE_RECTANGLE || ty == SPA_TYPE_FRACTION)
            .then(|| Some((u32_at(b, 0)?, u32_at(b, 4)?)))
            .flatten()
    }
}

/// The property `key` of an object pod's property list.
pub fn find_prop<'a>(props: &'a [PodProp<'a>], key: u32) -> Option<&'a PodProp<'a>> {
    props.iter().find(|p| p.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_libpipewire() {
        assert_eq!(mem::size_of::<PwStreamEvents>(), 96);
        assert_eq!(mem::size_of::<PwBuffer>(), 40);
        assert_eq!(mem::size_of::<SpaBuffer>(), 24);
        assert_eq!(mem::size_of::<SpaData>(), 40);
        assert_eq!(mem::size_of::<SpaChunk>(), 16);
        assert_eq!(mem::size_of::<SpaMetaHeader>(), 32);
        assert_eq!(
            mem::size_of::<SpaMetaRegion>(),
            SPA_META_REGION_SIZE as usize
        );
        assert_eq!(mem::size_of::<SpaMetaCursor>(), 28);
        assert_eq!(mem::size_of::<SpaMetaBitmap>(), 20);
        assert_eq!(cursor_meta_size(64, 64), 28 + 20 + 64 * 64 * 4);
        assert_eq!(mem::offset_of!(PwStreamEvents, process), 64);
        assert_eq!(mem::offset_of!(SpaData, data), 24);
        assert_eq!(mem::offset_of!(SpaData, chunk), 32);
    }

    /// Every value cell the builders emit reads back through the reader, plain or as a choice
    /// whose default is the first value, with property flags preserved.
    #[test]
    fn built_pods_read_back() {
        let pod = object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT, |p| {
            prop(p, SPA_FORMAT_MEDIA_TYPE, |v| {
                pod_id(v, SPA_MEDIA_TYPE_VIDEO)
            });
            prop(p, SPA_FORMAT_VIDEO_FORMAT, |v| {
                pod_choice_id(
                    v,
                    SPA_CHOICE_ENUM,
                    &[
                        SPA_VIDEO_FORMAT_BGRX,
                        SPA_VIDEO_FORMAT_BGRX,
                        SPA_VIDEO_FORMAT_BGRA,
                    ],
                )
            });
            prop_flags(
                p,
                SPA_FORMAT_VIDEO_MODIFIER,
                SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE,
                |v| {
                    pod_choice_long(
                        v,
                        SPA_CHOICE_ENUM,
                        &[0x0300_0000_0000_0001, 0x0300_0000_0000_0001, 0],
                    )
                },
            );
            prop(p, SPA_FORMAT_VIDEO_SIZE, |v| {
                pod_choice_rect(v, SPA_CHOICE_RANGE, &[(1280, 720), (1, 1), (16384, 16384)])
            });
            prop(p, SPA_FORMAT_VIDEO_FRAMERATE, |v| pod_frac(v, 0, 1));
            prop(p, SPA_FORMAT_VIDEO_MAX_FRAMERATE, |v| {
                pod_choice_frac(v, SPA_CHOICE_RANGE, &[(60, 1), (1, 1), (60, 1)])
            });
            prop(p, SPA_FORMAT_VIDEO_MAX_FRAMERATE + 1, |v| pod_int(v, 4));
        });
        assert_eq!(pod.len() % 8, 0);
        let (ty, id, props) = object_props(&pod).unwrap();
        assert_eq!((ty, id), (SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT));
        assert_eq!(
            find_prop(&props, SPA_FORMAT_MEDIA_TYPE).unwrap().as_u32(),
            Some(SPA_MEDIA_TYPE_VIDEO)
        );
        assert_eq!(
            find_prop(&props, SPA_FORMAT_VIDEO_FORMAT).unwrap().as_u32(),
            Some(SPA_VIDEO_FORMAT_BGRX)
        );
        let modifier = find_prop(&props, SPA_FORMAT_VIDEO_MODIFIER).unwrap();
        assert_eq!(
            modifier.flags,
            SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE
        );
        assert_eq!(modifier.as_i64(), Some(0x0300_0000_0000_0001));
        assert_eq!(
            find_prop(&props, SPA_FORMAT_VIDEO_SIZE).unwrap().as_pair(),
            Some((1280, 720))
        );
        assert_eq!(
            find_prop(&props, SPA_FORMAT_VIDEO_FRAMERATE)
                .unwrap()
                .as_pair(),
            Some((0, 1))
        );
        assert_eq!(
            find_prop(&props, SPA_FORMAT_VIDEO_MAX_FRAMERATE)
                .unwrap()
                .as_pair(),
            Some((60, 1))
        );
        assert_eq!(
            find_prop(&props, SPA_FORMAT_VIDEO_MAX_FRAMERATE + 1)
                .unwrap()
                .as_u32(),
            Some(4)
        );
        assert!(find_prop(&props, SPA_FORMAT_MEDIA_SUBTYPE).is_none());
        let bytes = unsafe { pod_bytes(pod.as_ptr() as *const c_void) };
        assert_eq!(bytes.len(), pod.len());
    }
}
