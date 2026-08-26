/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Settings panel of the wish dashboard: Video, Audio and Resolution tabs
 * over the streaming core.
 *
 * State arrives through the `message` events the core posts on `window`:
 * `serverSettings` (the server's settings payload, per key a `value`,
 * `allowed`, `min`/`max`, `default`, `locked` and `overridden`),
 * `effectiveCursorState` and `audioDeviceSelected`. Changes go back as
 * `window.postMessage` messages: `settings` (debounced key/value batches the
 * core forwards to the server), `mode`, `setScaleLocally`,
 * `setManualResolution`, `resetResolutionToWindow`, `setAntiAliasing`, and
 * whatever the shared conditional-setting specs post. The transport is seeded
 * from `window.__SELKIES_STREAMING_MODE__`, and `window.__selkiesModeSwitching`
 * is raised around a transport switch.
 *
 * Every value persists under a localStorage key from `getPrefixedKey`, which
 * adds the `_display2` suffix for per-display settings on a secondary display;
 * the cores read the same keys. The cores also persist every value they are
 * told to apply, so a stored key alone cannot tell a user's explicit pick from
 * one the dashboard derived (HiDPI from the resolution mode, rate control from
 * the encoder). Settings that are also derived therefore carry an
 * `_explicit_choice` marker beside their value and resolve through the shared
 * specs of `selkies-web-core/lib/conditional-settings.js`, which honor pinned,
 * locked and operator-overridden server values: a derived write never pins
 * them, and an unmarked stored echo is dropped once the ladder moves on.
 * @module
 */

import { Card, CardContent } from "@/components/ui/card";
import { displayLabel, decodableEncoders } from "../../../../selkies-web-core/lib/util.js";
import { resolveSpec, isSettingPinned, HIDPI_SPEC, RATE_CONTROL_SPEC,
    USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_STREAMING_MODE_SPEC,
    USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC, FORCE_ALIGNED_RESOLUTION_SPEC } from "../../../../selkies-web-core/lib/conditional-settings.js";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Slider } from "@/components/ui/slider";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { ChevronUp } from "lucide-react";
import React, { useState, useEffect, useCallback, useMemo } from "react";
import { getPrefixedKey, getRoutePrefix, computeRenderableSettings, getLastServerSettings,
    getLastEffectiveCursorState, getLastAudioDevices, isSecondaryDisplay } from "@/utils";
import { t, tl } from "@/i18n";

/**
 * Mirrors the server's `audio_bitrate` allowed enum (settings.py) so the
 * slider never offers a value the server rejects; 510000 is libopus's maximum.
 */
const audioBitrateOptions = [32000, 48000, 64000, 96000, 128000, 192000, 256000, 320000, 384000, 510000];
const DEFAULT_AUDIO_BITRATE = 128000;

/** UI scaling stops offered until the server's `scaling_dpi` enum arrives. */
const dpiScalingOptions = [
    { label: "100%", value: 96 },
    { label: "125%", value: 120 },
    { label: "150%", value: 144 },
    { label: "175%", value: 168 },
    { label: "200%", value: 192 },
    { label: "225%", value: 216 },
    { label: "250%", value: 240 },
    { label: "275%", value: 264 },
    { label: "300%", value: 288 },
];
/**
 * The default `scaling_dpi`: the local display scaling (devicePixelRatio) put
 * through the core's autoDeriveDpi formula, so the remote desktop's fonts and
 * UI match the local environment. The density is snapped to the nearest option
 * and clamped at both ends, and it is independent of the resolution.
 *
 * The ladder that governs the desktop is an operator override (which the
 * server refuses to let clients clobber), then the stored pick, then this
 * derived default; the cores send stored-else-derived on every connect.
 */
const deriveDpiFromDpr = (): number => {
    const dpr = window.devicePixelRatio || 1;
    const target = Math.round(dpr * 4) * 24;
    return dpiScalingOptions.reduce((prev, curr) =>
        Math.abs(curr.value - target) < Math.abs(prev.value - target) ? curr : prev
    ).value;
};

const commonResolutionValues = [
    "",
    "1920x1080",
    "1280x720",
    "1366x768",
    "1920x1200",
    "2560x1440",
    "3840x2160",
    "1024x768",
    "800x600",
    "640x480",
    "320x240",
];

const encoderOptions = [
    "h264enc",
    "h264enc-striped",
    "jpeg",
];

/**
 * WebRTC encoders offered until the server payload arrives; its `encoder`
 * allowed list is already filtered to what the webrtc pipeline produces.
 */
/** `webcam_encoder` values; labels come from `displayLabel`. */
const webcamEncoderOptions = ["auto", "h264", "vp8", "mjpeg"];

const encoderOptionsRTC = [
    "h264enc",
];

/** Encoders that support both CBR and CRF (constant-QP) rate control. */
const H264_ENCODERS = ["h264enc", "h264enc-striped", "nvh264enc"];

const FRAMERATE_STEPS = [8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144, 165, 240];

/** CRF stops, inside the server-supported `video_crf` range (min 5). */
const videoCRFOptions = [50, 45, 40, 35, 30, 25, 20, 10, 5];

/** Sub-Mbps CBR stops (kbps) for constrained links, ahead of the 1000-kbps steps. */
const SUB_MBPS_BITRATE_STEPS = [100, 250, 500, 750];
/**
 * CBR stops above 100000 kbps, where per-1000 granularity stops mattering and
 * a 1000-position slider would be unusable.
 */
const COARSE_MBPS_BITRATE_STEPS = [150000, 200000, 300000, 400000, 500000, 750000, 1000000];

const readStored = (key: string) => localStorage.getItem(getPrefixedKey(key));

/**
 * Suffix of the marker an explicit choice writes beside its value; settings
 * that are also derived read storage only through the marker, so a derived
 * write never pins them.
 */
const EXPLICIT_CHOICE_SUFFIX = "_explicit_choice";
/**
 * The marker key, suffixed onto the already-prefixed value key so it inherits
 * the per-display suffix and a secondary display keeps its own choice.
 */
const explicitChoiceKey = (spec: any) => `${getPrefixedKey(spec.storageKey)}${EXPLICIT_CHOICE_SUFFIX}`;
const isExplicitChoice = (spec: any) => localStorage.getItem(explicitChoiceKey(spec)) === "true";
const readExplicitStored = (spec: any) => (key: string) => (
    isExplicitChoice(spec) ? readStored(key) : null
);

/**
 * Drives a conditional setting: lazy init, then a re-resolve whenever the
 * server settings or any dependency in `deps` changes (server sync and
 * encoder or manual-resolution re-derivation alike). The resolver honors
 * explicit choices, so a re-resolve never clobbers a pinned value.
 *
 * Re-resolving writes state rather than deriving during render because the
 * caller edits the value afterwards; deriving would discard their choice.
 * @param spec Conditional-setting spec from `conditional-settings.js`.
 * @param serverSettings The last server settings payload, or null before one arrives.
 * @param ctx Resolution context the spec reads (stream mode, encoder, ...).
 * @param deps Dependencies whose change triggers a re-resolve.
 * @param read Storage reader; the explicit-choice reader for settings that are also derived.
 * @returns A `[value, setValue]` pair.
 */
function useConditionalSetting(spec: any, serverSettings: any, ctx: any, deps: any[], read: any = readStored) {
    const compute = () => resolveSpec(spec, serverSettings, ctx, read);
    const [value, setValue] = useState(compute);
    // eslint-disable-next-line react-hooks/exhaustive-deps, react-hooks/set-state-in-effect
    useEffect(() => { setValue(compute()); }, deps);
    return [value, setValue] as const;
}

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";
const STREAMING_MODES = [STREAM_MODE_WEBSOCKETS, STREAM_MODE_WEBRTC];
const DEFAULT_STREAM_MODE = STREAM_MODE_WEBSOCKETS;

const rateControlOptions = ["cbr", "crf"];
const readHidpiStored = readExplicitStored(HIDPI_SPEC);
const readRateControlStored = readExplicitStored(RATE_CONTROL_SPEC);
const readPaintOverStored = readExplicitStored(USE_PAINT_OVER_QUALITY_SPEC);
/** Default `video_bitrate` in kbps, the unit the slider and the wire share. */
const DEFAULT_VIDEO_BITRATE = 8000;

const roundDownToEven = (num: number) => {
    const n = parseInt(num.toString(), 10);
    if (isNaN(n)) return 0;
    return Math.floor(n / 2) * 2;
};

/** Trailing-edge debounce: the last call within `delay` wins. */
function debounce<A extends unknown[]>(func: (...args: A) => void, delay: number) {
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    return (...args: A) => {
        clearTimeout(timeoutId);
        timeoutId = setTimeout(() => func(...args), delay);
    };
}

/**
 * Sets the cross-script flag the cores read around a transport switch, so the
 * old peer's teardown does not surface a "Server disconnected" alert. Kept
 * outside the component: it is a signal to the runtime core, not component
 * state.
 */
function setModeSwitching(active: boolean) {
    window.__selkiesModeSwitching = active;
}

/**
 * The Settings panel: Video, Audio and Resolution tabs, each hidden when the
 * server's UI customization disables it. Server settings are seeded from the
 * cached broadcast because the panel mounts after the core connects, and every
 * value stays editable afterwards with localStorage taking precedence.
 *
 * Each conditional setting is one `useConditionalSetting` call over a shared
 * spec: the hook owns init and server sync, and client-driven changes go
 * through `writeConditional`.
 */
export function Settings() {
    const [serverSettings, setServerSettings] = useState<any>(() => getLastServerSettings());
    const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));

    const [streamMode, setStreamMode] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("stream_mode"));
        if (saved && STREAMING_MODES.includes(saved)) return saved;
        const runtimeMode = (window as any).__SELKIES_STREAMING_MODE__;
        if (runtimeMode && STREAMING_MODES.includes(runtimeMode)) return runtimeMode;
        return DEFAULT_STREAM_MODE;
    });
    const isWebrtc = streamMode === STREAM_MODE_WEBRTC;

    /**
     * On the WebSocket transport only encoders this engine can decode are
     * offered (jpeg alone without WebCodecs).
     */
    const offeredEncoders = useCallback(
        (list: string[]): string[] => (isWebrtc ? list : decodableEncoders(list)), [isWebrtc]);
    const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState(
        offeredEncoders(isWebrtc ? encoderOptionsRTC : encoderOptions)
    );

    const [manualWidth, setManualWidth] = useState(() =>
        localStorage.getItem(getPrefixedKey("manual_width")) || ''
    );
    const [manualHeight, setManualHeight] = useState(() =>
        localStorage.getItem(getPrefixedKey("manual_height")) || ''
    );
    const [presetValue, setPresetValue] = useState("");
    const [scaleLocally, setScaleLocally] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("scaleLocallyManual"));
        return saved !== null ? saved === 'true' : true;
    });

    const [selectedDpi, setSelectedDpi] = useState(() => {
        return parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")) ?? "", 10) || deriveDpiFromDpr();
    });

    const [videoBitRate, setVideoBitRate] = useState(() => {
        const parsed = parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")) ?? "", 10);
        return !isNaN(parsed) ? parsed : DEFAULT_VIDEO_BITRATE;
    });
    const [audioBitRate, setAudioBitRate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("audio_bitrate")) ?? "", 10) || DEFAULT_AUDIO_BITRATE
    );
    const [encoder, setEncoder] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder")) || "h264enc"
    );
    const [webcamEncoder, setWebcamEncoder] = useState(() =>
        localStorage.getItem(getPrefixedKey("webcam_encoder")) || "auto"
    );
    const [framerate, setFramerate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("framerate")) ?? "", 10) || 60
    );
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_crf"));
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    /**
     * State the conditional settings read; rebuilt each render so the hooks
     * below re-resolve against current values when their deps change.
     * `activeEncoder` is the one knob for both transports and reads storage
     * first: an out-of-set stored value falls to the server's own fallback and
     * the serverSettings sync re-seats it. `softwareH264Encoder` and `useCpu`
     * (client choice, else the server's) feed the rate-control default.
     */
    const conditionalCtx = {
        manualActive: !!readStored("manual_width") || serverSettings?.is_manual_resolution_mode?.value === true,
        streamMode,
        activeEncoder: readStored("encoder") || encoder,
        softwareH264Encoder: serverSettings?.software_h264_encoder?.value,
        useCpu: readStored("use_cpu") !== null
            ? readStored("use_cpu") === "true" : !!serverSettings?.use_cpu?.value,
        allowedRateControl: serverSettings?.rate_control_mode?.allowed || rateControlOptions,
    };
    const DEBOUNCE_DELAY = 500;
    const debouncedPostSetting = useMemo(() => debounce((setting: any) => {
        window.postMessage(
            { type: "settings", settings: setting },
            window.location.origin
        );
    }, DEBOUNCE_DELAY), []);

    /** The two push channels a spec's `propagate` may use. */
    const conditionalIo = {
        postSetting: (obj: any) => debouncedPostSetting(obj),
        postToCore: (obj: any) => window.postMessage(obj, window.location.origin),
    };
    /**
     * The one write path for conditional settings: optimistic setState,
     * persistence only for an explicit choice (which pins it; a derived value
     * keeps following), then propagation through the spec.
     */
    const writeConditional = (spec: any, uiValue: any, setValue: any, opts: any = {}) => {
        setValue(uiValue);
        if (opts.persist) {
            localStorage.setItem(getPrefixedKey(spec.storageKey),
                spec.serialize ? spec.serialize(uiValue) : String(uiValue));
            localStorage.setItem(explicitChoiceKey(spec), "true");
        }
        spec.propagate(spec.toServer ? spec.toServer(uiValue) : uiValue, conditionalCtx, conditionalIo);
    };

    const [hidpiEnabled, setHidpiEnabled] = useConditionalSetting(
        HIDPI_SPEC, serverSettings, conditionalCtx, [serverSettings], readHidpiStored);
    const [rateControlMode, setRateControlMode] = useConditionalSetting(
        RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings, streamMode], readRateControlStored);
    /**
     * With rate control disabled the server ignores rate_control_mode and
     * keeps the encoder's built-in default, so the dashboard neither pushes a
     * mode nor lets its own pick decide which quality slider is shown.
     */
    const rateControlEnabled = renderableSettings.enableRateControl ?? true;
    // The hook only sets UI state; when the resolved default diverges from
    // what the server applies (a transport switch seeds the previous mode's
    // value), push it so the encoder follows. Pinned values post nothing.
    useEffect(() => {
        if (!serverSettings) return;
        if (serverSettings.enable_rate_control?.value === false) return;
        const rcKey = RATE_CONTROL_SPEC.storageKey;
        const resolved = resolveSpec(
            RATE_CONTROL_SPEC, serverSettings, conditionalCtx, readRateControlStored);
        // Stale-echo rule (module docblock): an unmarked stored value that no
        // longer matches the ladder is dropped, or it outlives the derivation.
        if (!isExplicitChoice(RATE_CONTROL_SPEC)
            && readStored(rcKey) !== null && readStored(rcKey) !== resolved) {
            localStorage.removeItem(getPrefixedKey(rcKey));
        }
        if (isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readRateControlStored)) return;
        const serverValue = serverSettings[RATE_CONTROL_SPEC.serverKey]?.value;
        if (resolved && serverValue !== undefined && resolved !== serverValue) {
            writeConditional(RATE_CONTROL_SPEC, resolved, setRateControlMode, { persist: false });
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings]);
    // Same stale-echo rule for HiDPI, or a derived pick outlives its resolution mode.
    useEffect(() => {
        if (!serverSettings) return;
        const key = HIDPI_SPEC.storageKey;
        const resolved = resolveSpec(
            HIDPI_SPEC, serverSettings, conditionalCtx, readHidpiStored);
        if (!isExplicitChoice(HIDPI_SPEC)
            && readStored(key) !== null
            && readStored(key) !== HIDPI_SPEC.serialize(resolved)) {
            localStorage.removeItem(getPrefixedKey(key));
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings]);
    const [videoFullColor, setVideoFullColor] = useConditionalSetting(
        VIDEO_FULLCOLOR_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [videoStreamingMode, setVideoStreamingMode] = useConditionalSetting(
        VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // Pre-settings fallbacks mirror the server defaults (settings.py).
    const [jpegQuality, setJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("jpeg_quality")) ?? "", 10) || 40
    );
    const [paintOverJpegQuality, setPaintOverJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("paint_over_jpeg_quality")) ?? "", 10) || 90
    );
    const [videoPaintoverCRF, setVideoPaintoverCRF] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("video_paintover_crf")) ?? "", 10) || 18
    );
    const [videoPaintoverBurstFrames, setVideoPaintoverBurstFrames] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("video_paintover_burst_frames")) ?? "", 10) || 5
    );
    // Paint-over's default tracks rate control, so its resolution ctx carries
    // the mode the rc hook just settled on.
    const paintOverCtx = { ...conditionalCtx, rateControlMode };
    const [usePaintOverQuality, setUsePaintOverQuality] = useConditionalSetting(
        USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, [serverSettings, rateControlMode], readPaintOverStored);
    // Push the paint-over default the resolved rate control implies so the
    // encoder agrees (same shape as the rate-control derivation above).
    useEffect(() => {
        if (!serverSettings) return;
        const key = USE_PAINT_OVER_QUALITY_SPEC.storageKey;
        const resolved = resolveSpec(
            USE_PAINT_OVER_QUALITY_SPEC, serverSettings, { ...conditionalCtx, rateControlMode }, readPaintOverStored);
        // Same stale-echo rule as rate control.
        if (!isExplicitChoice(USE_PAINT_OVER_QUALITY_SPEC)
            && readStored(key) !== null
            && readStored(key) !== String(resolved)) {
            localStorage.removeItem(getPrefixedKey(key));
        }
        if (isSettingPinned(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, readPaintOverStored)) return;
        const serverValue = serverSettings.use_paint_over_quality?.value;
        if (resolved !== undefined && serverValue !== undefined && resolved !== serverValue) {
            writeConditional(USE_PAINT_OVER_QUALITY_SPEC, resolved, setUsePaintOverQuality, { persist: false });
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings, rateControlMode]);
    const [useCpu, setUseCpu] = useConditionalSetting(
        USE_CPU_SPEC, serverSettings, conditionalCtx, [serverSettings]);

    // Anti-aliasing stays client-only (no server truth), so it keeps its own state.
    const [antiAliasing, setAntiAliasing] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useConditionalSetting(
        USE_BROWSER_CURSORS_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    /**
     * The cursor mode the core reports as actually in effect (multi-monitor
     * forces browser cursors on), null until reported; the toggle shows it
     * over the stored preference so it never lies about the live state. Seeded
     * from the cached report because the core emits it before this panel mounts.
     */
    const [effectiveCursor, setEffectiveCursor] = useState<boolean | null>(getLastEffectiveCursorState);
    const [forceAlignedResolution, setForceAlignedResolution] = useConditionalSetting(
        FORCE_ALIGNED_RESOLUTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);

    const [audioInputDevices, setAudioInputDevices] = useState<any[]>([]);
    const [audioOutputDevices, setAudioOutputDevices] = useState<any[]>([]);
    const [selectedInputDeviceId, setSelectedInputDeviceId] = useState(() => getLastAudioDevices().input ?? 'default');
    const [selectedOutputDeviceId, setSelectedOutputDeviceId] = useState(() => getLastAudioDevices().output ?? 'default');
    const [isOutputSelectionSupported, setIsOutputSelectionSupported] = useState(false);
    const [audioDeviceError, setAudioDeviceError] = useState<string | null>(null);
    const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);

    useEffect(() => {
        const handleMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            if (event.data?.type === "serverSettings") {
                console.log("Settings received server settings:", event.data.payload);
                setServerSettings(event.data.payload);
                setRenderableSettings(computeRenderableSettings(event.data.payload));
            }
            if (event.data?.type === "effectiveCursorState" && typeof event.data.value === "boolean") {
                setEffectiveCursor(event.data.value);
            }
            // Echo of this dashboard's own pick: the dropdown shows what the core was told.
            if (event.data?.type === "audioDeviceSelected" && event.data.deviceId) {
                if (event.data.context === "input") {
                    setSelectedInputDeviceId(event.data.deviceId);
                } else if (event.data.context === "output") {
                    setSelectedOutputDeviceId(event.data.deviceId);
                }
            }
        };
        window.addEventListener("message", handleMessage);
        return () => {
            window.removeEventListener("message", handleMessage);
        };
    }, []);

    // Seeding and re-clamping write state rather than deriving in render:
    // every value stays editable afterwards, so recomputing would discard edits.
    /* eslint-disable react-hooks/set-state-in-effect */
    useEffect(() => {
        if (!serverSettings) return;

        const getStoredInt = (key: string) => parseInt(localStorage.getItem(getPrefixedKey(key)) ?? "", 10);

        const s_encoder = serverSettings.encoder;
        if (s_encoder) {
            const allowed = offeredEncoders(s_encoder.allowed);
            const stored = localStorage.getItem(getPrefixedKey("encoder"));
            const final = stored !== null && allowed.includes(stored) ? stored
                : (allowed.includes(s_encoder.value) || allowed.length === 0) ? s_encoder.value : allowed[0];
            setEncoder(final);
            setDynamicEncoderOptions(allowed);
        }

        const s_framerate = serverSettings.framerate;
        if (s_framerate) {
            const stored = getStoredInt("framerate");
            const final = !isNaN(stored)
                ? Math.max(s_framerate.min, Math.min(s_framerate.max, stored))
                : s_framerate.default;
            setFramerate(final);
        }

        const s_video_bitrate = serverSettings.video_bitrate;
        if (s_video_bitrate) {
            const stored = parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")) ?? "", 10);
            const final = !isNaN(stored)
                ? Math.max(s_video_bitrate.min, Math.min(s_video_bitrate.max, stored))
                : s_video_bitrate.default;
            setVideoBitRate(final);
        }

        const s_audio_bitrate = serverSettings.audio_bitrate;
        if (s_audio_bitrate) {
            const stored = getStoredInt("audio_bitrate");
            // `allowed` holds string bps ("128000"), `stored`/`value` are
            // numbers: compare as strings and parse the fallback.
            const final = !isNaN(stored)
                ? (s_audio_bitrate.allowed
                    ? (s_audio_bitrate.allowed.includes(String(stored)) ? stored : parseInt(s_audio_bitrate.value, 10))
                    : Math.max(s_audio_bitrate.min ?? stored, Math.min(s_audio_bitrate.max ?? stored, stored)))
                : parseInt(s_audio_bitrate.value, 10);
            setAudioBitRate(final);
        }

        const s_video_crf = serverSettings.video_crf;
        if (s_video_crf) {
            const stored = getStoredInt("video_crf");
            const final = !isNaN(stored)
                ? Math.max(s_video_crf.min, Math.min(s_video_crf.max, stored))
                : s_video_crf.default;
            setVideoCRF(final);
        }

        const s_jpeg_quality = serverSettings.jpeg_quality;
        if (s_jpeg_quality) {
            const stored = getStoredInt("jpeg_quality");
            const final = !isNaN(stored)
                ? Math.max(s_jpeg_quality.min, Math.min(s_jpeg_quality.max, stored))
                : s_jpeg_quality.default;
            setJpegQuality(final);
        }

        const s_paint_over_jpeg_quality = serverSettings.paint_over_jpeg_quality;
        if (s_paint_over_jpeg_quality) {
            const stored = getStoredInt("paint_over_jpeg_quality");
            const final = !isNaN(stored)
                ? Math.max(s_paint_over_jpeg_quality.min, Math.min(s_paint_over_jpeg_quality.max, stored))
                : s_paint_over_jpeg_quality.default;
            setPaintOverJpegQuality(final);
        }

        const s_video_paintover_crf = serverSettings.video_paintover_crf;
        if (s_video_paintover_crf) {
            const stored = getStoredInt("video_paintover_crf");
            const final = !isNaN(stored)
                ? Math.max(s_video_paintover_crf.min, Math.min(s_video_paintover_crf.max, stored))
                : s_video_paintover_crf.default;
            setVideoPaintoverCRF(final);
        }

        const s_paintover_burst = serverSettings.video_paintover_burst_frames;
        if (s_paintover_burst) {
            const stored = getStoredInt("video_paintover_burst_frames");
            const final = !isNaN(stored)
                ? Math.max(s_paintover_burst.min, Math.min(s_paintover_burst.max, stored))
                : s_paintover_burst.default;
            setVideoPaintoverBurstFrames(final);
        }

        const s_scaling_dpi = serverSettings.scaling_dpi;
        if (s_scaling_dpi) {
            const stored = getStoredInt("scaling_dpi");
            const storedAllowed = s_scaling_dpi.allowed.includes(String(stored));
            const serverVal = parseInt(s_scaling_dpi.value, 10);
            const derived = deriveDpiFromDpr();
            const willPostDerived = !storedAllowed && !s_scaling_dpi.overridden
                && derived !== serverVal;
            const final = s_scaling_dpi.overridden ? serverVal
                : storedAllowed ? stored
                : derived;
            setSelectedDpi(final);
            if (willPostDerived) {
                debouncedPostSetting({ scaling_dpi: derived });
            }
        }
    }, [serverSettings, streamMode, debouncedPostSetting, offeredEncoders]);
    /* eslint-enable react-hooks/set-state-in-effect */

    const audioDevicesRequested = React.useRef(false);
    /**
     * Populates the audio device lists once. Enumerating labelled devices
     * needs a getUserMedia grant, so this runs only when the Audio tab is
     * actually shown: merely opening Settings must not raise a microphone
     * permission prompt.
     *
     * Output selection is probed on the sink the active core plays through,
     * `HTMLMediaElement.setSinkId` for the WebRTC core's video element and
     * `AudioContext.setSinkId` (which Firefox lacks) for the WebSocket core;
     * probing the wrong one would render a picker that does nothing.
     */
    const ensureAudioDevices = useCallback(() => {
        if (audioDevicesRequested.current) return;
        audioDevicesRequested.current = true;
        const populateAudioDevices = async () => {
            setIsLoadingAudioDevices(true);
            setAudioDeviceError(null);
            setAudioInputDevices([]);
            setAudioOutputDevices([]);

            const supportsSinkId = isWebrtc
                ? 'setSinkId' in HTMLMediaElement.prototype
                : typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype;
            setIsOutputSelectionSupported(supportsSinkId);

            try {
                const tempStream = await navigator.mediaDevices.getUserMedia({ audio: true });
                tempStream.getTracks().forEach(track => track.stop());

                const devices = await navigator.mediaDevices.enumerateDevices();
                const inputs: { deviceId: string; label: string }[] = [];
                const outputs: { deviceId: string; label: string }[] = [];

                devices.forEach((device, index) => {
                    if (!device.deviceId) return;
                    const label = device.label || t(device.kind === 'audiooutput' ? 'sections.audio.defaultOutputLabelFallback' : 'sections.audio.defaultInputLabelFallback', { index: index + 1 });

                    if (device.kind === 'audioinput') {
                        inputs.push({ deviceId: device.deviceId, label: label });
                    } else if (device.kind === 'audiooutput' && supportsSinkId) {
                        outputs.push({ deviceId: device.deviceId, label: label });
                    }
                });

                setAudioInputDevices(inputs);
                setAudioOutputDevices(outputs);
            } catch (err) {
                const error = err instanceof Error ? err : new Error(String(err));
                console.error('Error getting media devices:', error);
                const messageKey = error.name === 'NotAllowedError' ? 'sections.audio.deviceErrorPermission'
                    : error.name === 'NotFoundError' ? 'sections.audio.deviceErrorNotFound'
                    : 'sections.audio.deviceErrorDefault';
                setAudioDeviceError(t(messageKey, { errorName: error.name || 'unknown' }));
            } finally {
                setIsLoadingAudioDevices(false);
            }
        };

        populateAudioDevices();
    }, [isWebrtc]);

    /**
     * A half-typed size stays in component state: the stored `manual_width` and
     * `manual_height` mean "a manual resolution is applied", which the HiDPI
     * and UI-scaling derivations read, so only Set, a preset and Reset write them.
     */
    const handleManualWidthChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setManualWidth(event.target.value);
        setPresetValue("");
    };

    const handleManualHeightChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setManualHeight(event.target.value);
        setPresetValue("");
    };

    /** The core persists scaleLocallyManual itself when it applies the message. */
    const handleScaleLocallyToggle = () => {
        const newState = !scaleLocally;
        setScaleLocally(newState);
        window.postMessage({ type: 'setScaleLocally', value: newState }, window.location.origin);
    };

    /** An explicit toggle pins the choice; the core persists useCssScaling when it applies the message. */
    const handleHidpiToggle = () => {
        writeConditional(HIDPI_SPEC, !hidpiEnabled, setHidpiEnabled, { persist: true });
    };

    const handleDpiScalingChange = (value: string) => {
        const newDpi = parseInt(value, 10);
        setSelectedDpi(newDpi);
        localStorage.setItem(getPrefixedKey('scaling_dpi'), newDpi.toString());
        debouncedPostSetting({ scaling_dpi: newDpi });
    };

    /**
     * Asks the server to swap transports, then lets the core loader persist
     * the mode and reload the page into the new stack.
     *
     * `/api/switch` is gated on the master token (Bearer) when set, or Basic
     * credentials via same-origin. With Basic Auth off the Bearer is required
     * but the dashboard is not given it, so a 401 prompts once, keeps the token
     * in sessionStorage, and retries.
     */
    const handleStreamModeChange = async (mode: string) => {
        if (mode === streamMode) return;
        // /api/switch tears down the old peer (WS close 4000) before responding, so
        // the flag must precede the request or the core alerts "Server disconnected".
        setModeSwitching(true);
        try {
            const MASTER_TOKEN_KEY = "selkies_master_token";
            const doSwitch = () => {
                const headers: Record<string, string> = { "Content-Type": "application/json" };
                let storedToken: string | null = null;
                try { storedToken = sessionStorage.getItem(MASTER_TOKEN_KEY); } catch { /* sessionStorage unavailable */ }
                if (storedToken) headers["Authorization"] = `Bearer ${storedToken}`;
                return fetch(`${getRoutePrefix()}/api/switch`, {
                    method: "POST",
                    headers,
                    credentials: "same-origin",
                    body: JSON.stringify({ mode }),
                });
            };
            let response = await doSwitch();
            if (response.status === 401) {
                const entered = (typeof window !== "undefined" && window.prompt)
                    ? window.prompt("Switching the stream mode requires the Selkies master token:")
                    : null;
                if (entered && entered.trim()) {
                    try { sessionStorage.setItem(MASTER_TOKEN_KEY, entered.trim()); } catch { /* sessionStorage unavailable */ }
                    response = await doSwitch();
                }
            }
            if (!response.ok) {
                // Drop a stale token on 401 so the next attempt re-prompts.
                if (response.status === 401) { try { sessionStorage.removeItem(MASTER_TOKEN_KEY); } catch { /* sessionStorage unavailable */ } }
                throw new Error(`Request failed with status ${response.status}`);
            }
            setStreamMode(mode);
            window.postMessage({ type: "mode", mode }, window.location.origin);
        } catch (error) {
            // The switch failed, so no reload follows; clear the flag or a real
            // disconnect afterwards would be silently suppressed.
            setModeSwitching(false);
            console.error("Error switching stream mode:", error);
        }
    };

    /**
     * Re-derives rate control from the encoder and software encoding unless
     * it is pinned by an explicit client or server choice. A derived change
     * is not persisted, so it keeps following. `ctxOverrides` carries the
     * value just chosen, ahead of the re-render that would put it in
     * conditionalCtx.
     */
    const rederiveRateControl = (ctxOverrides: Record<string, unknown>) => {
        if (!rateControlEnabled
            || isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readRateControlStored)) return;
        const rcResolved = resolveSpec(
            RATE_CONTROL_SPEC, serverSettings,
            { ...conditionalCtx, ...ctxOverrides }, readRateControlStored);
        if (rcResolved !== rateControlMode) {
            writeConditional(RATE_CONTROL_SPEC, rcResolved, setRateControlMode, { persist: false });
        }
    };
    const handleEncoderChange = (selectedEncoder: string) => {
        setEncoder(selectedEncoder);
        localStorage.setItem(getPrefixedKey('encoder'), selectedEncoder);
        debouncedPostSetting({ encoder: selectedEncoder });
        rederiveRateControl({ activeEncoder: selectedEncoder });
    };

    const handleWebcamEncoderChange = (preference: string) => {
        setWebcamEncoder(preference);
        localStorage.setItem(getPrefixedKey("webcam_encoder"), preference);
        debouncedPostSetting({ webcam_encoder: preference });
    };
    // The server default; locked overrides the stored choice.
    useEffect(() => {
        const wce = serverSettings?.webcam_encoder;
        if (!wce || !webcamEncoderOptions.includes(wce.value)) return;
        const stored = localStorage.getItem(getPrefixedKey("webcam_encoder"));
        setWebcamEncoder(wce.locked || !webcamEncoderOptions.includes(stored ?? "") ? wce.value : stored!);
    }, [serverSettings]);

    const handleFramerateChange = (selectedFramerate: number) => {
        setFramerate(selectedFramerate);
        localStorage.setItem(getPrefixedKey('framerate'), selectedFramerate.toString());
        debouncedPostSetting({ framerate: selectedFramerate });
    };

    const handleVideoCRFChange = (selectedCRF: number) => {
        setVideoCRF(selectedCRF);
        localStorage.setItem(getPrefixedKey('video_crf'), selectedCRF.toString());
        debouncedPostSetting({ video_crf: selectedCRF });
    };

    /** An explicit choice is persisted, which pins it against encoder changes. */
    const handleRateControlChange = (mode: string) => {
        writeConditional(RATE_CONTROL_SPEC, mode, setRateControlMode, { persist: true });
    };

    const handleVideoBitRateChange = (selectedBitRate: number) => {
        setVideoBitRate(selectedBitRate);
        localStorage.setItem(getPrefixedKey('video_bitrate'), selectedBitRate.toString());
        debouncedPostSetting({ video_bitrate: selectedBitRate });
    };

    const handleJpegQualityChange = (selectedQuality: number) => {
        setJpegQuality(selectedQuality);
        localStorage.setItem(getPrefixedKey('jpeg_quality'), selectedQuality.toString());
        debouncedPostSetting({ jpeg_quality: selectedQuality });
    };

    const handlePaintOverJpegQualityChange = (selectedQuality: number) => {
        setPaintOverJpegQuality(selectedQuality);
        localStorage.setItem(getPrefixedKey('paint_over_jpeg_quality'), selectedQuality.toString());
        debouncedPostSetting({ paint_over_jpeg_quality: selectedQuality });
    };

    const handleH264PaintoverCRFChange = (selectedCRF: number) => {
        setVideoPaintoverCRF(selectedCRF);
        localStorage.setItem(getPrefixedKey('video_paintover_crf'), selectedCRF.toString());
        debouncedPostSetting({ video_paintover_crf: selectedCRF });
    };

    const handleH264PaintoverBurstChange = (selectedFrames: number) => {
        setVideoPaintoverBurstFrames(selectedFrames);
        localStorage.setItem(getPrefixedKey('video_paintover_burst_frames'), selectedFrames.toString());
        debouncedPostSetting({ video_paintover_burst_frames: selectedFrames });
    };

    const handleH264FullColorToggle = () => {
        writeConditional(VIDEO_FULLCOLOR_SPEC, !videoFullColor, setVideoFullColor, { persist: true });
    };

    const handleH264StreamingModeToggle = () => {
        writeConditional(VIDEO_STREAMING_MODE_SPEC, !videoStreamingMode, setVideoStreamingMode, { persist: true });
    };

    const handleUsePaintOverQualityToggle = () => {
        writeConditional(USE_PAINT_OVER_QUALITY_SPEC, !usePaintOverQuality, setUsePaintOverQuality, { persist: true });
    };

    const handleUseCpuToggle = () => {
        writeConditional(USE_CPU_SPEC, !useCpu, setUseCpu, { persist: true });
        rederiveRateControl({ useCpu: !useCpu });
    };

    /** Anti-aliasing is client-only; the core persists antiAliasingEnabled itself. */
    const handleAntiAliasingToggle = () => {
        const newState = !antiAliasing;
        setAntiAliasing(newState);
        window.postMessage(
            { type: 'setAntiAliasing', value: newState },
            window.location.origin
        );
    };

    /**
     * Propagates the new preference and lets the core, which owns persistence,
     * report the effective (possibly multi-monitor-forced) value back. Derived
     * from the displayed value: while multi-monitor forces the toggle on, the
     * base preference may be off, and negating the base would silently persist
     * the forced value over the user's real choice.
     */
    const handleUseBrowserCursorsToggle = () => {
        writeConditional(USE_BROWSER_CURSORS_SPEC, !(effectiveCursor ?? useBrowserCursors), setUseBrowserCursors, { persist: false });
    };

    const handleForceAlignedResolutionToggle = () => {
        writeConditional(FORCE_ALIGNED_RESOLUTION_SPEC, !forceAlignedResolution, setForceAlignedResolution, { persist: true });
    };

    /**
     * Pairs the resolution mode with CSS scaling: HiDPI off when a manual or
     * preset resolution is set, on when reset, as a derived (unpinned) write.
     * An explicit toggle or a locked or overridden server value pins HiDPI and
     * stops the resolution buttons from re-deriving it.
     */
    const deriveHidpiForResolution = (manual: boolean) => {
        if (isSettingPinned(HIDPI_SPEC, serverSettings, readHidpiStored)) return;
        writeConditional(HIDPI_SPEC, !manual, setHidpiEnabled, { persist: false });
    };

    /**
     * Restores HiDPI to its default on reset-to-window. Unlike the
     * resolution-derived writes, which respect a pinned choice, a reset means
     * "back to defaults", so the client's own pin is dropped even under an
     * operator-explicit value: `use_css_scaling` overridden does not imply
     * locked, and a kept pin would keep outranking the operator's value in the
     * resolution ladder. The operator value (when explicit) or the derived
     * default is then applied without storing; only a locked value leaves
     * everything alone.
     */
    const resetHidpiToDerivedDefault = () => {
        const s = serverSettings?.use_css_scaling;
        if (s?.locked) return;
        localStorage.removeItem(getPrefixedKey(HIDPI_SPEC.storageKey));
        localStorage.removeItem(explicitChoiceKey(HIDPI_SPEC));
        const uiValue = s?.overridden ? s.value !== true : true;
        writeConditional(HIDPI_SPEC, uiValue, setHidpiEnabled, { persist: false });
    };

    /**
     * Returns UI scaling to its derived (devicePixelRatio) default on
     * reset-to-window: the pinned client choice is dropped and the derived
     * value propagates like a user change. A locked or operator-overridden
     * value governs scaling instead, the same gate as the startup
     * derived-default post, so nothing happens then.
     */
    const resetDpiToDerivedDefault = () => {
        const s = serverSettings?.scaling_dpi;
        if (s?.locked || s?.overridden) return;
        localStorage.removeItem(getPrefixedKey('scaling_dpi'));
        const derived = deriveDpiFromDpr();
        setSelectedDpi(derived);
        debouncedPostSetting({ scaling_dpi: derived });
    };

    const handleSetManualResolution = () => {
        const widthVal = manualWidth.trim();
        const heightVal = manualHeight.trim();
        const width = parseInt(widthVal, 10);
        const height = parseInt(heightVal, 10);

        if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
            alert(t('alerts.invalidResolution'));
            return;
        }
        const evenWidth = roundDownToEven(width);
        const evenHeight = roundDownToEven(height);
        setManualWidth(evenWidth.toString());
        setManualHeight(evenHeight.toString());
        setPresetValue("");
        localStorage.setItem(getPrefixedKey('manual_width'), evenWidth.toString());
        localStorage.setItem(getPrefixedKey('manual_height'), evenHeight.toString());
        window.postMessage({ type: 'setManualResolution', width: evenWidth, height: evenHeight }, window.location.origin);
        deriveHidpiForResolution(true);
    };

    const handleResetResolution = () => {
        setManualWidth('');
        setManualHeight('');
        setPresetValue("");
        localStorage.removeItem(getPrefixedKey('manual_width'));
        localStorage.removeItem(getPrefixedKey('manual_height'));
        window.postMessage({ type: 'resetResolutionToWindow' }, window.location.origin);
        resetHidpiToDerivedDefault();
        resetDpiToDerivedDefault();
    };

    /** CBR stops: the sub-Mbps steps, whole-Mbps steps to 100000, then the coarse steps, clipped to the server range. */
    const videoBitrateOptions = (() => {
        const min = serverSettings?.video_bitrate?.min ?? 100;
        const max = serverSettings?.video_bitrate?.max ?? 1000000;
        const stops = SUB_MBPS_BITRATE_STEPS.filter(v => v >= min && v <= max);
        for (let v = Math.max(1000, Math.ceil(min / 1000) * 1000); v <= Math.min(100000, Math.floor(max / 1000) * 1000); v += 1000) stops.push(v);
        stops.push(...COARSE_MBPS_BITRATE_STEPS.filter(v => v >= min && v <= max));
        return stops.length ? stops : [min];
    })();
    /** Framerate stops clipped to the server-allowed span, as the stored value itself is clamped. */
    const framerateOptions = (() => {
        const min = serverSettings?.framerate?.min ?? 8;
        const max = serverSettings?.framerate?.max ?? 240;
        const stops = FRAMERATE_STEPS.filter(v => v >= min && v <= max);
        return stops.length ? stops : [min];
    })();
    const framerateIndex = (() => {
        const exact = framerateOptions.indexOf(framerate);
        if (exact >= 0) return exact;
        const above = framerateOptions.findIndex(v => v >= framerate);
        return above >= 0 ? above : framerateOptions.length - 1;
    })();
    const bitrateIndex = (() => {
        const exact = videoBitrateOptions.indexOf(videoBitRate);
        if (exact >= 0) return exact;
        const above = videoBitrateOptions.findIndex(v => v >= videoBitRate);
        return above >= 0 ? above : videoBitrateOptions.length - 1;
    })();
    /**
     * CRF stops clipped to the server-allowed span. The list descends (higher
     * quality to the right), so the nearest fallback for an off-stop value
     * (server default, clamp) is the first stop at or below it.
     */
    const videoCRFChoices = (() => {
        const min = serverSettings?.video_crf?.min ?? 5;
        const max = serverSettings?.video_crf?.max ?? 50;
        const stops = videoCRFOptions.filter(v => v >= min && v <= max);
        return stops.length ? stops : [min];
    })();
    const videoCRFIndex = (() => {
        const exact = videoCRFChoices.indexOf(videoCRF);
        if (exact >= 0) return exact;
        const below = videoCRFChoices.findIndex(v => v <= videoCRF);
        return below >= 0 ? below : videoCRFChoices.length - 1;
    })();
    const formatBitrate = (v: number) => `${v / 1000} Mbps`;

    const audioBitrateChoices = (serverSettings?.audio_bitrate?.allowed?.map((v: string) => parseInt(v, 10))) || audioBitrateOptions;
    const dpiScalingChoices: { label: string; value: number }[] = (serverSettings?.scaling_dpi?.allowed?.map((v: string) => {
        const value = parseInt(v, 10);
        return { label: `${Math.round((value / 96) * 100)}%`, value };
    })) || dpiScalingOptions;
    /**
     * A single allowed stop, or an operator-set DPI (the server drops client
     * DPI syncs while scaling_dpi is overridden), leaves nothing to change.
     */
    const dpiScalingDisabled = !serverSettings || serverSettings.scaling_dpi?.allowed?.length <= 1
        || serverSettings.scaling_dpi?.overridden === true;
    const activeEncoder = encoder;
    const isH264 = H264_ENCODERS.includes(activeEncoder);
    const showJpegOptions = !isWebrtc && activeEncoder === 'jpeg';
    const showRateControl = rateControlEnabled && isH264;
    /**
     * The mode the encoder is actually using, which the quality slider must
     * belong to: with rate control disabled that is the server's mode.
     */
    const appliedRateControlMode = rateControlEnabled
        ? rateControlMode
        : (serverSettings?.rate_control_mode?.value ?? rateControlMode);
    const encoderRenderable = renderableSettings.encoder ?? true;
    const webcamEncoderRenderable = (renderableSettings.webcamEncoder ?? true) && !isWebrtc;

    const showVideoTab = renderableSettings.videoSettings !== false;
    const showAudioTab = renderableSettings.audioSettings !== false;
    const showResolutionTab = renderableSettings.screenSettings !== false;
    const visibleTabCount = [showVideoTab, showAudioTab, showResolutionTab].filter(Boolean).length;
    const defaultTab = showVideoTab ? "video" : showAudioTab ? "audio" : "resolution";

    // Audio is the mount-time tab whenever Video is hidden, so it counts as shown.
    useEffect(() => {
        if (defaultTab === "audio") ensureAudioDevices();
    }, [defaultTab, ensureAudioDevices]);

    if (visibleTabCount === 0) {
        return null;
    }

    return (
        <Card className="w-[300px] p-0 pb-4 bg-background/95 backdrop-blur-sm border shadow-sm">
            <Tabs
                defaultValue={defaultTab}
                onValueChange={(value) => { if (value === "audio") ensureAudioDevices(); }}
                className="w-full"
            >
                <TabsList className={`grid w-full bg-muted/50 ${visibleTabCount === 3 ? 'grid-cols-3' : visibleTabCount === 2 ? 'grid-cols-2' : 'grid-cols-1'}`}>
                    {showVideoTab && <TabsTrigger value="video">{t('settingsTabs.video')}</TabsTrigger>}
                    {showAudioTab && <TabsTrigger value="audio">{t('settingsTabs.audio')}</TabsTrigger>}
                    {showResolutionTab && <TabsTrigger value="resolution">{t('settingsTabs.resolution')}</TabsTrigger>}
                </TabsList>

                {showResolutionTab && (
                <TabsContent value="resolution">
                    <CardContent className="space-y-4">
                        {/* Per-display capable settings (the core routes them with a
                            _display2 suffix): available on secondary displays too. */}
                        <div className="flex items-center justify-between">
                            <div className="space-y-0.5">
                                <label className="text-sm font-medium">{t('sections.screen.antiAliasingLabel')}</label>
                            </div>
                            <Switch
                                checked={antiAliasing}
                                onCheckedChange={handleAntiAliasingToggle}
                            />
                        </div>

                        {(renderableSettings.useBrowserCursors ?? true) && (
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <label className="text-sm font-medium">{t('sections.screen.useNativeCursorStylesLabel')}</label>
                                </div>
                                <Switch
                                    checked={effectiveCursor !== null ? effectiveCursor : useBrowserCursors}
                                    onCheckedChange={handleUseBrowserCursorsToggle}
                                />
                            </div>
                        )}

                        {!isSecondaryDisplay && (
                            <>
                                {(renderableSettings.hidpi ?? true) && (
                                    <div className="flex items-center justify-between">
                                        <div className="space-y-0.5">
                                            <label className="text-sm font-medium"
                                                title={serverSettings?.enable_resize?.value === false
                                                    ? t('sections.screen.hidpiDisabledNoResizeTitle')
                                                    : undefined}>{t('sections.screen.hidpiLabel')}</label>
                                        </div>
                                        <Switch
                                            checked={hidpiEnabled}
                                            onCheckedChange={handleHidpiToggle}
                                            disabled={serverSettings?.enable_resize?.value === false}
                                        />
                                    </div>
                                )}

                                {(renderableSettings.forceAlignedResolution ?? true) && (
                                    <div className="flex items-center justify-between">
                                        <div className="space-y-0.5">
                                            <label className="text-sm font-medium" title={t('sections.screen.forceAlignedResolutionDetails')}>{t('sections.screen.forceAlignedResolutionLabel')}</label>
                                        </div>
                                        <Switch
                                            checked={forceAlignedResolution}
                                            onCheckedChange={handleForceAlignedResolutionToggle}
                                        />
                                    </div>
                                )}

                                {(renderableSettings.uiScaling ?? true) && (
                                    <div className="space-y-2">
                                        <label className="text-sm font-medium">{t('sections.screen.uiScalingLabel')}</label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger asChild>
                                                <Button variant="outline" className="w-full justify-between" disabled={dpiScalingDisabled}>
                                                    {dpiScalingChoices.find(option => option.value === selectedDpi)?.label || "100%"}
                                                    <ChevronUp className="h-4 w-4 rotate-180" />
                                                </Button>
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent className="w-full">
                                                {dpiScalingChoices.map((option) => (
                                                    <DropdownMenuItem
                                                        key={option.value}
                                                        onClick={() => handleDpiScalingChange(option.value.toString())}
                                                    >
                                                        {option.label}
                                                    </DropdownMenuItem>
                                                ))}
                                            </DropdownMenuContent>
                                        </DropdownMenu>
                                    </div>
                                )}
                            </>
                        )}

                        {!serverSettings?.is_manual_resolution_mode?.locked && (
                            <>
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{tl('sections.screen.presetLabel')}</label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger asChild>
                                            <Button variant="outline" className="w-full justify-between">
                                                {presetValue || t('sections.screen.resolutionPresetSelect')}
                                                <ChevronUp className="h-4 w-4 rotate-180" />
                                            </Button>
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent className="w-full">
                                            {commonResolutionValues.slice(1).map((res) => (
                                                <DropdownMenuItem
                                                    key={res}
                                                    onClick={() => {
                                                        setPresetValue(res);
                                                        const parts = res.split('x');
                                                        if (parts.length === 2) {
                                                            const width = parseInt(parts[0], 10);
                                                            const height = parseInt(parts[1], 10);

                                                            if (!isNaN(width) && width > 0 && !isNaN(height) && height > 0) {
                                                                const evenWidth = roundDownToEven(width);
                                                                const evenHeight = roundDownToEven(height);

                                                                setManualWidth(evenWidth.toString());
                                                                setManualHeight(evenHeight.toString());
                                                                localStorage.setItem(getPrefixedKey('manual_width'), evenWidth.toString());
                                                                localStorage.setItem(getPrefixedKey('manual_height'), evenHeight.toString());
                                                                window.postMessage({ type: 'setManualResolution', width: evenWidth, height: evenHeight }, window.location.origin);
                                                                deriveHidpiForResolution(true);
                                                            }
                                                        }
                                                    }}
                                                >
                                                    {res}
                                                </DropdownMenuItem>
                                            ))}
                                        </DropdownMenuContent>
                                    </DropdownMenu>
                                </div>

                                <div className="flex gap-2">
                                    <div className="flex-1 space-y-2">
                                        <label className="text-sm font-medium">{tl('sections.screen.widthLabel')}</label>
                                        <Input
                                            type="number"
                                            value={manualWidth}
                                            onChange={handleManualWidthChange}
                                            placeholder={t('sections.screen.widthPlaceholder')}
                                            min="1"
                                            step="2"
                                            className="[appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                                        />
                                    </div>
                                    <div className="flex-1 space-y-2">
                                        <label className="text-sm font-medium">{tl('sections.screen.heightLabel')}</label>
                                        <Input
                                            type="number"
                                            value={manualHeight}
                                            onChange={handleManualHeightChange}
                                            placeholder={t('sections.screen.heightPlaceholder')}
                                            min="1"
                                            step="2"
                                            className="[appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                                        />
                                    </div>
                                </div>

                                <div className="flex gap-2">
                                    <Button
                                        variant="outline"
                                        className="flex-1"
                                        onClick={handleSetManualResolution}
                                    >
                                        {t('screen.setButton')}
                                    </Button>
                                    <Button
                                        variant="outline"
                                        className="flex-1"
                                        onClick={handleResetResolution}
                                    >
                                        {t('sections.screen.resetButton')}
                                    </Button>
                                </div>
                            </>
                        )}

                        <Button
                            variant={scaleLocally ? "default" : "outline"}
                            className="w-full"
                            onClick={handleScaleLocallyToggle}
                        >
                            {tl('sections.screen.scaleLocallyLabel')}: {t(scaleLocally ? 'sections.screen.scaleLocallyOn' : 'sections.screen.scaleLocallyOff')}
                        </Button>
                    </CardContent>
                </TabsContent>
                )}

                {showVideoTab && (
                <TabsContent value="video">
                    <CardContent className="space-y-4">
                        {(renderableSettings.enableDualMode ?? (window as any).__SELKIES_DUAL_MODE__ ?? false) && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{t('streamingModeTitle')}</label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger asChild>
                                        <Button variant="outline" className="w-full justify-between">
                                            {displayLabel(streamMode)}
                                            <ChevronUp className="h-4 w-4 rotate-180" />
                                        </Button>
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-full">
                                        {STREAMING_MODES.map(mode => (
                                            <DropdownMenuItem
                                                key={mode}
                                                onClick={() => handleStreamModeChange(mode)}
                                            >
                                                {displayLabel(mode)}
                                            </DropdownMenuItem>
                                        ))}
                                    </DropdownMenuContent>
                                </DropdownMenu>
                            </div>
                        )}

                        {encoderRenderable && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{tl('sections.video.encoderLabel')}</label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger asChild>
                                        <Button variant="outline" className="w-full justify-between">
                                            {displayLabel(activeEncoder)}
                                            <ChevronUp className="h-4 w-4 rotate-180" />
                                        </Button>
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-full">
                                        {dynamicEncoderOptions.map(enc => (
                                            <DropdownMenuItem
                                                key={enc}
                                                onClick={() => handleEncoderChange(enc)}
                                            >
                                                {displayLabel(enc)}
                                            </DropdownMenuItem>
                                        ))}
                                    </DropdownMenuContent>
                                </DropdownMenu>
                            </div>
                        )}

                        {webcamEncoderRenderable && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{tl('sections.video.webcamEncoderLabel')}</label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger asChild>
                                        <Button variant="outline" className="w-full justify-between"
                                            disabled={!!serverSettings?.webcam_encoder?.locked}>
                                            {displayLabel(webcamEncoder)}
                                            <ChevronUp className="h-4 w-4 rotate-180" />
                                        </Button>
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-full">
                                        {webcamEncoderOptions.map(pref => (
                                            <DropdownMenuItem
                                                key={pref}
                                                onClick={() => handleWebcamEncoderChange(pref)}
                                            >
                                                {displayLabel(pref)}
                                            </DropdownMenuItem>
                                        ))}
                                    </DropdownMenuContent>
                                </DropdownMenu>
                            </div>
                        )}

                        {(renderableSettings.framerate ?? true) && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{tl('sections.video.framerateLabel', { framerate })}</label>
                                <div className="flex items-center gap-2">
                                    <Slider
                                        min={0}
                                        max={framerateOptions.length - 1}
                                        step={1}
                                        value={[framerateIndex]}
                                        onValueChange={(value) => {
                                            const index = value[0];
                                            const selectedFramerate = framerateOptions[index];
                                            if (selectedFramerate !== undefined) {
                                                handleFramerateChange(selectedFramerate);
                                            }
                                        }}
                                        className="flex-1"
                                    />
                                </div>
                            </div>
                        )}


                        {isH264 && (
                            <>
                                {showRateControl && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{t('sections.video.rateControlLabel')}</label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger asChild>
                                            <Button variant="outline" className="w-full justify-between">
                                                {displayLabel(rateControlMode)}
                                                <ChevronUp className="h-4 w-4 rotate-180" />
                                            </Button>
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent className="w-full">
                                            {(serverSettings?.rate_control_mode?.allowed || rateControlOptions).map((mode: string) => (
                                                <DropdownMenuItem key={mode} onClick={() => handleRateControlChange(mode)}>
                                                    {displayLabel(mode)}
                                                </DropdownMenuItem>
                                            ))}
                                        </DropdownMenuContent>
                                    </DropdownMenu>
                                </div>
                                )}

                                {appliedRateControlMode === 'cbr' && (renderableSettings.videoBitrate ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{tl('sections.video.bitrateLabel', { bitrate: formatBitrate(videoBitRate) })}</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={0}
                                            max={videoBitrateOptions.length - 1}
                                            step={1}
                                            value={[bitrateIndex]}
                                            onValueChange={(value) => {
                                                const selected = videoBitrateOptions[value[0]];
                                                if (selected !== undefined) handleVideoBitRateChange(selected);
                                            }}
                                            disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>
                                )}

                                {appliedRateControlMode === 'crf' && (renderableSettings.videoCRF ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{tl('sections.video.crfLabel', { crf: videoCRF })}</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={0}
                                            max={videoCRFChoices.length - 1}
                                            step={1}
                                            value={[videoCRFIndex]}
                                            onValueChange={(value) => {
                                                const newCRF = videoCRFChoices[value[0]];
                                                if (newCRF !== undefined) handleVideoCRFChange(newCRF);
                                            }}
                                            disabled={!serverSettings || serverSettings.video_crf?.min === serverSettings.video_crf?.max}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>
                                )}
                            </>
                        )}

                        {/* Paint-over, Turbo and 4:4:4 are pixelflux encoder features shared by both transports. */}
                        {isH264 && (
                            <>
                                {(renderableSettings.videoFullColor ?? true) && (
                                <div className="flex items-center justify-between">
                                    <div className="space-y-0.5">
                                        <label className="text-sm font-medium">{t('sections.video.fullColorLabel')}</label>
                                    </div>
                                    <Switch
                                        checked={videoFullColor}
                                        onCheckedChange={handleH264FullColorToggle}
                                        disabled={!serverSettings || serverSettings.video_fullcolor?.locked}
                                    />
                                </div>
                                )}

                                {(renderableSettings.videoStreamingMode ?? true) && (
                                <div className="flex items-center justify-between">
                                    <div className="space-y-0.5">
                                        <label className="text-sm font-medium" title={t('sections.video.streamingModeDetails')}>{t('sections.video.streamingModeLabel')}</label>
                                    </div>
                                    <Switch
                                        checked={videoStreamingMode}
                                        onCheckedChange={handleH264StreamingModeToggle}
                                        disabled={!serverSettings || serverSettings.video_streaming_mode?.locked}
                                    />
                                </div>
                                )}

                            </>
                        )}

                        {/* Base JPEG quality is independent of paint-over. */}
                        {showJpegOptions && (renderableSettings.jpegQuality ?? true) && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{t('sections.video.jpegQualityLabel', { jpegQuality })}</label>
                                <div className="flex items-center gap-2">
                                    <Slider
                                        min={serverSettings?.jpeg_quality?.min || 1}
                                        max={serverSettings?.jpeg_quality?.max || 100}
                                        step={1}
                                        value={[jpegQuality]}
                                        onValueChange={(value) => handleJpegQualityChange(value[0])}
                                        disabled={!serverSettings || serverSettings.jpeg_quality?.min === serverSettings.jpeg_quality?.max}
                                        className="flex-1"
                                    />
                                </div>
                            </div>
                        )}

                        {/* Server honors paint-over quality for every H.264 encoder and jpeg. */}
                        {(isH264 || activeEncoder === 'jpeg') && (renderableSettings.usePaintOverQuality ?? true) && (
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <label className="text-sm font-medium">{t('sections.video.usePaintOverQualityLabel')}</label>
                                </div>
                                <Switch
                                    checked={usePaintOverQuality}
                                    onCheckedChange={handleUsePaintOverQualityToggle}
                                    disabled={!serverSettings || serverSettings.use_paint_over_quality?.locked}
                                />
                            </div>
                        )}

                        {isH264 && usePaintOverQuality && (
                            <>
                                {(renderableSettings.videoPaintoverCRF ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{tl('sections.video.paintoverCrfLabel', { crf: videoPaintoverCRF })}</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={serverSettings?.video_paintover_crf?.min || 5}
                                            max={serverSettings?.video_paintover_crf?.max || 50}
                                            step={1}
                                            value={[videoPaintoverCRF]}
                                            onValueChange={(value) => handleH264PaintoverCRFChange(value[0])}
                                            disabled={!serverSettings || serverSettings.video_paintover_crf?.min === serverSettings.video_paintover_crf?.max}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>
                                )}
                                {(renderableSettings.videoPaintoverBurstFrames ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">{tl('sections.video.paintoverBurstLabel', { frames: videoPaintoverBurstFrames })}</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={serverSettings?.video_paintover_burst_frames?.min || 1}
                                            max={serverSettings?.video_paintover_burst_frames?.max || 30}
                                            step={1}
                                            value={[videoPaintoverBurstFrames]}
                                            onValueChange={(value) => handleH264PaintoverBurstChange(value[0])}
                                            disabled={!serverSettings || serverSettings.video_paintover_burst_frames?.min === serverSettings.video_paintover_burst_frames?.max}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>
                                )}
                            </>
                        )}

                        {showJpegOptions && usePaintOverQuality && (renderableSettings.paintOverJpegQuality ?? true) && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{t('sections.video.paintOverJpegQualityLabel', { paintOverJpegQuality })}</label>
                                <div className="flex items-center gap-2">
                                    <Slider
                                        min={serverSettings?.paint_over_jpeg_quality?.min || 1}
                                        max={serverSettings?.paint_over_jpeg_quality?.max || 100}
                                        step={1}
                                        value={[paintOverJpegQuality]}
                                        onValueChange={(value) => handlePaintOverJpegQualityChange(value[0])}
                                        disabled={!serverSettings || serverSettings.paint_over_jpeg_quality?.min === serverSettings.paint_over_jpeg_quality?.max}
                                        className="flex-1"
                                    />
                                </div>
                            </div>
                        )}

                        {/* use_cpu only changes behavior for full-frame h264enc (HW vs the server's
                            software encoder); the server forces it true for jpeg/striped in both transports. */}
                        {activeEncoder === 'h264enc' && (renderableSettings.useCpu ?? true) && (
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <label className="text-sm font-medium">{t('sections.video.useCpuLabel')}</label>
                                </div>
                                <Switch
                                    checked={useCpu}
                                    onCheckedChange={handleUseCpuToggle}
                                    disabled={!serverSettings || serverSettings.use_cpu?.locked}
                                />
                            </div>
                        )}
                    </CardContent>
                </TabsContent>
                )}

                {showAudioTab && (
                <TabsContent value="audio">
                    <CardContent className="space-y-4">
                        {(renderableSettings.audioBitrate ?? true) && (
                        <div className="space-y-2">
                            <label className="text-sm font-medium">{tl('sections.audio.bitrateLabel', { bitrate: audioBitRate / 1000 })}</label>
                            <div className="flex items-center gap-2">
                                <Slider
                                    min={0}
                                    max={audioBitrateChoices.length - 1}
                                    step={1}
                                    value={[Math.max(0, audioBitrateChoices.indexOf(audioBitRate))]}
                                    onValueChange={(value) => {
                                        const index = value[0];
                                        const selectedBitrate = audioBitrateChoices[index];
                                        if (selectedBitrate !== undefined) {
                                            setAudioBitRate(selectedBitrate);
                                            localStorage.setItem(getPrefixedKey('audio_bitrate'), selectedBitrate.toString());
                                            debouncedPostSetting({ audio_bitrate: selectedBitrate });
                                        }
                                    }}
                                    className="flex-1"
                                />
                            </div>
                        </div>
                        )}

                        {audioDeviceError && (
                            <div className="text-sm text-red-500">{audioDeviceError}</div>
                        )}

                        <div className="space-y-2">
                            <label className="text-sm font-medium">{tl('sections.audio.inputLabel')}</label>
                            <DropdownMenu>
                                <DropdownMenuTrigger asChild>
                                    <Button variant="outline" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError}>
                                        <span className="truncate">
                                            {audioInputDevices.find(d => d.deviceId === selectedInputDeviceId)?.label || t('audio.defaultDevice')}
                                        </span>
                                        <ChevronUp className="h-4 w-4 rotate-180 flex-shrink-0" />
                                    </Button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent className="w-[280px] max-w-[90vw]">
                                    {audioInputDevices.map(device => (
                                        <DropdownMenuItem
                                            key={device.deviceId}
                                            onClick={() => {
                                                setSelectedInputDeviceId(device.deviceId);
                                                window.postMessage({ type: 'audioDeviceSelected', context: 'input', deviceId: device.deviceId }, window.location.origin);
                                            }}
                                            className="cursor-pointer"
                                        >
                                            <span className="truncate" title={device.label}>
                                                {device.label}
                                            </span>
                                        </DropdownMenuItem>
                                    ))}
                                </DropdownMenuContent>
                            </DropdownMenu>
                        </div>

                        {isOutputSelectionSupported && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">{tl('sections.audio.outputLabel')}</label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger asChild>
                                        <Button variant="outline" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError}>
                                            <span className="truncate">
                                                {audioOutputDevices.find(d => d.deviceId === selectedOutputDeviceId)?.label || t('audio.defaultDevice')}
                                            </span>
                                            <ChevronUp className="h-4 w-4 rotate-180 flex-shrink-0" />
                                        </Button>
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-[280px] max-w-[90vw]">
                                        {audioOutputDevices.map(device => (
                                            <DropdownMenuItem
                                                key={device.deviceId}
                                                onClick={() => {
                                                    setSelectedOutputDeviceId(device.deviceId);
                                                    window.postMessage({ type: 'audioDeviceSelected', context: 'output', deviceId: device.deviceId }, window.location.origin);
                                                }}
                                                className="cursor-pointer"
                                            >
                                                <span className="truncate" title={device.label}>
                                                    {device.label}
                                                </span>
                                            </DropdownMenuItem>
                                        ))}
                                    </DropdownMenuContent>
                                </DropdownMenu>
                            </div>
                        )}

                        {!isOutputSelectionSupported && !isLoadingAudioDevices && !audioDeviceError && (
                            <p className="text-sm text-muted-foreground">{t('sections.audio.outputNotSupported')}</p>
                        )}
                    </CardContent>
                </TabsContent>
                )}
            </Tabs>
        </Card>
    );
}
