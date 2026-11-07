//! PipeWire sink: the camera as a `Video/Source` node.
//!
//! Where a PipeWire daemon is reachable, the same frames that feed the interposer ring are also
//! offered as a PipeWire camera node, which PipeWire-native consumers (GStreamer `pipewiresrc`,
//! portal-aware applications) link to directly and which `pipewire-v4l2` exposes as a V4L2 device
//! to plain libc consumers. `libpipewire-0.3` is loaded at run time and the SPA pods are built by
//! hand, so pixelflux keeps no build-time or load-time dependency on PipeWire and the sink simply
//! stays off where the library or the daemon is absent.
//!
//! The stream is the graph driver: every published frame is copied into the node's latest-frame
//! slot and one graph cycle is triggered; the process callback then fills whatever buffer the
//! consumers negotiated (memfd-backed, mapped for us) from that slot. The library binding and the
//! pod encoding live in `crate::pipewire`, shared with the portal screen capture.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ring::{
    RingFormat, V4L2_PIX_FMT_MJPEG, V4L2_PIX_FMT_NV12, V4L2_PIX_FMT_YUV420, V4L2_PIX_FMT_YUYV,
};
use crate::pipewire::*;

fn spa_video_format(fourcc: u32) -> Option<u32> {
    match fourcc {
        V4L2_PIX_FMT_YUV420 => Some(SPA_VIDEO_FORMAT_I420),
        V4L2_PIX_FMT_NV12 => Some(SPA_VIDEO_FORMAT_NV12),
        V4L2_PIX_FMT_YUYV => Some(SPA_VIDEO_FORMAT_YUY2),
        _ => None,
    }
}

/// `SPA_PARAM_EnumFormat`: one video format at the device size and rate — raw with the given
/// SPA pixel format, or `video/mjpg` for the MJPEG device (`spa_format` None).
fn format_pod(fmt: &RingFormat, spa_format: Option<u32>) -> Vec<u8> {
    object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT, |p| {
        prop(p, SPA_FORMAT_MEDIA_TYPE, |v| {
            pod_id(v, SPA_MEDIA_TYPE_VIDEO)
        });
        match spa_format {
            Some(raw) => {
                prop(p, SPA_FORMAT_MEDIA_SUBTYPE, |v| {
                    pod_id(v, SPA_MEDIA_SUBTYPE_RAW)
                });
                prop(p, SPA_FORMAT_VIDEO_FORMAT, |v| pod_id(v, raw));
            }
            None => prop(p, SPA_FORMAT_MEDIA_SUBTYPE, |v| {
                pod_id(v, SPA_MEDIA_SUBTYPE_MJPG)
            }),
        }
        prop(p, SPA_FORMAT_VIDEO_SIZE, |v| {
            pod_rect(v, fmt.width, fmt.height)
        });
        prop(p, SPA_FORMAT_VIDEO_FRAMERATE, |v| {
            pod_frac(v, fmt.fps_num, fmt.fps_den)
        });
    })
}

/// `SPA_PARAM_Buffers` answered once the format is chosen: memfd (or plain memory) buffers of
/// exactly one frame.
fn buffers_pod(fmt: &RingFormat) -> Vec<u8> {
    object(SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS, |p| {
        prop(p, SPA_PARAM_BUFFERS_BUFFERS, |v| {
            pod_choice_int(v, SPA_CHOICE_RANGE, &[4, 2, 8])
        });
        prop(p, SPA_PARAM_BUFFERS_BLOCKS, |v| pod_int(v, 1));
        prop(p, SPA_PARAM_BUFFERS_SIZE, |v| {
            pod_int(v, fmt.sizeimage as i32)
        });
        prop(p, SPA_PARAM_BUFFERS_STRIDE, |v| {
            pod_int(v, fmt.bytesperline as i32)
        });
        prop(p, SPA_PARAM_BUFFERS_ALIGN, |v| pod_int(v, 16));
        prop(p, SPA_PARAM_BUFFERS_DATATYPE, |v| {
            pod_choice_int(
                v,
                SPA_CHOICE_FLAGS,
                &[(1 << SPA_DATA_MEMFD) | (1 << SPA_DATA_MEMPTR)],
            )
        });
    })
}

fn meta_pod() -> Vec<u8> {
    object(SPA_TYPE_OBJECT_PARAM_META, SPA_PARAM_META, |p| {
        prop(p, SPA_PARAM_META_TYPE, |v| pod_id(v, SPA_META_HEADER));
        prop(p, SPA_PARAM_META_SIZE, |v| {
            pod_int(v, SPA_META_HEADER_SIZE as i32)
        });
    })
}

// --- the sink ---------------------------------------------------------------------------------

/// State shared with the stream callbacks (handed to PipeWire as the listener's user data).
struct Shared {
    api: Api,
    stream: AtomicPtr<c_void>,
    frame: Mutex<Vec<u8>>,
    frame_seq: AtomicU64,
    frame_ts_ns: AtomicU64,
    state: AtomicI32,
    stride: i32,
    buffers_pod: Vec<u8>,
    meta_pod: Vec<u8>,
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    new: c_int,
    error: *const c_char,
) {
    unsafe {
        let shared = &*(data as *const Shared);
        shared.state.store(new, Ordering::Release);
        if new == PW_STREAM_STATE_ERROR {
            let msg = if error.is_null() {
                String::new()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            };
            eprintln!("[webcam] PipeWire stream error: {}", msg);
        }
    }
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    unsafe {
        let shared = &*(data as *const Shared);
        let stream = shared.stream.load(Ordering::Acquire);
        if stream.is_null() {
            return;
        }
        let mut params: [*const c_void; 2] = [
            shared.buffers_pod.as_ptr() as *const c_void,
            shared.meta_pod.as_ptr() as *const c_void,
        ];
        (shared.api.stream_update_params)(stream, params.as_mut_ptr(), 2);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    unsafe {
        let shared = &*(data as *const Shared);
        let stream = shared.stream.load(Ordering::Acquire);
        if stream.is_null() {
            return;
        }
        let frame = shared.frame.lock().unwrap_or_else(|e| e.into_inner());
        if frame.is_empty() {
            return;
        }
        let b = (shared.api.stream_dequeue_buffer)(stream);
        if b.is_null() {
            return;
        }
        let spa_buf = (*b).buffer;
        if !spa_buf.is_null() && (*spa_buf).n_datas >= 1 {
            let d = &mut *(*spa_buf).datas;
            let mut written = 0usize;
            if !d.data.is_null() && !d.chunk.is_null() {
                written = frame.len().min(d.maxsize as usize);
                ptr::copy_nonoverlapping(frame.as_ptr(), d.data as *mut u8, written);
                (*d.chunk).offset = 0;
                (*d.chunk).size = written as u32;
                (*d.chunk).stride = shared.stride;
                (*d.chunk).flags = 0;
            }
            for i in 0..(*spa_buf).n_metas as usize {
                let m = &*(*spa_buf).metas.add(i);
                if m.type_ == SPA_META_HEADER && m.size >= SPA_META_HEADER_SIZE && !m.data.is_null()
                {
                    let h = &mut *(m.data as *mut SpaMetaHeader);
                    h.flags = 0;
                    h.offset = 0;
                    h.pts = shared.frame_ts_ns.load(Ordering::Relaxed) as i64;
                    h.dts_offset = 0;
                    h.seq = shared.frame_seq.load(Ordering::Relaxed);
                }
            }
            (*b).size = written as u64;
        }
        (shared.api.stream_queue_buffer)(stream, b);
    }
}

pub struct PipeWireSink {
    api: Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<[u64; 8]>,
    _events: Box<PwStreamEvents>,
    _format_pod: Vec<u8>,
    shared: Arc<Shared>,
    failed: bool,
}

unsafe impl Send for PipeWireSink {}

impl PipeWireSink {
    /// Connect to the PipeWire daemon and publish the camera node; fails (and the sink stays off)
    /// when the library or the daemon is missing or the format has no PipeWire equivalent.
    pub fn connect(node_name: &str, description: &str, fmt: &RingFormat) -> Result<Self, String> {
        let api = *api()?;
        // The node name reaches here from Python, and a C string is what PipeWire takes.
        let node_name = CString::new(node_name)
            .map_err(|_| "node.name must not contain NUL bytes".to_string())?;
        let description = CString::new(description)
            .map_err(|_| "node.description must not contain NUL bytes".to_string())?;
        let spa_format = if fmt.fourcc == V4L2_PIX_FMT_MJPEG {
            None
        } else {
            Some(
                spa_video_format(fmt.fourcc)
                    .ok_or_else(|| "pixel format has no PipeWire equivalent".to_string())?,
            )
        };
        unsafe {
            let thread_loop = (api.thread_loop_new)(c"pixelflux-webcam-pw".as_ptr(), ptr::null());
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".into());
            }
            let mut sink = PipeWireSink {
                api,
                thread_loop,
                context: ptr::null_mut(),
                core: ptr::null_mut(),
                stream: ptr::null_mut(),
                _hook: Box::new([0u64; 8]),
                _events: Box::new(PwStreamEvents {
                    state_changed: Some(on_state_changed),
                    param_changed: Some(on_param_changed),
                    process: Some(on_process),
                    ..PwStreamEvents::empty()
                }),
                _format_pod: format_pod(fmt, spa_format),
                shared: Arc::new(Shared {
                    api,
                    stream: AtomicPtr::new(ptr::null_mut()),
                    frame: Mutex::new(Vec::new()),
                    frame_seq: AtomicU64::new(0),
                    frame_ts_ns: AtomicU64::new(0),
                    state: AtomicI32::new(0),
                    stride: fmt.bytesperline as i32,
                    buffers_pod: buffers_pod(fmt),
                    meta_pod: meta_pod(),
                }),
                failed: false,
            };
            let pw_loop = (api.thread_loop_get_loop)(thread_loop);
            sink.context = (api.context_new)(pw_loop, ptr::null_mut(), 0);
            if sink.context.is_null() {
                return Err("pw_context_new failed".into());
            }
            if (api.thread_loop_start)(thread_loop) < 0 {
                return Err("pw_thread_loop_start failed".into());
            }
            (api.thread_loop_lock)(thread_loop);
            let result = (|| -> Result<(), String> {
                sink.core = (api.context_connect)(sink.context, ptr::null_mut(), 0);
                if sink.core.is_null() {
                    return Err("no PipeWire daemon reachable".into());
                }
                let props = (api.properties_new)(ptr::null::<c_char>());
                if props.is_null() {
                    return Err("pw_properties_new failed".into());
                }
                for (k, v) in [
                    (c"media.class", c"Video/Source" as &CStr),
                    (c"media.role", c"Camera"),
                    (c"node.name", node_name.as_c_str()),
                    (c"node.description", description.as_c_str()),
                    (c"node.virtual", c"true"),
                ] {
                    (api.properties_set)(props, k.as_ptr(), v.as_ptr());
                }
                sink.stream = (api.stream_new)(sink.core, description.as_ptr(), props);
                if sink.stream.is_null() {
                    return Err("pw_stream_new failed".into());
                }
                sink.shared.stream.store(sink.stream, Ordering::Release);
                let user_data = Arc::as_ptr(&sink.shared) as *mut c_void;
                (api.stream_add_listener)(
                    sink.stream,
                    sink._hook.as_mut_ptr() as *mut c_void,
                    &*sink._events,
                    user_data,
                );
                let mut params: [*const c_void; 1] = [sink._format_pod.as_ptr() as *const c_void];
                let rc = (api.stream_connect)(
                    sink.stream,
                    PW_DIRECTION_OUTPUT,
                    PW_ID_ANY,
                    PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_DRIVER,
                    params.as_mut_ptr(),
                    1,
                );
                if rc < 0 {
                    return Err(format!("pw_stream_connect failed ({})", rc));
                }
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let st = sink.shared.state.load(Ordering::Acquire);
                if st == PW_STREAM_STATE_ERROR {
                    return Err("PipeWire stream entered the error state".into());
                }
                if st == PW_STREAM_STATE_PAUSED
                    || st == PW_STREAM_STATE_STREAMING
                    || Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(sink)
        }
    }

    pub fn is_failed(&self) -> bool {
        self.failed || self.shared.state.load(Ordering::Acquire) == PW_STREAM_STATE_ERROR
    }

    /// Whether a consumer is linked (the node is running); the latest frame is always kept so a
    /// consumer that links later starts from it.
    pub fn is_streaming(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == PW_STREAM_STATE_STREAMING
    }

    /// Store one device frame and schedule a graph cycle so linked consumers receive it. With no
    /// consumer linked nothing is copied; the first frame after a link fills the slot.
    pub fn publish(&mut self, frame: &[u8], ts_ns: u64) {
        if self.is_failed() || !self.is_streaming() {
            return;
        }
        {
            let mut slot = self.shared.frame.lock().unwrap_or_else(|e| e.into_inner());
            slot.clear();
            slot.extend_from_slice(frame);
        }
        self.shared.frame_seq.fetch_add(1, Ordering::Relaxed);
        self.shared.frame_ts_ns.store(ts_ns, Ordering::Relaxed);
        unsafe {
            (self.api.thread_loop_lock)(self.thread_loop);
            (self.api.stream_trigger_process)(self.stream);
            (self.api.thread_loop_unlock)(self.thread_loop);
        }
    }
}

impl Drop for PipeWireSink {
    fn drop(&mut self) {
        unsafe {
            (self.api.thread_loop_lock)(self.thread_loop);
            self.shared.stream.store(ptr::null_mut(), Ordering::Release);
            if !self.stream.is_null() {
                (self.api.stream_disconnect)(self.stream);
                (self.api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (self.api.core_disconnect)(self.core);
            }
            (self.api.thread_loop_unlock)(self.thread_loop);
            (self.api.thread_loop_stop)(self.thread_loop);
            if !self.context.is_null() {
                (self.api.context_destroy)(self.context);
            }
            (self.api.thread_loop_destroy)(self.thread_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(v: &[u8], off: usize) -> u32 {
        u32::from_ne_bytes(v[off..off + 4].try_into().unwrap())
    }

    /// One key of an object pod: its id, value type, and value payload.
    type PodKey = (u32, u32, Vec<u8>);

    /// Walk an object pod: (object type, param id, [keys]).
    fn parse_object(pod: &[u8]) -> (u32, u32, Vec<PodKey>) {
        assert_eq!(pod.len() % 8, 0);
        let (ty, id, props) = object_props(pod).unwrap();
        (
            ty,
            id,
            props
                .into_iter()
                .map(|p| (p.key, p.ty, p.payload.to_vec()))
                .collect(),
        )
    }

    #[test]
    fn format_pod_is_a_well_formed_object() {
        let fmt = RingFormat::raw(V4L2_PIX_FMT_YUV420, 1280, 720, 30, 1).unwrap();
        let (ty, id, props) = parse_object(&format_pod(&fmt, Some(SPA_VIDEO_FORMAT_I420)));
        assert_eq!((ty, id), (SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT));
        let get = |k: u32| props.iter().find(|p| p.0 == k).cloned().unwrap();
        assert_eq!(get(SPA_FORMAT_MEDIA_TYPE).1, SPA_TYPE_ID);
        assert_eq!(
            u32_at(&get(SPA_FORMAT_MEDIA_TYPE).2, 0),
            SPA_MEDIA_TYPE_VIDEO
        );
        assert_eq!(
            u32_at(&get(SPA_FORMAT_MEDIA_SUBTYPE).2, 0),
            SPA_MEDIA_SUBTYPE_RAW
        );
        assert_eq!(
            u32_at(&get(SPA_FORMAT_VIDEO_FORMAT).2, 0),
            SPA_VIDEO_FORMAT_I420
        );
        let size = get(SPA_FORMAT_VIDEO_SIZE);
        assert_eq!(size.1, SPA_TYPE_RECTANGLE);
        assert_eq!((u32_at(&size.2, 0), u32_at(&size.2, 4)), (1280, 720));
        let rate = get(SPA_FORMAT_VIDEO_FRAMERATE);
        assert_eq!(rate.1, SPA_TYPE_FRACTION);
        assert_eq!((u32_at(&rate.2, 0), u32_at(&rate.2, 4)), (30, 1));
    }

    #[test]
    fn mjpg_format_pod_names_no_pixel_format() {
        let fmt = RingFormat::compressed(V4L2_PIX_FMT_MJPEG, 640, 480, 30, 1).unwrap();
        let (ty, id, props) = parse_object(&format_pod(&fmt, None));
        assert_eq!((ty, id), (SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT));
        let get = |k: u32| props.iter().find(|p| p.0 == k).cloned();
        assert_eq!(
            u32_at(&get(SPA_FORMAT_MEDIA_SUBTYPE).unwrap().2, 0),
            SPA_MEDIA_SUBTYPE_MJPG
        );
        assert!(get(SPA_FORMAT_VIDEO_FORMAT).is_none());
        let size = get(SPA_FORMAT_VIDEO_SIZE).unwrap();
        assert_eq!((u32_at(&size.2, 0), u32_at(&size.2, 4)), (640, 480));
        let (_, _, bprops) = parse_object(&buffers_pod(&fmt));
        let bget = |k: u32| bprops.iter().find(|p| p.0 == k).cloned().unwrap();
        assert_eq!(u32_at(&bget(SPA_PARAM_BUFFERS_SIZE).2, 0), 640 * 480 * 2);
        assert_eq!(u32_at(&bget(SPA_PARAM_BUFFERS_STRIDE).2, 0), 0);
    }

    #[test]
    fn buffers_pod_carries_frame_geometry() {
        let fmt = RingFormat::raw(V4L2_PIX_FMT_YUYV, 640, 480, 30, 1).unwrap();
        let (ty, id, props) = parse_object(&buffers_pod(&fmt));
        assert_eq!((ty, id), (SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS));
        let get = |k: u32| props.iter().find(|p| p.0 == k).cloned().unwrap();
        assert_eq!(
            (
                get(SPA_PARAM_BUFFERS_SIZE).1,
                u32_at(&get(SPA_PARAM_BUFFERS_SIZE).2, 0)
            ),
            (SPA_TYPE_INT, 640 * 480 * 2)
        );
        assert_eq!(u32_at(&get(SPA_PARAM_BUFFERS_STRIDE).2, 0), 1280);
        assert_eq!(u32_at(&get(SPA_PARAM_BUFFERS_BLOCKS).2, 0), 1);
        let buffers = get(SPA_PARAM_BUFFERS_BUFFERS);
        assert_eq!(buffers.1, SPA_TYPE_CHOICE);
        assert_eq!(u32_at(&buffers.2, 0), SPA_CHOICE_RANGE);
        assert_eq!(
            (u32_at(&buffers.2, 8), u32_at(&buffers.2, 12)),
            (4, SPA_TYPE_INT)
        );
        assert_eq!(&buffers.2[16..28], &[4u8, 0, 0, 0, 2, 0, 0, 0, 8, 0, 0, 0]);
        let dt = get(SPA_PARAM_BUFFERS_DATATYPE);
        assert_eq!(u32_at(&dt.2, 0), SPA_CHOICE_FLAGS);
        assert_eq!(
            u32_at(&dt.2, 16),
            (1 << SPA_DATA_MEMFD) | (1 << SPA_DATA_MEMPTR)
        );
        let (mty, mid, mprops) = parse_object(&meta_pod());
        assert_eq!((mty, mid), (SPA_TYPE_OBJECT_PARAM_META, SPA_PARAM_META));
        assert_eq!(u32_at(&mprops[0].2, 0), SPA_META_HEADER);
        assert_eq!(u32_at(&mprops[1].2, 0), SPA_META_HEADER_SIZE);
    }
}
