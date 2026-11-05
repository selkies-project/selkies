/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bindings for the software codec libraries pixelflux links, one module per library,
//! generated at build time from the headers of the library pkg-config located (`build.rs`).
//! A library is present only under its feature; the wheel enables all of them.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::all)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unnecessary_transmutes)]
#![allow(improper_ctypes)]

/// libvpx: the VP8 and VP9 encoders and decoders.
#[cfg(feature = "vpx")]
pub mod vpx {
    include!(concat!(env!("OUT_DIR"), "/vpx.rs"));
}

/// x265: the GPL HEVC encoder, reached through its versioned API table.
#[cfg(feature = "x265")]
pub mod x265 {
    include!(concat!(env!("OUT_DIR"), "/x265.rs"));

    /// `api.encoder_encode`, whose output takes an array of pictures, or on builds 210 to
    /// 212 an array of pointers to them; `out` points at two pictures either way.
    ///
    /// # Safety
    /// The pointers are the library's own, and `out` addresses two initialized pictures.
    pub unsafe fn encoder_encode(
        api: &x265_api,
        encoder: *mut x265_encoder,
        nals: *mut *mut x265_nal,
        count: *mut u32,
        pic_in: *mut x265_picture,
        out: *mut x265_picture,
    ) -> ::std::os::raw::c_int {
        let encode = api
            .encoder_encode
            .expect("x265's API table names encoder_encode");
        #[cfg(x265_layer_pointers)]
        {
            let mut layers = [out, unsafe { out.add(1) }];
            unsafe { encode(encoder, nals, count, pic_in, layers.as_mut_ptr()) }
        }
        #[cfg(not(x265_layer_pointers))]
        {
            unsafe { encode(encoder, nals, count, pic_in, out) }
        }
    }
}

/// kvazaar: the BSD HEVC encoder, reached through its API table.
#[cfg(feature = "kvazaar")]
pub mod kvazaar {
    include!(concat!(env!("OUT_DIR"), "/kvazaar.rs"));
}

/// SVT-AV1: the AV1 encoder. `svtav1_handle_priv` is set for a release before 3.0, whose
/// handle takes an application pointer, `svtav1_rtc` for one from 3.1, which has the
/// real-time mode, and `svtav1_events` for one whose header names the reference-management
/// events (4.2 on), the release whose low-delay constant-rate session takes a new bitrate with
/// any picture.
#[cfg(feature = "svtav1")]
pub mod svtav1 {
    include!(concat!(env!("OUT_DIR"), "/svtav1.rs"));
    pub const HAS_RTC: bool = cfg!(svtav1_rtc);
    pub const HAS_EVENTS: bool = cfg!(svtav1_events);

    /// What a picture carries to a running encoder besides its pixels, where the release takes it
    /// (`HAS_EVENTS`): the constant-rate target in bits per second and the preset from that
    /// picture on, and the ids of the anchors to store the picture as, to release, and to predict
    /// it from alone; zero leaves each out.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Events {
        pub target_bit_rate: u32,
        pub store: u32,
        pub clear: u32,
        pub predict_from: u32,
        pub preset: i8,
    }

    /// `svt_av1_enc_send_picture`, with `events` attached where the release takes them.
    ///
    /// # Safety
    /// `handle` is an initialized encoder and `header` a picture it may read.
    pub unsafe fn send_picture(
        handle: *mut EbComponentType,
        header: *mut EbBufferHeaderType,
        events: Events,
    ) -> EbErrorType {
        #[cfg(svtav1_events)]
        if events != Events::default() {
            fn node<T>(node_type: PrivDataType, data: &mut T) -> EbPrivDataNode {
                let size = std::mem::size_of::<T>() as u32;
                EbPrivDataNode {
                    node_type,
                    data: (data as *mut T).cast(),
                    size,
                    next: std::ptr::null_mut(),
                }
            }
            let mut rate = SvtAv1RateInfo {
                seq_qp: 0,
                target_bit_rate: events.target_bit_rate,
            };
            let mut ids = [events.store, events.clear, events.predict_from]
                .map(|pic_id| SvtAv1RefFrameCmd { pic_id });
            let mut preset = SvtAv1PresetInfo {
                enc_mode: events.preset,
            };
            let mut nodes = Vec::with_capacity(5);
            if events.target_bit_rate != 0 {
                nodes.push(node(RATE_CHANGE_EVENT, &mut rate));
            }
            if events.preset != 0 {
                nodes.push(node(PRESET_CHANGE_EVENT, &mut preset));
            }
            for (node_type, cmd) in [REF_STORE_EVENT, REF_CLEAR_EVENT, REF_USE_EVENT]
                .into_iter()
                .zip(&mut ids)
            {
                if cmd.pic_id != 0 {
                    nodes.push(node(node_type, cmd));
                }
            }
            for i in 1..nodes.len() {
                let next: *mut EbPrivDataNode = &mut nodes[i];
                nodes[i - 1].next = next;
            }
            unsafe {
                (*header).p_app_private = nodes.as_mut_ptr().cast();
                let code = svt_av1_enc_send_picture(handle, header);
                (*header).p_app_private = std::ptr::null_mut();
                return code;
            }
        }
        #[cfg(not(svtav1_events))]
        let _ = events;
        unsafe { svt_av1_enc_send_picture(handle, header) }
    }

    /// Hold `count` anchors for the application where the release manages them (`HAS_EVENTS`).
    pub fn set_managed_refs(config: &mut EbSvtAv1EncConfiguration, count: u8) {
        #[cfg(svtav1_events)]
        {
            config.max_managed_refs = count;
        }
        #[cfg(not(svtav1_events))]
        let _ = (config, count);
    }

    /// `svt_av1_enc_init_handle`, with the application pointer a release before 3.0 took.
    ///
    /// # Safety
    /// `handle` and `config` are valid for writes.
    pub unsafe fn init_handle(
        handle: *mut *mut EbComponentType,
        config: *mut EbSvtAv1EncConfiguration,
    ) -> EbErrorType {
        #[cfg(svtav1_handle_priv)]
        {
            unsafe { svt_av1_enc_init_handle(handle, ::std::ptr::null_mut(), config) }
        }
        #[cfg(not(svtav1_handle_priv))]
        {
            unsafe { svt_av1_enc_init_handle(handle, config) }
        }
    }
}

/// dav1d: the AV1 decoder.
#[cfg(feature = "dav1d")]
pub mod dav1d {
    include!(concat!(env!("OUT_DIR"), "/dav1d.rs"));
}

/// libde265: the HEVC decoder.
#[cfg(feature = "de265")]
pub mod de265 {
    include!(concat!(env!("OUT_DIR"), "/de265.rs"));
}
