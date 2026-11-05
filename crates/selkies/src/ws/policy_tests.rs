/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the settings decide for a display: the rate-control and
//! paint-over defaults, the encoder names, the software-encode policy, the client sanitizer,
//! and the backends the host serves.

use serde_json::{Value as Json, json};

use crate::settings::{
    CPU_ONLY_ENCODERS, ENCODER_CODECS, Settings, WEBRTC_ENCODER_CHOICES, codec_for_encoder,
    effective_cpu_encode_threading, effective_use_cpu, encoder_for_codec, software_encoders,
    software_video_path,
};

fn parse(env: &[(&str, &str)]) -> Settings {
    let owned: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Settings::parse(&[], &move |name| {
        owned
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
}

#[test]
fn rate_control_defaults_to_cbr_and_an_operator_pin_wins() {
    for encoder in ["h264enc", "jpeg", "h265enc", "vp8enc", "vp9enc", "av1enc"] {
        let s = parse(&[("SELKIES_MODE", "websockets"), ("SELKIES_ENCODER", encoder)]);
        assert_eq!(s.rate_control_mode(), "cbr", "{encoder}");
    }
    assert_eq!(
        parse(&[("SELKIES_MODE", "webrtc")]).rate_control_mode(),
        "cbr"
    );
    for extra in [("SELKIES_USE_CPU", "true"), ("SELKIES_GPU_ID", "-1")] {
        assert_eq!(
            parse(&[("SELKIES_ENCODER", "h264enc"), extra]).rate_control_mode(),
            "cbr"
        );
    }
    assert_eq!(
        parse(&[("SELKIES_RATE_CONTROL_MODE", "crf")]).rate_control_mode(),
        "crf"
    );
    assert_eq!(
        parse(&[
            ("SELKIES_MODE", "webrtc"),
            ("SELKIES_RATE_CONTROL_MODE", "crf")
        ])
        .rate_control_mode(),
        "crf"
    );
    let disabled = parse(&[
        ("SELKIES_ENABLE_RATE_CONTROL", "false"),
        ("SELKIES_RATE_CONTROL_MODE", "cbr"),
    ]);
    assert_eq!(
        disabled.rate_control_mode(),
        "crf",
        "disabled rate control forces crf"
    );
    assert_eq!(
        disabled.enum_allowed("rate_control_mode"),
        vec!["crf"],
        "and publishes a crf-only menu"
    );
}

#[test]
fn encoder_aliases_and_the_published_menu() {
    assert_eq!(
        parse(&[("SELKIES_ENCODER", "openh264enc")]).encoder(),
        "h264enc"
    );
    assert_eq!(
        parse(&[("SELKIES_ENCODER", "x264enc")]).encoder(),
        "h264enc"
    );
    let s = parse(&[]);
    for alias in ["openh264enc", "X264ENC"] {
        assert_eq!(
            s.sanitize_client_setting("encoder", Some(&json!(alias)), "t"),
            Some(json!("h264enc")),
            "{alias}"
        );
    }
    assert_eq!(
        s.enum_allowed("encoder"),
        vec!["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "jpeg"]
    );
    let webrtc = parse(&[("SELKIES_MODE", "webrtc"), ("SELKIES_ENCODER", "jpeg")]);
    assert_eq!(
        webrtc.encoder(),
        "h264enc",
        "webrtc boot falls a websockets-only encoder back"
    );
    assert_eq!(
        parse(&[("SELKIES_MODE", "webrtc")]).enum_allowed("encoder"),
        WEBRTC_ENCODER_CHOICES
    );
    let pinned = parse(&[("SELKIES_ENCODER", "jpeg")]);
    assert_eq!(
        pinned.enum_allowed("encoder"),
        vec!["jpeg"],
        "an operator pin narrows the menu"
    );
    assert_eq!(
        pinned.sanitize_client_setting("encoder", Some(&json!("h264enc")), "t"),
        Some(json!("jpeg")),
        "a client cannot escape an operator pin"
    );
}

#[test]
fn a_live_transport_switch_round_trips_the_pin_and_the_pick() {
    let mut s = parse(&[("SELKIES_ENCODER", "jpeg")]);
    s.set_str("mode", "webrtc".into());
    s.apply_webrtc_encoder_filter();
    assert_eq!(s.encoder(), "h264enc", "a live switch clamps");
    s.set_str("mode", "websockets".into());
    s.apply_webrtc_encoder_filter();
    assert_eq!(s.encoder(), "jpeg", "switching back restores the pin");
    assert_eq!(s.enum_allowed("encoder"), vec!["jpeg"]);

    let mut s = parse(&[]);
    s.set_str("encoder", "jpeg".into());
    s.encoder_client_set = true;
    s.set_str("mode", "webrtc".into());
    s.apply_webrtc_encoder_filter();
    assert_eq!(s.encoder(), "h264enc");
    s.set_str("mode", "websockets".into());
    s.apply_webrtc_encoder_filter();
    assert_eq!(
        s.encoder(),
        "jpeg",
        "a client's websockets-only pick survives a webrtc round trip"
    );

    let mut s = parse(&[]);
    s.set_str("encoder", "jpeg".into());
    s.encoder_client_set = true;
    s.set_str("mode", "webrtc".into());
    s.apply_webrtc_encoder_filter();
    s.set_str("encoder", "h264enc".into());
    s.encoder_client_set = true;
    s.set_str("mode", "websockets".into());
    s.apply_webrtc_encoder_filter();
    assert_eq!(
        s.encoder(),
        "h264enc",
        "a fresh pick during the webrtc leg wins"
    );
}

#[test]
fn paint_over_follows_turbo_and_the_encoder() {
    let on = |env: &[(&str, &str)]| parse(env).use_paint_over_quality().on;
    for mode in ["websockets", "webrtc"] {
        assert!(
            !on(&[("SELKIES_MODE", mode)]),
            "{mode} defaults it off under Turbo"
        );
        assert!(
            on(&[
                ("SELKIES_MODE", mode),
                ("SELKIES_VIDEO_STREAMING_MODE", "false")
            ]),
            "{mode} on without Turbo"
        );
        assert!(!on(&[
            ("SELKIES_MODE", mode),
            ("SELKIES_RATE_CONTROL_MODE", "crf")
        ]));
        assert!(
            on(&[
                ("SELKIES_MODE", mode),
                ("SELKIES_USE_PAINT_OVER_QUALITY", "true")
            ]),
            "an operator's stands"
        );
        assert!(!on(&[
            ("SELKIES_MODE", mode),
            ("SELKIES_VIDEO_STREAMING_MODE", "false"),
            ("SELKIES_USE_PAINT_OVER_QUALITY", "false")
        ]));
    }
    assert!(
        on(&[("SELKIES_ENCODER", "jpeg")]),
        "jpeg defaults paint-over on under Turbo"
    );
}

#[test]
fn software_encode_threading() {
    let threading = |env: &[(&str, &str)]| {
        let s = parse(env);
        let b = s.cpu_encode_threading();
        (b.on, b.locked)
    };
    assert_eq!(threading(&[]), (true, false));
    assert_eq!(
        threading(&[("SELKIES_CPU_ENCODE_THREADING", "false")]),
        (false, false)
    );
    assert_eq!(
        threading(&[("SELKIES_CPU_ENCODE_THREADING", "false|locked")]),
        (false, true)
    );
    assert_eq!(
        threading(&[("SELKIES_ENCODER", "vp8enc")]),
        (false, false),
        "unset, vp8enc runs one thread"
    );
    assert_eq!(
        threading(&[
            ("SELKIES_ENCODER", "vp8enc"),
            ("SELKIES_CPU_ENCODE_THREADING", "true")
        ]),
        (true, false)
    );
    assert_eq!(
        threading(&[("SELKIES_ENCODER", "vp8enc,h264enc")]),
        (false, false),
        "the first encoder decides"
    );
    assert_eq!(
        threading(&[("SELKIES_ENCODER", "h264enc,vp8enc")]),
        (true, false)
    );
    for (encoder, requested, default, provided, want) in [
        ("vp8enc", None, true, false, false),
        ("vp9enc", None, true, false, true),
        ("vp8enc", Some(true), true, false, true),
        ("h264enc", Some(false), true, false, false),
        ("vp8enc", None, true, true, true),
        ("h264enc", None, false, true, false),
        ("vp8enc", Some(false), true, true, false),
    ] {
        assert_eq!(
            effective_cpu_encode_threading(encoder, requested, default, provided),
            want,
            "{encoder} {requested:?}"
        );
    }
    assert!(crate::settings::STREAM_SETTINGS.contains(&"cpu_encode_threading"));
    let base = pixelflux::CaptureSettings::default();
    let threaded = pixelflux::CaptureSettings {
        cpu_encode_threading: !base.cpu_encode_threading,
        ..base.clone()
    };
    assert!(
        !crate::display::structurally_same(&base, &threaded),
        "a change rebuilds a capture"
    );
}

#[test]
fn software_encoding_policy_across_encoder_switches() {
    for encoder in CPU_ONLY_ENCODERS {
        assert!(
            effective_use_cpu(encoder, Some(false), false),
            "{encoder} forces software"
        );
    }
    assert!(!effective_use_cpu("h264enc", None, false));
    assert!(effective_use_cpu("jpeg", None, false));
    assert!(
        !effective_use_cpu("h264enc", None, false),
        "returning to h264enc restores hardware"
    );
    assert!(
        effective_use_cpu("h264enc", Some(true), false),
        "an explicit software choice is kept"
    );
    assert!(
        effective_use_cpu("jpeg", Some(true), false)
            && effective_use_cpu("h264enc", Some(true), false)
    );
    assert!(
        !effective_use_cpu("h264enc", Some(false), true),
        "an explicit hardware choice is honored"
    );
    assert!(
        effective_use_cpu("h264enc", None, true),
        "the server default applies when the client never chose"
    );
}

#[test]
fn encoder_names_and_their_codecs() {
    let allowed = parse(&[]).enum_allowed("encoder");
    let mapped: Vec<&str> = ENCODER_CODECS.iter().map(|(e, _)| *e).collect();
    assert!(allowed.iter().all(|e| mapped.contains(&e.as_str())));
    assert!(mapped.iter().all(|e| allowed.iter().any(|a| a == e)));
    for (encoder, codec) in [
        ("jpeg", "jpeg"),
        ("h264enc", "h264"),
        ("h265enc", "h265"),
        ("vp8enc", "vp8"),
        ("vp9enc", "vp9"),
        ("av1enc", "av1"),
        ("openh264enc", "h264"),
        ("nvh264enc", "h264"),
    ] {
        assert_eq!(codec_for_encoder(encoder), codec, "{encoder}");
    }
    assert!(
        !software_video_path("jpeg", true),
        "jpeg is never a software video path"
    );
    for encoder in ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc"] {
        assert!(
            !software_video_path(encoder, false) && software_video_path(encoder, true),
            "{encoder}"
        );
    }
    let codecs: Vec<&str> = ["h264", "h265", "vp8", "vp9", "av1", "jpeg"]
        .map(encoder_for_codec)
        .to_vec();
    assert_eq!(
        codecs,
        ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "jpeg"]
    );
    assert_eq!(
        encoder_for_codec("mpeg2"),
        "h264enc",
        "an unknown codec lands on H.264"
    );
}

#[test]
fn the_encode_node_and_the_backends_the_host_serves() {
    use crate::settings::AUTO_ENCODE_NODE;
    assert_eq!(parse(&[]).encode_node_index(), Some(AUTO_ENCODE_NODE));
    assert_eq!(
        parse(&[("SELKIES_GPU_ID", "1")]).encode_node_index(),
        Some(1)
    );
    assert_eq!(parse(&[("SELKIES_GPU_ID", "-1")]).encode_node_index(), None);
    assert_eq!(
        parse(&[("SELKIES_ENCODE_DRI", "/dev/dri/renderD130")]).encode_node_index(),
        Some(2)
    );
    assert_eq!(
        parse(&[("SELKIES_USE_CPU", "true|locked")]).encode_node_index(),
        None
    );

    let unresolved = parse(&[]);
    assert!(unresolved.encoder_backends().is_none());
    assert!(!unresolved.client_payload().contains_key("encoder_backends"));
    assert!(
        ENCODER_CODECS
            .iter()
            .all(|(e, _)| unresolved.encoder_served(e))
    );

    let mut s = parse(&[("SELKIES_GPU_ID", "-1")]);
    s.resolve_encoder_backends();
    let table = s.encoder_backends().expect("resolved without a probe");
    let software = software_encoders();
    assert_eq!(table.len(), 5);
    for (codec, backends) in &table {
        assert!(backends.hardware.is_none(), "{codec}");
        assert_eq!(
            backends.software.as_deref(),
            software.get(codec).copied(),
            "{codec}"
        );
    }
    assert!(
        s.encoder_served("jpeg"),
        "a CPU-only encoder is always served"
    );
    let payload = s.client_payload();
    assert!(payload.contains_key("encoder_backends"));
    assert!(
        s.enum_allowed("encoder")
            .iter()
            .all(|e| s.encoder_served(e))
    );

    let mut narrowed = parse(&[("SELKIES_GPU_ID", "-1"), ("SELKIES_ENCODER", "vp9enc,jpeg")]);
    narrowed.resolve_encoder_backends();
    assert!(
        narrowed
            .enum_allowed("encoder")
            .iter()
            .all(|e| e == "vp9enc" || e == "jpeg"),
        "narrowed, not replaced"
    );
}

#[test]
fn the_client_setting_sanitizer() {
    let framed = parse(&[("SELKIES_FRAMERATE", "30-120")]);
    let framerate =
        |value: Option<Json>| framed.sanitize_client_setting("framerate", value.as_ref(), "t");
    assert_eq!(framerate(Some(json!(60))), Some(json!(60)));
    assert_eq!(
        framerate(Some(json!(240))),
        Some(json!(120)),
        "clamped to the operator's range"
    );
    assert_eq!(framerate(Some(json!(5))), Some(json!(30)));
    assert_eq!(
        framerate(Some(json!(59.5))),
        Some(json!(59.5)),
        "a fractional value stays fractional"
    );
    assert_eq!(
        framerate(Some(json!(60.0))),
        Some(json!(60)),
        "an integral value comes back an int"
    );
    assert_eq!(
        framerate(None),
        Some(json!(60)),
        "no value asks for the built-in default"
    );
    let pinned = parse(&[("SELKIES_FRAMERATE", "24-24")]);
    assert_eq!(
        pinned.sanitize_client_setting("framerate", None, "t"),
        Some(json!(24))
    );

    for value in ["inf", "nan", "-inf", "1e309"] {
        let s = parse(&[("SELKIES_FRAMERATE", value)]);
        let (lo, hi) = s.framerate();
        assert_eq!(
            (lo.as_f64(), hi.as_f64()),
            (8.0, 240.0),
            "{value} keeps the built-in span"
        );
    }
    let s = parse(&[("SELKIES_FRAMERATE", "60,8-120")]);
    assert_eq!(s.framerate().1.as_f64(), 120.0);
    assert_eq!(s.range_default("framerate").unwrap().as_f64(), 60.0);

    let mut crf = parse(&[]);
    crf.set_str("rate_control_mode", "crf".into());
    let mode = |v: &str| crf.sanitize_client_setting("rate_control_mode", Some(&json!(v)), "t");
    assert_eq!(mode("cbr"), Some(json!("cbr")));
    assert_eq!(
        mode("vbr"),
        Some(json!("crf")),
        "off the list falls back to the server's own, not the first stop"
    );
    assert_eq!(
        crf.sanitize_client_setting("rate_control_mode", None, "t"),
        Some(json!("crf"))
    );
    let mut vbr = parse(&[]);
    vbr.set_str("rate_control_mode", "vbr".into());
    assert_eq!(
        vbr.sanitize_client_setting("rate_control_mode", Some(&json!("vbr")), "t"),
        Some(json!("vbr")),
        "an operator value outside the list is still agreed with when echoed back"
    );
    let mut audio = parse(&[]);
    audio.set_str("audio_bitrate", "128000".into());
    assert_eq!(
        audio.sanitize_client_setting("audio_bitrate", Some(&json!(128000)), "t"),
        Some(json!("128000")),
        "a numeric stop comes back as the string the list holds"
    );

    let s = parse(&[]);
    let qp = |v: Json| s.sanitize_client_setting("video_min_qp", Some(&v), "t");
    assert_eq!(qp(json!(20)), Some(json!(20)));
    assert_eq!(qp(json!(99)), Some(json!(51)));
    assert_eq!(qp(json!(-5)), Some(json!(0)));
    let default = qp(json!("not a number")).expect("a defined setting");
    assert_eq!(
        crate::ws::state::json_i64(&default),
        Some(0),
        "text resolves to the default rather than raising"
    );
    assert_eq!(
        qp(json!(40)),
        Some(json!(40)),
        "the operator's own value does not bound the client's"
    );
    let interval = |v: Json| s.sanitize_client_setting("keyframe_interval", Some(&v), "t");
    assert_eq!(interval(json!(2.5)), Some(json!(2.5)));
    assert_eq!(interval(json!(9999.0)), Some(json!(300.0)));

    let unlocked = parse(&[("SELKIES_ENABLE_BINARY_CLIPBOARD", "true")]);
    let binary =
        |v: Json| unlocked.sanitize_client_setting("enable_binary_clipboard", Some(&v), "t");
    assert_eq!(binary(json!("false")), Some(json!(false)));
    assert_eq!(binary(json!("1")), Some(json!(true)));
    assert_eq!(binary(json!(true)), Some(json!(true)));
    assert_eq!(
        unlocked.sanitize_client_setting("enable_binary_clipboard", None, "t"),
        Some(json!(true))
    );
    let locked = parse(&[("SELKIES_ENABLE_BINARY_CLIPBOARD", "true|locked")]);
    assert_eq!(
        locked.sanitize_client_setting("enable_binary_clipboard", Some(&json!(false)), "t"),
        Some(json!(true)),
        "a locked bool keeps the server's value"
    );
    assert_eq!(
        s.sanitize_client_setting("no_such_setting", Some(&json!(1)), "t"),
        None
    );
}
