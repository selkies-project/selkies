/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Every setting the server takes, as one declarative table.
//!
//! The definition of record: a setting is added or changed here, its typed accessor on
//! `Settings` at the end beside it, and `scripts/generate-settings-doc.py` renders
//! `docs/settings.md` from this table. Each entry names a setting, its type, the default as the
//! text an operator would type, its bounds and menu, the environment variables it also reads,
//! and whether it is a secret or kept from the client payload.

use super::{BoolSetting, Kind, Num, SettingDef, Settings, Value};

/// The setting definitions, in the order `selkies --help` lists them.
pub static DEFINITIONS: &[SettingDef] = &[
    SettingDef {
        name: "addr",
        kind: Kind::Str,
        default: "localhost",
        help: "Address the streaming service listens on: a host name or IP address, or a comma-separated list of them, bound on every address each resolves to. The default binds the loopback addresses only (127.0.0.1,::1); to accept connections on every interface pass --public instead, never both.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "public",
        kind: Kind::Bool,
        default: "False",
        help: "Accept connections on every interface, IPv4 and IPv6 (0.0.0.0,::), instead of the loopback addresses only; the bare flag turns it on. Given together with --addr, the server refuses to start.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "port",
        kind: Kind::Int,
        default: "8080",
        help: "Port to start the streaming service, default: \"8080\"",
        env_vars: &["CUSTOM_WS_PORT"],
        allowed: &[],
        value_range: None,
        min: Some(1.0),
        max: Some(65535.0),
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "unix_socket",
        kind: Kind::Str,
        default: "",
        help: "Unix socket path to start the streaming service; when set, a Unix domain socket is bound instead of the TCP addr/port pair.",
        env_vars: &["SELKIES_UNIX_SOCKET"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "web_root",
        kind: Kind::Str,
        default: "",
        help: "Directory holding a web client, with its index.html, to serve in place of the one compiled into Selkies",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "audio_enabled",
        kind: Kind::Bool,
        default: "True",
        help: "Enable server-to-client audio streaming. Disabling this will also disable microphone support.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "microphone_enabled",
        kind: Kind::Bool,
        default: "False",
        help: "Enable client-to-server microphone forwarding.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "gamepad_enabled",
        kind: Kind::Bool,
        default: "True",
        help: "Enable gamepad support.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_enabled",
        kind: Kind::Bool,
        default: "False",
        help: "Enable client-to-server webcam forwarding to the virtual V4L2 device.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_on_start",
        kind: Kind::Bool,
        default: "True",
        help: "Start a session with the display's video stream on. Off, nothing is captured for the primary display until the client turns video on with the side menu toggle; a shared viewer and a second display page always start their stream.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_on_start",
        kind: Kind::Bool,
        default: "True",
        help: "Start a session with server-to-client audio on. Off, the audio capture stays stopped until the client turns audio on with the side menu toggle; on WebRTC the audio track is negotiated but carries nothing until then. Unlike audio_enabled=false, nothing is torn down and the microphone keeps working.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "microphone_on_start",
        kind: Kind::Str,
        default: "false",
        help: "Microphone uplink policy: \"true\" asks the browser for the device as soon as the session connects, \"false\" waits for the side menu toggle, and \"demand\" asks only while an application in the session records from the virtual source, released ten seconds after the last one stops (an application that records in the background and mutes in software keeps it asked for). A microphone_enabled locked off leaves it off either way.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_on_start",
        kind: Kind::Str,
        default: "false",
        help: "Webcam uplink policy: \"true\" asks the browser for the camera as soon as the session connects, \"false\" waits for the side menu toggle, and \"demand\" asks only while an application in the session holds the virtual device open, released two seconds after the last one closes it; the device then exists before any uplink, so a webcam_pixel_format of \"auto\" resolves to I420. A webcam_enabled locked off leaves it off either way.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "gamepad_on_start",
        kind: Kind::Bool,
        default: "True",
        help: "Start a session with gamepad input on. Off, connected gamepads are not polled until the client turns them on with the side menu toggle; a choice made there is remembered by the browser and takes precedence on its later visits.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_clipboard",
        kind: Kind::Str,
        default: "true",
        help: "Clipboard policy for both transports: \"true\" (both directions), \"in\" (client-to-server only), \"out\" (server-to-client only), \"false\" (disabled). \"out\" is what stops the page reading the local clipboard at all, which is the read Firefox and Safari raise their paste prompt for; the dashboard's clipboard box still sends.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "command_enabled",
        kind: Kind::Bool,
        default: "False",
        help: "Enable parsing of command websocket messages. Disabled by default for security; opt in with SELKIES_COMMAND_ENABLED=true (or --command-enabled true).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "apps_repo_folder",
        kind: Kind::Str,
        default: "",
        help: "Directory of a proot-apps local repository (the folder `proot-apps localrepo` fills), mounted into the session read-only. When set, the apps panel lists that folder's catalog (its metadata/metadata.yml and metadata/img, else the applications the folder holds) instead of the remote one, and the runner installs and updates from it; the server passes it to the runner as PA_REPO_FOLDER, the variable proot-apps itself reads, which is also the fallback for this setting. Empty uses the remote repository.",
        env_vars: &["PA_REPO_FOLDER"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "webrtc_pacer",
        kind: Kind::Bool,
        default: "True",
        help: "Pace outgoing WebRTC packets per transport with strict priorities (audio/RTCP > data-channel > video), an IDR-aware video queue budget, and GOP-reset recovery, so audio and interactive signaling are protected from video bursts on congested links. Enabled by default; set SELKIES_WEBRTC_PACER=false to disable. SELKIES_WEBRTC_PACER_STALE_MS sets the stale-GOP purge deadline in milliseconds (0 = disabled).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "file_transfers",
        kind: Kind::List,
        default: "upload,download",
        help: "Allowed file transfer directions (comma-separated: \"upload,download\"). Set to \"\" or \"none\" to disable.",
        env_vars: &[],
        allowed: &["upload", "download"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "printing_enabled",
        kind: Kind::Bool,
        default: "True",
        help: "Hand the documents printed in the session to the browser: a job printed to the session's Selkies queue lands in the print spool as a PDF, and the page opens it in the browser's print dialog. Off refuses the documents and shows no printer section.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "print_spool_path",
        kind: Kind::Str,
        default: "",
        help: "Directory the session's print queue writes finished jobs into as PDFs, watched for documents to hand to the browser and created at startup when missing; a document is removed once a page has taken it. Empty (default) uses selkies/print under $XDG_STATE_HOME, or under ~/.local/state when that is unset.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "file_transfer_limit_mbps",
        kind: Kind::Float,
        default: "0.0",
        help: "Static file-transfer throttle in Mbit/s, one allowance shared by all downloads and uploads, for links whose rate the operator knows. 0 disables. The congestion-control pacing protects the video stream without it, in both directions and end to end through a reverse proxy; the cap is for holding transfers to a fixed share regardless.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audit_webhook_url",
        kind: Kind::Str,
        default: "",
        help: "URL that receives one JSON POST per clipboard transfer, file upload, file download, printed document handed over, page connection, and recording, carrying metadata only (the event, an RFC 3339 timestamp, byte size, MIME type, or file name) and never the content. Events are delivered in order over one keep-alive connection; a collector that is slow or down loses what overflows the queue rather than stalling the session, and is sent an audit.dropped event with the count once the queue drains. Empty (default) sends nothing.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "audit_webhook_token",
        kind: Kind::Str,
        default: "",
        help: "Bearer token sent in the Authorization header of every audit POST. Empty sends no header.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "audit_webhook_timeout",
        kind: Kind::Float,
        default: "2.0",
        help: "Seconds one audit POST may take before it counts as failed and the next event is sent.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.1),
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "framerate",
        kind: Kind::Range,
        default: "8-240",
        help: "Framerate: allowed range (e.g., \"8-240\"), initial value (e.g., \"60\"), or both (\"60,8-240\"); \"60-60\" locks.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(60.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_crf",
        kind: Kind::Range,
        default: "5-50",
        help: "Video CRF (constant quality): allowed range (e.g., \"5-50\"), initial value (e.g., \"25\"), or both (\"25,5-50\"); \"25-25\" locks. With congestion_control on, the encoders that take a cap (software H.264, VP8, VP9, NVENC) code at the CRF under a peak of what the path carries, the CRF picking the picture wherever the path carries it.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(25.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_bitrate",
        kind: Kind::Range,
        default: "100-1000000",
        help: "Video bitrate aka CBR, in kilobits per second (kbps): allowed range (e.g., \"100-1000000\"), initial value (e.g., \"8000\" for 8 Mbps, \"250\" for 250 kbps), or both (\"8000,100-1000000\"); \"8000-8000\" locks. In CRF mode the chosen value goes unused but by the encoders without a CRF (V4L2 M2M, Tegra), and with congestion_control on the range bounds the peak a capped CRF is steered to: its high end is the most, its low end the least.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(8000.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "rate_control_mode",
        kind: Kind::Enum,
        default: "cbr",
        help: "Rate control mode for the video encoders on both transports (cbr = constant bitrate, crf = constant quality/QP). Honored by every video encoder when enable_rate_control is true (the default), except the two that run at the bitrate whatever the mode: a V4L2 memory-to-memory device at a variable one, Tegra's encoder at a constant one. With congestion_control on, a crf display is steered to what its path carries as a cbr one is, its encoder capped at that rate rather than held to it, where the encoder takes a cap (congestion_control names which).",
        env_vars: &[],
        allowed: &["cbr", "crf"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_rate_control",
        kind: Kind::Bool,
        default: "True",
        help: "Honor the client-selected rate_control_mode (crf/cbr). Enabled by default so both modes are selectable; set false to lock the encoder to its built-in default.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "keyframe_interval",
        kind: Kind::Float,
        default: "0.0",
        help: "Seconds between scheduled video recovery keyframes (any video codec). 0 (default) keeps the GOP infinite: keyframes are sent only on demand (client join/reset, keyframe requests), which keeps bitrate and quality steady.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(300.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_min_qp",
        kind: Kind::Int,
        default: "0",
        help: "CBR-mode minimum quantizer on the H.264 QP scale, mapped onto each codec's own quantizer range. 0 leaves the encoder its own: software H.264 (x264) then floors a moving screen at video_paintover_crf, so a generous bitrate is not spent coding motion finer than the still picture it leaves, and lifts the floor while a still screen is cleaned up; the other encoders have no floor. Raising it caps bit spend on easy content when the bitrate budget is generous, still screens included.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(51.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_max_qp",
        kind: Kind::Int,
        default: "0",
        help: "CBR-mode maximum quantizer on the H.264 QP scale, mapped onto each codec's own quantizer range (0 = encoder default). Lowering it keeps screen text legible under motion at the cost of overshooting the bitrate target on hard content (measured at 720p60 scrolling text: 35 lifts x264 by ~19 dB at ~2.5x the target).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(51.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_frame_duration_ms",
        kind: Kind::Enum,
        default: "10",
        help: "Opus frame duration in milliseconds for server-to-client audio. Lower values cut audio latency (each frame must fill before it can be sent, and the client buffers a fixed number of frames) at a small bitrate-efficiency and packet-rate cost. On WebRTC the SDP ptime/minptime follow this value.",
        env_vars: &[],
        allowed: &["2.5", "5", "10", "20", "40", "60"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_bitrate",
        kind: Kind::Enum,
        default: "128000",
        help: "The default audio bitrate.",
        env_vars: &[],
        allowed: &[
            "32000", "48000", "64000", "96000", "128000", "192000", "256000", "320000", "384000",
            "510000",
        ],
        value_range: Some((6000.0, 510000.0)),
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_redundancy",
        kind: Kind::Bool,
        default: "True",
        help: "Enable Opus RED (RFC 2198) audio redundancy to cut dropouts/concealment under packet loss. On by default; carries prior frames as redundancy on WebRTC (browsers de-RED natively, plain-opus fallback for peers that decline) and, on WebSocket, is gated on every client supporting it.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_redundancy_distance",
        kind: Kind::Int,
        default: "2",
        help: "Number of prior Opus frames carried as RED redundancy when audio_redundancy is enabled (0-4; higher survives longer loss bursts at proportionally more bandwidth).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(4.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "manual_resolution",
        kind: Kind::Bool,
        default: "False",
        help: "Lock the resolution to the manual width/height values.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "manual_width",
        kind: Kind::Int,
        default: "0",
        help: "Lock width to a fixed value. Setting this forces manual resolution mode.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(16384.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "manual_height",
        kind: Kind::Int,
        default: "0",
        help: "Lock height to a fixed value. Setting this forces manual resolution mode.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(16384.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "scaling_dpi",
        kind: Kind::Enum,
        default: "96",
        help: "The default DPI for UI scaling.",
        env_vars: &[],
        allowed: &["96", "120", "144", "168", "192", "216", "240", "264", "288"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "force_aligned_resolution",
        kind: Kind::Bool,
        default: "False",
        help: "Forces the display resolution to be a multiple of 16 pixels.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_binary_clipboard",
        kind: Kind::Bool,
        default: "True",
        help: "Allow binary data (e.g., images) on the clipboard.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "clipboard_seamless",
        kind: Kind::Bool,
        default: "True",
        help: "Synchronize the clipboard as the session and the browser change it, so a copy on either side is ready to paste on the other without asking. Turn it off to move clipboard content only when the user asks for it, through the side menu's clipboard box, which suits a session whose content should not follow the user out of it by accident. The direction settings still bound what moves either way. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "keyboard_shortcuts",
        kind: Kind::Bool,
        default: "True",
        help: "Let the client keep its own chords (Control+Shift with F, M, X, or G, and Control+Shift+click) instead of passing them to the session. Turn it off where an application in the session binds the same chords; the side menu's buttons still reach every function, and pressing Escape three times still leaves gaming mode. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "use_browser_cursors",
        kind: Kind::Bool,
        default: "True",
        help: "Use browser CSS cursors instead of rendering to canvas.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "raw_pointer_motion",
        kind: Kind::Bool,
        default: "True",
        help: "Ask the browser for unaccelerated (raw) pointer movement under pointer lock, so a locked pointer travels by the mouse's own counts rather than the local acceleration curve; Windows and macOS grant it, Linux and Android refuse it and keep the curve either way. Clients leave it off on macOS unless it is chosen or set here, since that curve is what carries a slow hand across the remote screen. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "mac_cmd_as_ctrl",
        kind: Kind::Bool,
        default: "True",
        help: "Send a macOS client's Command chords as their Control chords, so Cmd+C copies in the remote application the way it does locally. Turn it off where the session's window manager takes Super as its own modifier: remapped, a Cmd+Return bound to open a terminal arrives as Ctrl+Return, and Cmd+C interrupts the foreground program instead of copying. Command then arrives as the Super it physically is. Only macOS clients read it. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "use_css_scaling",
        kind: Kind::Bool,
        default: "False",
        help: "HiDPI when false, if true a lower resolution is sent from the client and the canvas is stretched; the UI scaling pick then divides that resolution and the desktop stays at 96 DPI.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_title",
        kind: Kind::Str,
        default: "Selkies",
        help: "Title in the dashboard's header, and of the browser tab when set.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_show_logo",
        kind: Kind::Bool,
        default: "True",
        help: "Show the Selkies logo in the dashboard's header.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_show_core_buttons",
        kind: Kind::Bool,
        default: "True",
        help: "Show the core components buttons display, audio, microphone, webcam, and gamepad.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_show_sidebar",
        kind: Kind::Bool,
        default: "True",
        help: "Show the dashboard's chrome: the layout's menu with every panel in it. Off, the page streams with no controls at all.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_layout",
        kind: Kind::Enum,
        default: "default",
        help: "How the dashboard arranges its panels: \"default\" is the sidebar, a drawer on the left edge with every panel as a section; \"wish\" is the toolbar, a floating bar along the top edge with the panels in its menus; \"edge\" is a developer's button, a round button docked on an edge of the window that sinks half into it at rest and fans out the toolbar's tools around it when tapped, which the user can drag to any edge or corner, with the stream's live figures in view over the stream. The first value listed is what a page shows until its user picks another with the layout toggle, which the page keeps per browser; a single value locks the layout and hides the toggle.",
        env_vars: &[],
        allowed: &["default", "wish", "edge"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_show_connection_indicator",
        kind: Kind::Bool,
        default: "True",
        help: "Show the mark over the stream while the server finds the client's connection poor, with or without the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_video_settings",
        kind: Kind::Bool,
        default: "True",
        help: "Show the video settings section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_screen_settings",
        kind: Kind::Bool,
        default: "True",
        help: "Show the screen settings section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_audio_settings",
        kind: Kind::Bool,
        default: "True",
        help: "Show the audio settings section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_stats",
        kind: Kind::Bool,
        default: "True",
        help: "Show the stats section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_shortcuts",
        kind: Kind::Bool,
        default: "True",
        help: "Show the keyboard shortcuts panel.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_clipboard",
        kind: Kind::Bool,
        default: "True",
        help: "Show the clipboard section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_files",
        kind: Kind::Bool,
        default: "True",
        help: "Show the file transfer section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_apps",
        kind: Kind::Bool,
        default: "True",
        help: "Show the applications section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_sharing",
        kind: Kind::Bool,
        default: "True",
        help: "Show the sharing section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_gamepads",
        kind: Kind::Bool,
        default: "True",
        help: "Show the gamepads section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_webcam",
        kind: Kind::Bool,
        default: "True",
        help: "Show the webcam toggle among the stream controls. Hides the control only; webcam_enabled governs whether the server accepts webcam frames.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_fullscreen",
        kind: Kind::Bool,
        default: "True",
        help: "Show the fullscreen button in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_gaming_mode",
        kind: Kind::Bool,
        default: "True",
        help: "Show the gaming mode button in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_trackpad",
        kind: Kind::Bool,
        default: "True",
        help: "Show the virtual trackpad button in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_keyboard_button",
        kind: Kind::Bool,
        default: "True",
        help: "Show the on-screen keyboard button in the display area.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "ui_sidebar_show_soft_buttons",
        kind: Kind::Bool,
        default: "True",
        help: "Show the soft buttons section in the sidebar.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_sharing",
        kind: Kind::Bool,
        default: "True",
        help: "Master toggle for all sharing features.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_collab",
        kind: Kind::Bool,
        default: "True",
        help: "Let a viewer holding the session's master-key token act as a read-write collaborator (secure mode only; not a sharing link of its own).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_shared",
        kind: Kind::Bool,
        default: "True",
        help: "Enable view-only sharing links.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_player2",
        kind: Kind::Bool,
        default: "True",
        help: "Enable sharing link for gamepad player 2.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_player3",
        kind: Kind::Bool,
        default: "True",
        help: "Enable sharing link for gamepad player 3.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_player4",
        kind: Kind::Bool,
        default: "True",
        help: "Enable sharing link for gamepad player 4.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "debug",
        kind: Kind::Bool,
        default: "False",
        help: "Enable debug logging.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "mode",
        kind: Kind::Str,
        default: "websockets",
        help: "Specify the mode: 'webrtc' or 'websockets'; defaults to websockets",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_dual_mode",
        kind: Kind::Bool,
        default: "True",
        help: "Enable switching Streaming modes from UI",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_device_name",
        kind: Kind::Str,
        default: "output.monitor",
        help: "Audio device name for pcmflux capture.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "master_token",
        kind: Kind::Str,
        default: "",
        help: "Master token to enable secure mode and protect the control plane API.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "tokens_file",
        kind: Kind::Str,
        default: "",
        help: "JSON file holding the session token table in the body format of POST /api/tokens, provisioned at start and again whenever it changes, in secure mode. Root or the server's user must own it, and no one else may write to it.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "enable_https",
        kind: Kind::Bool,
        default: "False",
        help: "Enable or disable HTTPS for the web application, specifying a valid server certificate is recommended",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "https_cert",
        kind: Kind::Str,
        default: "/etc/ssl/certs/ssl-cert-snakeoil.pem",
        help: "Path to the TLS server certificate file when HTTPS is enabled",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "https_key",
        kind: Kind::Str,
        default: "/etc/ssl/private/ssl-cert-snakeoil.key",
        help: "Path to the TLS server private key file when HTTPS is enabled, set to an empty value if the private key is included in the certificate",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "cert_reload_interval",
        kind: Kind::Int,
        default: "30",
        help: "Seconds between checks for SSL certificate file changes when HTTPS is enabled, set to 0 to disable automatic certificate reloading",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_basic_auth",
        kind: Kind::Bool,
        default: "True",
        help: "Enable basic authentication on the server. On by default, and the server refuses to start until a password is set through --basic-auth-password, SELKIES_BASIC_AUTH_PASSWORD, PASSWORD, or PASSWD; pass --enable-basic-auth=false to serve without a login instead.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "basic_auth_user",
        kind: Kind::Str,
        default: "ubuntu",
        help: "Username for basic authentication; resolves from the CUSTOM_USER, then USERNAME, then USER environment variables, and defaults to \"ubuntu\" when none is set.",
        env_vars: &["CUSTOM_USER", "USERNAME", "USER"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "basic_auth_password",
        kind: Kind::Str,
        default: "",
        help: "Password used when basic authentication is set; resolves from SELKIES_BASIC_AUTH_PASSWORD, then PASSWORD, then PASSWD, so an image that already names a container account password does not have to repeat it. There is no default: the server will not start with basic authentication enabled until one of these is set.",
        env_vars: &["PASSWORD", "PASSWD"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "basic_auth_viewonly_password",
        kind: Kind::Str,
        default: "",
        help: "Optional second basic-auth password that grants view-only access. Clients authenticating with it are capped at the viewer role (no keyboard, mouse, clipboard, gamepad, or command input, and no file transfers) regardless of the role they request, while the main password authorizes full control. Empty disables the split. Ignored in secure mode, where the master token governs roles.",
        env_vars: &["VIEWONLY_PASSWORD"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "subfolder",
        kind: Kind::Str,
        default: "",
        help: "URL path prefix the server is reverse-proxied under; prepended to every route (websockets, tokens, metrics, static files). Slashes are optional, so \"desk\", \"/desk\", and \"/desk/\" are the same prefix and \"/\" is the root. The web client reads its own prefix from the URL it was loaded from, so only the server needs telling.",
        env_vars: &["SUBFOLDER"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "run_after_connect",
        kind: Kind::Str,
        default: "",
        help: "Shell command run after the first client has connected ('' = off); runs again each time a client connects while no others are connected.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "run_after_disconnect",
        kind: Kind::Str,
        default: "",
        help: "Shell command run after the last client has disconnected ('' = off), including on server shutdown while clients are connected.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "encoder",
        kind: Kind::Enum,
        default: "h264enc",
        help: "The default video encoder. Every full-frame encoder runs on NVENC or VA-API where the GPU carries the codec and falls back to the software encoder pixelflux was built with: h264enc (x264, or OpenH264 in a GPL-free build), h265enc (x265, or kvazaar in a GPL-free build), vp8enc and vp9enc (libvpx), av1enc (SVT-AV1). jpeg is CPU-striped JPEG. Clients are offered only the encoders this host serves: those whose codec the encode node's GPU encodes (probed once at startup) or the pixelflux build carries a software encoder for. A client whose browser cannot decode the codec steps through the allowed encoders it does decode, taking the codecs this host encodes in hardware, most efficient first, before those it encodes in software, in order of encode time, and jpeg last of all. The WebRTC transport carries the full-frame encoders and offers them in the same order behind the display's codec, so a browser that declines the codec answers with the next one it decodes and the display moves to it; jpeg is WebSocket-only.",
        env_vars: &[],
        allowed: &["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "jpeg"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "jpeg_quality",
        kind: Kind::Range,
        default: "1-100",
        help: "JPEG quality: allowed range (e.g., \"1-100\"), initial value (e.g., \"40\"), or both (\"40,1-100\"); \"40-40\" locks.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(40.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_fullcolor",
        kind: Kind::Bool,
        default: "False",
        help: "Encode with 4:4:4 chroma rather than 4:2:0 where the codec and encoder carry it (H.264 and H.265 on NVENC, VA-API, x264, and x265; VP9 profile 1 on VA-API and libvpx); other codecs and encoders stay 4:2:0. The server knows which of its encoders carry it on this host, so a client whose decoder has no 4:4:4 profile turns it off for itself only where the stream would carry it, whether it or this default asked for it, and streams 4:2:0 on the same codec; where it is locked on, such a client steps over WebSockets to the next allowed encoder whose 4:4:4 it decodes or that has none, and to JPEG last, and reports the stream over WebRTC. A WebRTC client names the 4:4:4 it decodes in its hello, so its first offer already fits it.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_10bit",
        kind: Kind::Bool,
        default: "False",
        help: "Encode 10 bits per sample rather than 8 where the codec has a 10-bit profile an encoder of this host codes: H.264 on x264, and H.265 (Main 10, Main 4:4:4 10), VP9 (profiles 2 and 3), and AV1 on VA-API, x265, libvpx, and SVT-AV1, and H.265 and AV1 on NVENC. A GPU that does not encode the format hands the session to the software encoder, as it does a 4:4:4 it lacks, which the stream statistics show (H.264 on NVENC, which codes it at 8 bits); OpenH264, kvazaar, VP8, and JPEG stay 8-bit. A software encoder pays for it in CPU, converting every frame to 10-bit samples and coding at high bit depth, and the source is an 8-bit desktop, so it is recommended only together with 4:4:4 full color, where the added precision shows. The server knows which of its encoders carry it, and a client offers it only where its own decoder shows a 10-bit picture of that codec, turning it off for itself where it does not, whether it or this default asked for it; where it is locked on, such a client steps over WebSockets to the next allowed encoder it decodes, and reports the stream over WebRTC. A WebRTC client names the 10-bit formats it decodes in its hello, so its first offer already fits it; Chromium-based browsers decode AV1 over WebRTC at 8 bits only.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_streaming_mode",
        kind: Kind::Bool,
        default: "False",
        help: "Enable video streaming mode (Turbo: encode every frame like a traditional video encoder) for pixelflux encoders.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "use_cpu",
        kind: Kind::Bool,
        default: "False",
        help: "Force CPU-based encoding for pixelflux.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "cpu_encode_threading",
        kind: Kind::Bool,
        default: "True",
        help: "Spread each frame a software encoder codes across threads: x264 and OpenH264 code a full frame in slices on several threads, libvpx, x265, kvazaar, and SVT-AV1 run their worker threads, and jpeg codes its stripes in parallel. Turn it off on low-end hosts that pack many sessions onto their cores, so each session's encode keeps to one thread, the stripes one after another; a frame then takes longer to code, which a high frame rate notices. Unless set, a vp8enc session runs on one thread and every other encoder threaded: libvpx's VP8 row threads cost screen content up to four times the bits at the same quality, and 6 dB under a constant rate, while spinning on the rows they wait for. Hardware encoders have no such threads, so the dashboards show it only while the session encodes in software. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "use_paint_over_quality",
        kind: Kind::Bool,
        default: "True",
        help: "Clean up a still screen at the paint-over quality. Unless set, it is off while video_streaming_mode (Turbo) drives a video encoder and on otherwise, JPEG included, as the dashboards default it. Under CBR a video stream's still screen is cleaned up to the paint-over quality whatever this says, since a rate control alone leaves it short of clean where the bitrate is low for the frame rate, so this changes CRF sessions and JPEG only and the dashboards hide it under CBR; under CRF and for JPEG a still screen is cleaned up only with it on. Under Turbo the frames are hashed for the cleanup, on the GPU where NVENC encodes and on the CPU beside the encode otherwise. When on: once the picture stops changing, or keeps changing only in small places, a video encoder refreshes what changed at the paint-over CRF, and after a large change sends a key frame at it once the screen holds still. Under CBR the frames instead keep coming after the screen stops, each coded by the rate control, until the picture is clean, and then stop until it moves again, so a blinking cursor on a clean screen costs only its own frames: NVENC and x264 until their rate control reaches that quality (NVENC refreshes the screen a band a frame where the bitrate is too low for it to get there), VA-API until the encoded picture stops improving, x265 and VP9 until theirs codes at it, for half a minute at most; VP8 and SVT-AV1 hold a refresh at it, a key frame within a second of the bitrate; JPEG re-sends still stripes at the paint-over JPEG quality.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "paint_over_jpeg_quality",
        kind: Kind::Range,
        default: "1-100",
        help: "JPEG paint-over quality: allowed range, initial value, or both (\"90,1-100\"); \"90-90\" locks.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(90.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_paintover_crf",
        kind: Kind::Range,
        default: "5-50",
        help: "Paint-over quality index, on the video_crf scale and mapped onto each video codec's quantizer, that a still screen is cleaned up at (under CBR the finest a still screen is refined to, reached as fast as the path carries, whatever use_paint_over_quality says): allowed range, initial value, or both (\"18,5-50\"); \"18-18\" locks.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(18.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "video_paintover_burst_frames",
        kind: Kind::Range,
        default: "1-30",
        help: "Frames a video encoder keeps sending after a cleanup or a key frame on a still screen, so rate control settles (under CBR every encoder but VP8 and SVT-AV1 keeps sending until the screen is clean instead): allowed range, initial value, or both (\"5,1-30\"); \"5-5\" locks.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: Some(5.0),
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "second_screen",
        kind: Kind::Bool,
        default: "True",
        help: "Enable support for a second monitor/display.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "encode_dri",
        kind: Kind::Str,
        default: "",
        help: "Path to the DRI render node the ENCODER uses (VA-API/NVENC device selection).",
        env_vars: &["DRI_NODE"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "render_dri",
        kind: Kind::Str,
        default: "",
        help: "Path to the DRI render node the Wayland compositor RENDERS on (defaults to auto_gpu selection, else software rendering).",
        env_vars: &["DRINODE"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "auto_gpu",
        kind: Kind::Str,
        default: "true",
        help: "GPU auto-selection for rendering, enabled by default: \"true\" picks the first GPU; \"false\" disables it; otherwise a case-insensitive token picks the first GPU it matches — a vendor name (nvidia, amd/ati, intel, arm/mali, qualcomm/adreno, broadcom/videocore, apple, imagination/powervr, vmware, virtio, ...), a kernel driver name (amdgpu, i915, xe, nouveau, panfrost, msm, v3d, ...), a devicetree vendor prefix (qcom, rockchip, brcm, ...), or a raw PCI vendor ID (0x10de).",
        env_vars: &["AUTO_GPU"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "wayland",
        kind: Kind::Bool,
        default: "False",
        help: "Run the Wayland (headless compositor) backend instead of X11 capture/input (default: X11); PIXELFLUX_WAYLAND remains as a legacy fallback.",
        env_vars: &["PIXELFLUX_WAYLAND"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "wm",
        kind: Kind::Bool,
        default: "False",
        help: "Manage the session's windows in the selkies process, on whichever backend runs: on Wayland the capture compositor places, frames, focuses, and stacks its clients itself and runs Xwayland for the X11 ones, so no labwc nests inside it; on X11 selkies claims the screen's window manager selection, so no openbox runs. The openbox-format rc.xml and menu.xml the images configure are read as they stand.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "desktop",
        kind: Kind::Bool,
        default: "False",
        help: "Start the session on the selkies-desktop shell (the panel, start menu, taskbar, wallpaper, and file manager) over the Selkies window manager, started and kept running by the server, rather than on the application alone; implies --wm. A user's switch from the window manager's menus is kept for the next session and wins over this unless it is locked.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "session_switch",
        kind: Kind::Bool,
        default: "True",
        help: "Offer the switch between the desktop and the application alone in the window manager's root menu and window menu, where the shell is installed and --desktop is not locked: the shell starts or stops, the application keeps running, and the choice is kept for the next session.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "app_restart",
        kind: Kind::Bool,
        default: "False",
        help: "Run the window manager's autostart file, the session's application, again once everything it started has exited, while the session is the application alone.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "wm_maximize",
        kind: Kind::Bool,
        default: "True",
        help: "Maximize the application's top-level windows while the session is the application alone, as they open and as the session switches to it; an rc.xml application rule that sets maximized wins.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "wm_decorations",
        kind: Kind::Bool,
        default: "True",
        help: "Frame windows with the window manager's title bar and border; false frames none, as openbox's decor rule set to no does for every window. Windows that draw their own title bar keep it.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "wm_buttons",
        kind: Kind::Str,
        default: "minimize,maximize,close",
        help: "The buttons the window manager's title bars show, a comma-separated list of minimize, maximize, and close, or none for no buttons.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "wm_menus",
        kind: Kind::Bool,
        default: "True",
        help: "Let the mouse open the window manager's menus: the root menu, the window list, and the window menu. False takes those bindings away and stops the window manager from reading the user's rc.xml, menu.xml, and key bind overrides, only the system's.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "wm_keybinds",
        kind: Kind::Bool,
        default: "True",
        help: "Keep the key binds that close or kill a window, open a menu, or start a program. False takes them away and stops the window manager from reading the user's rc.xml, menu.xml, and key bind overrides, only the system's.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "app_wayland_display",
        kind: Kind::Str,
        default: "",
        help: "Wayland socket where applications run — the target for input injection and clipboard — when it differs from the pixelflux capture compositor (e.g. a nested session that pixelflux captures). Empty auto-detects the nested compositor socket in XDG_RUNTIME_DIR, falling back to the capture compositor when nothing is nested.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "computer_use_bind",
        kind: Kind::Str,
        default: "",
        help: "Start pixelflux's Computer-Use HTTP server on comma-separated entries: a bare port listens on the loopback addresses only, which every account on the host can reach, host:port names the address to listen on (0.0.0.0:9500,[::]:9500 accepts connections on every interface). A caller drives the desktop with this user's full authority, so the server needs --computer-use-token and does not start without it. Empty leaves it off; the PIXELFLUX_CU environment variable remains the standalone fallback.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "computer_use_token",
        kind: Kind::Str,
        default: "",
        help: "Bearer token every Computer-Use request has to carry (Authorization: Bearer <token>); falls back to the PIXELFLUX_CU_TOKEN environment variable.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: true,
    },
    SettingDef {
        name: "wayland_host_display",
        kind: Kind::Str,
        default: "",
        help: "Wayland socket of an EXTERNAL compositor (e.g. labwc started with WLR_BACKENDS=headless, or the GNOME/KDE session's own socket) that pixelflux captures and injects into as a client, instead of compositing its own. Frames arrive by screencopy into pixelflux-allocated GPU buffers (zero-copy into the encoder) and input rides virtual-keyboard/pointer devices where the compositor offers those protocols; a compositor without them (GNOME, KDE) is captured and driven through xdg-desktop-portal over the session bus, PipeWire frames imported in place. Empty keeps the built-in compositor.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "recording_socket",
        kind: Kind::Str,
        default: "",
        help: "Unix socket path for the out-of-band H.264 recording tap ('' = off); pixelflux binds it and multiplexes the elementary stream to connected clients.",
        env_vars: &["PIXELFLUX_RECORDING_SOCKET"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "file_manager_path",
        kind: Kind::Str,
        default: "~/Desktop",
        help: "Directory for client file transfers on both transports: uploads land here and the file-browser/download API serves it (created at startup if missing).",
        env_vars: &["FILE_MANAGER_PATH"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "watermark_path",
        kind: Kind::Str,
        default: "",
        help: "Absolute path to the watermark PNG file.",
        env_vars: &["WATERMARK_PNG"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "watermark_location",
        kind: Kind::Int,
        default: "-1",
        help: "Watermark location enum (0-6).",
        env_vars: &["WATERMARK_LOCATION"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "wayland_socket_index",
        kind: Kind::Int,
        default: "0",
        help: "Index for the Wayland command socket (e.g. 0 for wayland-0).",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "rtc_config_json",
        kind: Kind::Str,
        default: "/tmp/rtc.json",
        help: "JSON file with WebRTC configuration to use, checked periodically, overriding all other STUN/TURN settings",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "turn_rest_uri",
        kind: Kind::Str,
        default: "",
        help: "URI for TURN REST API service, example: http://localhost:8008",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_rest_api_key",
        kind: Kind::Str,
        default: "",
        help: "API key to pass to the TURN REST API service",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_rest_username",
        kind: Kind::Str,
        default: "",
        help: "Username sent to the TURN REST API service (x-auth-user header); the service embeds it in the HMAC credential. Empty (default) uses the generic 'selkies'.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_rest_username_auth_header",
        kind: Kind::Str,
        default: "x-auth-user",
        help: "Header to pass user to TURN REST API service",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_rest_protocol_header",
        kind: Kind::Str,
        default: "x-turn-protocol",
        help: "Header to pass desired TURN protocol to TURN REST API service",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_rest_tls_header",
        kind: Kind::Str,
        default: "x-turn-tls",
        help: "Header to pass TURN (D)TLS usage to TURN REST API service",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_host",
        kind: Kind::Str,
        default: "staticauth.openrelay.metered.ca",
        help: "TURN host when generating RTC config from shared secret or using long-term credentials, IPv6 addresses must be enclosed with square brackets such as [::1]",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_port",
        kind: Kind::Int,
        default: "443",
        help: "TURN port when generating RTC config from shared secret or using long-term credentials",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(1.0),
        max: Some(65535.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_protocol",
        kind: Kind::Str,
        default: "udp",
        help: "TURN protocol for the client to use (\"udp\" or \"tcp\"), set to \"tcp\" without the quotes if \"udp\" is blocked on the network, \"udp\" is otherwise strongly recommended",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_tls",
        kind: Kind::Bool,
        default: "False",
        help: "Enable or disable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_shared_secret",
        kind: Kind::Str,
        default: "openrelayprojectsecret",
        help: "Shared TURN secret used to generate HMAC credentials, also requires --turn-host and --turn-port",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_username",
        kind: Kind::Str,
        default: "",
        help: "Legacy non-HMAC TURN credential username, also requires --turn-host and --turn-port",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "turn_password",
        kind: Kind::Str,
        default: "",
        help: "Legacy non-HMAC TURN credential password, also requires --turn-host and --turn-port",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "stun_host",
        kind: Kind::Str,
        default: "stun.l.google.com",
        help: "STUN host for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to \"stun.l.google.com\"",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "stun_port",
        kind: Kind::Int,
        default: "19302",
        help: "STUN port for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to \"19302\"",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(1.0),
        max: Some(65535.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_public_ip",
        kind: Kind::Str,
        default: "",
        help: "Public IP address(es) to advertise in WebRTC host ICE candidates (Pion-style NAT1TO1), for a host behind static 1:1 NAT such as a cloud instance whose private address maps to a fixed public/elastic IP with the WebRTC UDP ports forwarded. Accepts one or more addresses (comma- or space-separated): the first of each family replaces the private address of host candidates in that family, while server-reflexive (STUN) and relay (TURN) candidates are left untouched so hole-punching and TURN fallback still work. A host published at more than one public address lists each: a page whose signaling request named another listed address in its Host header is told that one instead, so its media enters where the page did; every entry must forward the same ICE ports. Empty (default) keeps the gathered addresses unchanged.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_port_range",
        kind: Kind::Str,
        default: "",
        help: "Inclusive UDP port range \"min-max\" (e.g. \"50000-50100\") that the local sockets behind direct WebRTC host ICE candidates bind into, for scheduler-managed deployments that allot each session a small firewalled window. Both bounds must lie within 1024-65535; a malformed or out-of-range value is rejected rather than clamped. Distinct from TURN_MIN_PORT/TURN_MAX_PORT, which confine the TURN relay allocation on the TURN server. Empty (default) keeps ephemeral OS-assigned ports.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_udp_mux_port",
        kind: Kind::Int,
        default: "0",
        help: "Single UDP port every WebRTC session shares for its host ICE candidates (UDPMUX): bound once on each host address at startup, with sessions told apart by their ICE username fragment, so one forwarded port (e.g. \"-p 59000:59000/udp\") serves any number of sessions. STUN server-reflexive discovery rides the same port; TURN relay and mDNS sockets stay separate, and webrtc_port_range is unused. 0 (default) gives each session sockets of its own.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(65535.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_tcp_mux_port",
        kind: Kind::Int,
        default: "0",
        help: "Single TCP port on which the server accepts ICE-TCP connections (TCPMUX), advertised as a passive TCP host candidate on each host address next to the UDP ones, so a client on a network that blocks UDP still connects; UDP is preferred whenever it works. It may equal the UDP mux port, and a port firewalls pass, such as 443, is the usual choice for a public deployment. 0 (default) offers no TCP candidates.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(0.0),
        max: Some(65535.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_ice_lite",
        kind: Kind::Bool,
        default: "False",
        help: "Run the server's ICE agent as ICE-lite: it offers host candidates only, takes the controlled role, and answers the client's connectivity checks instead of sending its own, which suits a server whose host candidates are reachable as advertised (a public address, a static 1:1 NAT with webrtc_public_ip, or forwarded mux ports). STUN and TURN are then unused by the server itself; clients still receive them for candidates of their own.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_cloudflare_turn",
        kind: Kind::Bool,
        default: "False",
        help: "Enable Cloudflare TURN service, requires SELKIES_CLOUDFLARE_TURN_TOKEN_ID, and SELKIES_CLOUDFLARE_TURN_API_TOKEN",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "cloudflare_turn_token_id",
        kind: Kind::Str,
        default: "",
        help: "The Cloudflare TURN App token ID.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "cloudflare_turn_api_token",
        kind: Kind::Str,
        default: "",
        help: "The Cloudflare TURN API token.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: true,
        client_excluded: false,
    },
    SettingDef {
        name: "app_wait_ready",
        kind: Kind::Bool,
        default: "False",
        help: "Waits for --app-ready-file to exist before starting stream if set to \"true\"",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "app_ready_file",
        kind: Kind::Str,
        default: "/tmp/selkies-appready",
        help: "File set by sidecar used to indicate that app is initialized and ready",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "uinput_mouse_socket",
        kind: Kind::Str,
        default: "",
        help: "Path to the uinput mouse socket, if not provided uinput is used directly",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "js_socket_path",
        kind: Kind::Str,
        default: "",
        help: "Directory for the Selkies Input Interposer sockets (selkies_js{0-3}.sock, selkies_event{1000-1003}.sock). Empty uses XDG_RUNTIME_DIR, the session's private directory, and /tmp where that is unset; the interposer looks in the same place.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "webcam_socket_path",
        kind: Kind::Str,
        default: "",
        help: "Directory for the Selkies V4L2 Interposer webcam socket (selkies_webcam0.sock). Empty uses XDG_RUNTIME_DIR, the session's private directory, and /tmp where that is unset; the interposer looks in the same place.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "webcam_width",
        kind: Kind::Int,
        default: "1280",
        help: "Width of the virtual webcam device; client camera frames are scaled and letterboxed to fit.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_height",
        kind: Kind::Int,
        default: "720",
        help: "Height of the virtual webcam device; client camera frames are scaled and letterboxed to fit.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_pixel_format",
        kind: Kind::Str,
        default: "auto",
        help: "Pixel format of the virtual webcam device. \"auto\" follows the uplink: a browser sending JPEG (no WebCodecs) gets an MJPEG device that carries its frames as received, any other uplink an I420 device, and a later uplink of the other kind re-creates the device for itself while no application is reading it. Or pin \"I420\" (planar 4:2:0, the browsers' preference), \"NV12\", \"YUYV\", or \"MJPEG\", which is then kept whatever arrives.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_encoder",
        kind: Kind::Enum,
        default: "auto",
        help: "Codec clients encode the webcam uplink with. Over WebSockets \"auto\" runs the measured ladder (H.264, else VP8, then VP9, AV1, and H.265 where the engine encodes them, JPEG when none keeps up) on engines that stream camera frames through MediaStreamTrackProcessor, and JPEG on the `<video>`-element path (Firefox): its software encoders can hold the camera rate while costing a full core, which no client-side probe can price. A codec name runs that one codec on every path, trading client CPU for a fraction of the uplink bandwidth, still falling to JPEG where it cannot keep up or encodes the wrong colors; \"mjpeg\" pins JPEG everywhere. Over WebRTC the browser sends its camera as the named codec when the answer negotiated it, and otherwise, as for \"auto\" and \"mjpeg\", as the first codec negotiated. Clients may override per user unless the value is locked.",
        env_vars: &[],
        allowed: &["auto", "h264", "h265", "vp8", "vp9", "av1", "mjpeg"],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webcam_device",
        kind: Kind::Str,
        default: "auto",
        help: "Also mirror the webcam into a v4l2loopback kernel device, which applications find without the V4L2 Interposer: \"auto\" uses the first v4l2loopback output device found (typically a desktop host or privileged container), a path such as \"/dev/video10\" uses that device, and \"false\" never does. The interposer socket is always served.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "uinput_gamepad",
        kind: Kind::Str,
        default: "auto",
        help: "Register gamepads as kernel devices through /dev/uinput, which applications (Steam, Proton, in-desktop browsers) find without the Input Interposer or fake-udev: \"auto\" does so only where the interposer is not configured for the session and /dev/uinput is writable — typically a desktop host rather than a container — while \"true\" always attempts it and \"false\" never does.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "publish_input_devices",
        kind: Kind::Bool,
        default: "False",
        help: "Mirror the session keyboard and pointer onto input devices applications can read, for the few that enumerate evdev instead of the display server (fullscreen games, remappers). The desktop is driven by the compositor's virtual keyboard or XTEST either way, so this adds a copy of each event and never carries it; it costs a kernel device where /dev/uinput is writable, else an Input Interposer one.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "gpu_id",
        kind: Kind::Str,
        default: "",
        help: "GPU ID for hardware video encoders: selects /dev/dri/renderD{128 + n} and the GPU-stats index. Empty (default) sets no explicit pick, encoding on ID 0 — the first GPU — or on the GPU chosen by --auto-gpu; -1 disables hardware encoding. Ignored when --encode-dri specifies a device path.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "congestion_control",
        kind: Kind::Bool,
        default: "True",
        help: "Adapt a display's video rate to what its path carries, a CBR display's below the bitrate the client chose: over WebRTC from the transport-wide-cc receiver feedback (loss, and one-way delay standing past the path's own), over WebSockets from the queue a display's frame round trip shows past the path's own and its jitter. A stall, the path holding the stream a second or more and then delivering it at once, cuts the rate only until round trips are back at the path's own, which return it to the rate before the stall. A still screen whose rate was cut is padded up to that rate, so the path is tested and the rate climbs back without the screen moving; a probe that meets a queue waits 15 s before the next, longer each time one does. A page keeps the rate its display held for 10 s, and a restarted server starts that page's stream there, for a day or until the user sets a bitrate. In CRF rate-control mode the same steer sets the encoder's peak, where CBR's is its target, under the video_bitrate range's high end, which the chosen bitrate does not move: the stream opens capped at the page's remembered rate, else 4000 kbps, and climbs as CBR does until the path queues, while the CRF picks the picture wherever the path carries it; a still CRF screen is never padded. Software H.264 (x264), VP8 and VP9 (libvpx), and NVENC take the peak and code as an uncapped CRF until the CRF's own output overflows it, then hold to it, x264 by raising its rate factor and libvpx and NVENC by switching to their constant rate at the peak, never finer than the CRF, until the content fits under it again, when the blocks they coded under it are coded again at the CRF, a share a frame within the peak; x265, SVT-AV1, VA-API, OpenH264, and kvazaar have no capped CRF, so their CRF displays run unsteered at their CRF, as do the V4L2 M2M and Tegra encoders, which have no CRF and run at the chosen bitrate. False holds the chosen bitrate whatever the path carries, so a path slower than it lags or drops frames, and leaves a CRF stream uncapped.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "audio_channels",
        kind: Kind::Int,
        default: "2",
        help: "Number of audio channels, defaults to stereo (2 channels)",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(1.0),
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_resize",
        kind: Kind::Bool,
        default: "True",
        help: "Enable dynamic resizing to match browser size",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_cursors",
        kind: Kind::Bool,
        default: "True",
        help: "Enable passing remote cursors to client",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "debug_cursors",
        kind: Kind::Bool,
        default: "False",
        help: "Enable cursor debug logging",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "cursor_size",
        kind: Kind::Int,
        default: "-1",
        help: "Cursor size in points at 96 DPI (scaled with the session DPI) for the X11 server cursor and the Wayland compositor cursor theme; -1 uses the platform default (32 on X11, 24 on Wayland). Independent of it, remote-cursor capture delivers an application's own cursor whole up to 128 points, the most a browser shows as a cursor, and a size set here beyond that raises the cap with it.",
        env_vars: &["XCURSOR_SIZE"],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "enable_webrtc_statistics",
        kind: Kind::Bool,
        default: "False",
        help: "Enable WebRTC Statistics CSV dumping to the directory --webrtc-statistics-dir with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "webrtc_statistics_dir",
        kind: Kind::Str,
        default: "/tmp",
        help: "Directory to save WebRTC Statistics CSV from client with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: true,
    },
    SettingDef {
        name: "enable_metrics_http",
        kind: Kind::Bool,
        default: "False",
        help: "Enable the Prometheus HTTP /metrics endpoint.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "backpressure_queue_size",
        kind: Kind::Int,
        default: "120",
        help: "Audio capture broadcast queue depth in packets, clamped to 1-65536 and rounded up to a power of two; lagging receivers lose overwritten packets. Applies to WebSockets and WebRTC audio, not video. Higher values tolerate longer consumer stalls at the cost of latency.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: Some(1.0),
        max: Some(100000.0),
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "allowed_origins",
        kind: Kind::Str,
        default: "",
        help: "Comma-separated browser Origins allowed to open the streaming WebSocket (cross-site WebSocket-hijacking guard). Empty (default) allows only same-origin plus non-browser clients that send no Origin; use '*' to allow any origin.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
    SettingDef {
        name: "frame_ancestors",
        kind: Kind::Str,
        default: "'self'",
        help: "Comma-separated pages allowed to show the client in a frame, sent as the Content-Security-Policy frame-ancestors directive: 'self' (the default, also what an empty value means) for this server's own origin, origins such as https://portal.example.com for a platform embedding the desktop from another, * for any page, or 'none' for no frame at all. A page the list does not name gets an empty frame, so it cannot lay the session under clicks meant for itself.",
        env_vars: &[],
        allowed: &[],
        value_range: None,
        min: None,
        max: None,
        default_value: None,
        sensitive: false,
        client_excluded: false,
    },
];

/// One typed reader per setting, named as the setting is; a bool carries its lock.
impl Settings {
    pub fn addr(&self) -> &str {
        self.str_value("addr")
    }

    pub fn public(&self) -> BoolSetting {
        self.bool_setting("public")
    }

    pub fn port(&self) -> i64 {
        self.int_value("port")
    }

    pub fn unix_socket(&self) -> &str {
        self.str_value("unix_socket")
    }

    pub fn web_root(&self) -> &str {
        self.str_value("web_root")
    }

    pub fn audio_enabled(&self) -> BoolSetting {
        self.bool_setting("audio_enabled")
    }

    pub fn microphone_enabled(&self) -> BoolSetting {
        self.bool_setting("microphone_enabled")
    }

    pub fn gamepad_enabled(&self) -> BoolSetting {
        self.bool_setting("gamepad_enabled")
    }

    pub fn webcam_enabled(&self) -> BoolSetting {
        self.bool_setting("webcam_enabled")
    }

    pub fn video_on_start(&self) -> BoolSetting {
        self.bool_setting("video_on_start")
    }

    pub fn audio_on_start(&self) -> BoolSetting {
        self.bool_setting("audio_on_start")
    }

    pub fn microphone_on_start(&self) -> &str {
        self.str_value("microphone_on_start")
    }

    pub fn webcam_on_start(&self) -> &str {
        self.str_value("webcam_on_start")
    }

    pub fn gamepad_on_start(&self) -> BoolSetting {
        self.bool_setting("gamepad_on_start")
    }

    pub fn enable_clipboard(&self) -> &str {
        self.str_value("enable_clipboard")
    }

    pub fn command_enabled(&self) -> BoolSetting {
        self.bool_setting("command_enabled")
    }

    pub fn apps_repo_folder(&self) -> &str {
        self.str_value("apps_repo_folder")
    }

    pub fn webrtc_pacer(&self) -> BoolSetting {
        self.bool_setting("webrtc_pacer")
    }

    pub fn file_transfers(&self) -> &[String] {
        self.list_value("file_transfers")
    }

    pub fn printing_enabled(&self) -> BoolSetting {
        self.bool_setting("printing_enabled")
    }

    pub fn print_spool_path(&self) -> &str {
        self.str_value("print_spool_path")
    }

    pub fn file_transfer_limit_mbps(&self) -> f64 {
        self.float_value("file_transfer_limit_mbps")
    }

    pub fn audit_webhook_url(&self) -> &str {
        self.str_value("audit_webhook_url")
    }

    pub fn audit_webhook_token(&self) -> &str {
        self.str_value("audit_webhook_token")
    }

    pub fn audit_webhook_timeout(&self) -> f64 {
        self.float_value("audit_webhook_timeout")
    }

    pub fn framerate(&self) -> (Num, Num) {
        self.range_value("framerate")
    }

    pub fn video_crf(&self) -> (Num, Num) {
        self.range_value("video_crf")
    }

    pub fn video_bitrate(&self) -> (Num, Num) {
        self.range_value("video_bitrate")
    }

    pub fn rate_control_mode(&self) -> &str {
        self.str_value("rate_control_mode")
    }

    pub fn enable_rate_control(&self) -> BoolSetting {
        self.bool_setting("enable_rate_control")
    }

    pub fn keyframe_interval(&self) -> f64 {
        self.float_value("keyframe_interval")
    }

    pub fn video_min_qp(&self) -> i64 {
        self.int_value("video_min_qp")
    }

    pub fn video_max_qp(&self) -> i64 {
        self.int_value("video_max_qp")
    }

    pub fn audio_frame_duration_ms(&self) -> &str {
        self.str_value("audio_frame_duration_ms")
    }

    pub fn audio_bitrate(&self) -> &str {
        self.str_value("audio_bitrate")
    }

    pub fn audio_redundancy(&self) -> BoolSetting {
        self.bool_setting("audio_redundancy")
    }

    pub fn audio_redundancy_distance(&self) -> i64 {
        self.int_value("audio_redundancy_distance")
    }

    pub fn manual_resolution(&self) -> BoolSetting {
        self.bool_setting("manual_resolution")
    }

    pub fn manual_width(&self) -> i64 {
        self.int_value("manual_width")
    }

    pub fn manual_height(&self) -> i64 {
        self.int_value("manual_height")
    }

    pub fn scaling_dpi(&self) -> &str {
        self.str_value("scaling_dpi")
    }

    pub fn force_aligned_resolution(&self) -> BoolSetting {
        self.bool_setting("force_aligned_resolution")
    }

    pub fn enable_binary_clipboard(&self) -> BoolSetting {
        self.bool_setting("enable_binary_clipboard")
    }

    pub fn clipboard_seamless(&self) -> BoolSetting {
        self.bool_setting("clipboard_seamless")
    }

    pub fn keyboard_shortcuts(&self) -> BoolSetting {
        self.bool_setting("keyboard_shortcuts")
    }

    pub fn use_browser_cursors(&self) -> BoolSetting {
        self.bool_setting("use_browser_cursors")
    }

    pub fn raw_pointer_motion(&self) -> BoolSetting {
        self.bool_setting("raw_pointer_motion")
    }

    pub fn mac_cmd_as_ctrl(&self) -> BoolSetting {
        self.bool_setting("mac_cmd_as_ctrl")
    }

    pub fn use_css_scaling(&self) -> BoolSetting {
        self.bool_setting("use_css_scaling")
    }

    pub fn ui_title(&self) -> &str {
        self.str_value("ui_title")
    }

    pub fn ui_show_logo(&self) -> BoolSetting {
        self.bool_setting("ui_show_logo")
    }

    pub fn ui_show_core_buttons(&self) -> BoolSetting {
        self.bool_setting("ui_show_core_buttons")
    }

    pub fn ui_show_sidebar(&self) -> BoolSetting {
        self.bool_setting("ui_show_sidebar")
    }

    pub fn ui_layout(&self) -> &str {
        self.str_value("ui_layout")
    }

    pub fn ui_show_connection_indicator(&self) -> BoolSetting {
        self.bool_setting("ui_show_connection_indicator")
    }

    pub fn ui_sidebar_show_video_settings(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_video_settings")
    }

    pub fn ui_sidebar_show_screen_settings(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_screen_settings")
    }

    pub fn ui_sidebar_show_audio_settings(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_audio_settings")
    }

    pub fn ui_sidebar_show_stats(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_stats")
    }

    pub fn ui_sidebar_show_shortcuts(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_shortcuts")
    }

    pub fn ui_sidebar_show_clipboard(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_clipboard")
    }

    pub fn ui_sidebar_show_files(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_files")
    }

    pub fn ui_sidebar_show_apps(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_apps")
    }

    pub fn ui_sidebar_show_sharing(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_sharing")
    }

    pub fn ui_sidebar_show_gamepads(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_gamepads")
    }

    pub fn ui_sidebar_show_webcam(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_webcam")
    }

    pub fn ui_sidebar_show_fullscreen(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_fullscreen")
    }

    pub fn ui_sidebar_show_gaming_mode(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_gaming_mode")
    }

    pub fn ui_sidebar_show_trackpad(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_trackpad")
    }

    pub fn ui_sidebar_show_keyboard_button(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_keyboard_button")
    }

    pub fn ui_sidebar_show_soft_buttons(&self) -> BoolSetting {
        self.bool_setting("ui_sidebar_show_soft_buttons")
    }

    pub fn enable_sharing(&self) -> BoolSetting {
        self.bool_setting("enable_sharing")
    }

    pub fn enable_collab(&self) -> BoolSetting {
        self.bool_setting("enable_collab")
    }

    pub fn enable_shared(&self) -> BoolSetting {
        self.bool_setting("enable_shared")
    }

    pub fn enable_player2(&self) -> BoolSetting {
        self.bool_setting("enable_player2")
    }

    pub fn enable_player3(&self) -> BoolSetting {
        self.bool_setting("enable_player3")
    }

    pub fn enable_player4(&self) -> BoolSetting {
        self.bool_setting("enable_player4")
    }

    pub fn debug(&self) -> BoolSetting {
        self.bool_setting("debug")
    }

    pub fn mode(&self) -> &str {
        self.str_value("mode")
    }

    pub fn enable_dual_mode(&self) -> BoolSetting {
        self.bool_setting("enable_dual_mode")
    }

    pub fn audio_device_name(&self) -> &str {
        self.str_value("audio_device_name")
    }

    pub fn master_token(&self) -> &str {
        self.str_value("master_token")
    }

    pub fn tokens_file(&self) -> &str {
        self.str_value("tokens_file")
    }

    pub fn enable_https(&self) -> BoolSetting {
        self.bool_setting("enable_https")
    }

    pub fn https_cert(&self) -> &str {
        self.str_value("https_cert")
    }

    pub fn https_key(&self) -> &str {
        self.str_value("https_key")
    }

    pub fn cert_reload_interval(&self) -> i64 {
        self.int_value("cert_reload_interval")
    }

    pub fn enable_basic_auth(&self) -> BoolSetting {
        self.bool_setting("enable_basic_auth")
    }

    pub fn basic_auth_user(&self) -> &str {
        self.str_value("basic_auth_user")
    }

    pub fn basic_auth_password(&self) -> &str {
        self.str_value("basic_auth_password")
    }

    pub fn basic_auth_viewonly_password(&self) -> &str {
        self.str_value("basic_auth_viewonly_password")
    }

    pub fn subfolder(&self) -> &str {
        self.str_value("subfolder")
    }

    pub fn run_after_connect(&self) -> &str {
        self.str_value("run_after_connect")
    }

    pub fn run_after_disconnect(&self) -> &str {
        self.str_value("run_after_disconnect")
    }

    pub fn encoder(&self) -> &str {
        self.str_value("encoder")
    }

    pub fn jpeg_quality(&self) -> (Num, Num) {
        self.range_value("jpeg_quality")
    }

    pub fn video_fullcolor(&self) -> BoolSetting {
        self.bool_setting("video_fullcolor")
    }

    pub fn video_10bit(&self) -> BoolSetting {
        self.bool_setting("video_10bit")
    }

    pub fn video_streaming_mode(&self) -> BoolSetting {
        self.bool_setting("video_streaming_mode")
    }

    pub fn use_cpu(&self) -> BoolSetting {
        self.bool_setting("use_cpu")
    }

    pub fn cpu_encode_threading(&self) -> BoolSetting {
        self.bool_setting("cpu_encode_threading")
    }

    pub fn use_paint_over_quality(&self) -> BoolSetting {
        self.bool_setting("use_paint_over_quality")
    }

    pub fn paint_over_jpeg_quality(&self) -> (Num, Num) {
        self.range_value("paint_over_jpeg_quality")
    }

    pub fn video_paintover_crf(&self) -> (Num, Num) {
        self.range_value("video_paintover_crf")
    }

    pub fn video_paintover_burst_frames(&self) -> (Num, Num) {
        self.range_value("video_paintover_burst_frames")
    }

    pub fn second_screen(&self) -> BoolSetting {
        self.bool_setting("second_screen")
    }

    pub fn encode_dri(&self) -> &str {
        self.str_value("encode_dri")
    }

    pub fn render_dri(&self) -> &str {
        self.str_value("render_dri")
    }

    pub fn auto_gpu(&self) -> &str {
        self.str_value("auto_gpu")
    }

    pub fn wayland(&self) -> BoolSetting {
        self.bool_setting("wayland")
    }

    pub fn wm(&self) -> BoolSetting {
        self.bool_setting("wm")
    }

    pub fn desktop(&self) -> BoolSetting {
        self.bool_setting("desktop")
    }

    pub fn session_switch(&self) -> BoolSetting {
        self.bool_setting("session_switch")
    }

    pub fn app_restart(&self) -> BoolSetting {
        self.bool_setting("app_restart")
    }

    pub fn wm_maximize(&self) -> BoolSetting {
        self.bool_setting("wm_maximize")
    }

    pub fn wm_decorations(&self) -> BoolSetting {
        self.bool_setting("wm_decorations")
    }

    pub fn wm_buttons(&self) -> &str {
        self.str_value("wm_buttons")
    }

    pub fn wm_menus(&self) -> BoolSetting {
        self.bool_setting("wm_menus")
    }

    pub fn wm_keybinds(&self) -> BoolSetting {
        self.bool_setting("wm_keybinds")
    }

    pub fn app_wayland_display(&self) -> &str {
        self.str_value("app_wayland_display")
    }

    pub fn computer_use_bind(&self) -> &str {
        self.str_value("computer_use_bind")
    }

    pub fn computer_use_token(&self) -> &str {
        self.str_value("computer_use_token")
    }

    pub fn wayland_host_display(&self) -> &str {
        self.str_value("wayland_host_display")
    }

    pub fn recording_socket(&self) -> &str {
        self.str_value("recording_socket")
    }

    pub fn file_manager_path(&self) -> &str {
        self.str_value("file_manager_path")
    }

    pub fn watermark_path(&self) -> &str {
        self.str_value("watermark_path")
    }

    pub fn watermark_location(&self) -> i64 {
        self.int_value("watermark_location")
    }

    pub fn wayland_socket_index(&self) -> i64 {
        self.int_value("wayland_socket_index")
    }

    pub fn rtc_config_json(&self) -> &str {
        self.str_value("rtc_config_json")
    }

    pub fn turn_rest_uri(&self) -> &str {
        self.str_value("turn_rest_uri")
    }

    pub fn turn_rest_api_key(&self) -> &str {
        self.str_value("turn_rest_api_key")
    }

    pub fn turn_rest_username(&self) -> &str {
        self.str_value("turn_rest_username")
    }

    pub fn turn_rest_username_auth_header(&self) -> &str {
        self.str_value("turn_rest_username_auth_header")
    }

    pub fn turn_rest_protocol_header(&self) -> &str {
        self.str_value("turn_rest_protocol_header")
    }

    pub fn turn_rest_tls_header(&self) -> &str {
        self.str_value("turn_rest_tls_header")
    }

    pub fn turn_host(&self) -> &str {
        self.str_value("turn_host")
    }

    pub fn turn_port(&self) -> i64 {
        self.int_value("turn_port")
    }

    pub fn turn_protocol(&self) -> &str {
        self.str_value("turn_protocol")
    }

    pub fn turn_tls(&self) -> BoolSetting {
        self.bool_setting("turn_tls")
    }

    pub fn turn_shared_secret(&self) -> &str {
        self.str_value("turn_shared_secret")
    }

    pub fn turn_username(&self) -> &str {
        self.str_value("turn_username")
    }

    /// The long-term TURN password, none where unset.
    pub fn turn_password(&self) -> Option<&str> {
        match self.value("turn_password") {
            Some(Value::Str(password)) if !password.is_empty() => Some(password),
            _ => None,
        }
    }

    pub fn stun_host(&self) -> &str {
        self.str_value("stun_host")
    }

    pub fn stun_port(&self) -> i64 {
        self.int_value("stun_port")
    }

    pub fn webrtc_public_ip(&self) -> &str {
        self.str_value("webrtc_public_ip")
    }

    pub fn webrtc_port_range(&self) -> &str {
        self.str_value("webrtc_port_range")
    }

    pub fn webrtc_udp_mux_port(&self) -> i64 {
        self.int_value("webrtc_udp_mux_port")
    }

    pub fn webrtc_tcp_mux_port(&self) -> i64 {
        self.int_value("webrtc_tcp_mux_port")
    }

    pub fn webrtc_ice_lite(&self) -> BoolSetting {
        self.bool_setting("webrtc_ice_lite")
    }

    pub fn enable_cloudflare_turn(&self) -> BoolSetting {
        self.bool_setting("enable_cloudflare_turn")
    }

    pub fn cloudflare_turn_token_id(&self) -> &str {
        self.str_value("cloudflare_turn_token_id")
    }

    pub fn cloudflare_turn_api_token(&self) -> &str {
        self.str_value("cloudflare_turn_api_token")
    }

    pub fn app_wait_ready(&self) -> BoolSetting {
        self.bool_setting("app_wait_ready")
    }

    pub fn app_ready_file(&self) -> &str {
        self.str_value("app_ready_file")
    }

    pub fn uinput_mouse_socket(&self) -> &str {
        self.str_value("uinput_mouse_socket")
    }

    pub fn js_socket_path(&self) -> &str {
        self.str_value("js_socket_path")
    }

    pub fn webcam_socket_path(&self) -> &str {
        self.str_value("webcam_socket_path")
    }

    pub fn webcam_width(&self) -> i64 {
        self.int_value("webcam_width")
    }

    pub fn webcam_height(&self) -> i64 {
        self.int_value("webcam_height")
    }

    pub fn webcam_pixel_format(&self) -> &str {
        self.str_value("webcam_pixel_format")
    }

    pub fn webcam_encoder(&self) -> &str {
        self.str_value("webcam_encoder")
    }

    pub fn webcam_device(&self) -> &str {
        self.str_value("webcam_device")
    }

    pub fn uinput_gamepad(&self) -> &str {
        self.str_value("uinput_gamepad")
    }

    pub fn publish_input_devices(&self) -> BoolSetting {
        self.bool_setting("publish_input_devices")
    }

    pub fn gpu_id(&self) -> &str {
        self.str_value("gpu_id")
    }

    pub fn congestion_control(&self) -> BoolSetting {
        self.bool_setting("congestion_control")
    }

    pub fn audio_channels(&self) -> i64 {
        self.int_value("audio_channels")
    }

    pub fn enable_resize(&self) -> BoolSetting {
        self.bool_setting("enable_resize")
    }

    pub fn enable_cursors(&self) -> BoolSetting {
        self.bool_setting("enable_cursors")
    }

    pub fn debug_cursors(&self) -> BoolSetting {
        self.bool_setting("debug_cursors")
    }

    pub fn cursor_size(&self) -> i64 {
        self.int_value("cursor_size")
    }

    pub fn enable_webrtc_statistics(&self) -> BoolSetting {
        self.bool_setting("enable_webrtc_statistics")
    }

    pub fn webrtc_statistics_dir(&self) -> &str {
        self.str_value("webrtc_statistics_dir")
    }

    pub fn enable_metrics_http(&self) -> BoolSetting {
        self.bool_setting("enable_metrics_http")
    }

    pub fn backpressure_queue_size(&self) -> i64 {
        self.int_value("backpressure_queue_size")
    }

    pub fn allowed_origins(&self) -> &str {
        self.str_value("allowed_origins")
    }

    pub fn frame_ancestors(&self) -> &str {
        self.str_value("frame_ancestors")
    }
}
