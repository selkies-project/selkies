/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The stream settings' state and rules, held once above the layout and read
 * by the video, audio, and screen panels wherever a layout paints them.
 *
 * State arrives through the `message` events the core posts on `window`:
 * `serverSettings` (the server's settings payload, per key a `value`,
 * `allowed`, `min`/`max`, `default`, `locked`, and `overridden`),
 * `effectiveCursorState`, `scalingDpiFollowed` (the UI-scaling default the
 * core re-derived), `displayRefresh` (the display's measured refresh, also
 * `window.displayRefreshRate`, which the frame-rate slider offers as a stop of
 * its own), and `audioDeviceSelected`. Changes go back as
 * `window.postMessage` messages: `settings` (debounced key/value batches the
 * core forwards to the server), `mode`, `setScaleLocally`,
 * `setManualResolution`, `resetResolutionToWindow`, `setAntiAliasing`, and
 * whatever the shared conditional-setting specs post. The transport is seeded
 * from `window.__SELKIES_STREAMING_MODE__`, and a switch goes through the
 * shared `switchStreamMode`.
 *
 * Every value persists under a localStorage key from `getPrefixedKey`, which
 * adds the `_display2` suffix for per-display settings on a secondary display;
 * the cores read the same keys. The cores also persist every value they are
 * told to apply, so a stored key alone cannot tell a user's explicit pick from
 * one the dashboard derived (HiDPI from the resolution mode) or applied from the
 * server (paint-over). Those settings therefore carry an
 * `_explicit_choice` marker beside their value and resolve through the shared
 * specs of `selkies-web-core/lib/conditional-settings.js`, which honor pinned,
 * locked, and operator-overridden server values: a derived write never pins
 * them, and an unmarked stored echo is dropped once the ladder moves on.
 * @module
 */
import { useState, useEffect, useCallback, useMemo, useRef } from "react";
import { displayLabel, canPlayEncoder, decoderSupportReady, canDecodeFullColor, canDecodeTenBit, tenBitFormat,
    codecOfEncoder, codecCarriesFullColor, codecCarriesTenBit, isMacDesktop } from "../../../selkies-web-core/lib/util.js";
import { switchStreamMode } from "../../../selkies-web-core/lib/mode-switch.js";
import { BITRATE_STOPS, CRF_STOPS, FRAMERATE_STOPS, framerateStopIndex, stopIndex, stopsWithin, withDisplayStop } from "../../../selkies-web-core/lib/slider-stops.js";
import { FRAMERATE_DISPLAY, followsDisplay, framerateLabel, matchDisplay } from "../../../selkies-web-core/lib/display-refresh.js";
import { resolveSpec, isSettingPinned, HIDPI_SPEC, RATE_CONTROL_SPEC,
    USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_10BIT_SPEC, VIDEO_STREAMING_MODE_SPEC,
    USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC, FORCE_ALIGNED_RESOLUTION_SPEC, softwareChoiceAvailable,
    tenBitStream, CPU_ENCODE_THREADING_SPEC, cpuEncodeThreadingApplies,
    RAW_POINTER_MOTION_SPEC, MAC_CMD_AS_CTRL_SPEC } from "../../../selkies-web-core/lib/conditional-settings.js";
import { getPrefixedKey, computeRenderableSettings, getLastServerSettings,
    getLastEffectiveCursorState, getLastAudioDevices, isSecondaryDisplay, type ServerSettings } from "@/utils";
import { t } from "@/i18n";

export { displayLabel, canPlayEncoder, framerateLabel };

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
/** The rows a 96 DPI desktop is for, which a manual resolution is read against. */
const DPI_UNITY_ROWS = 1080;

/**
 * The default `scaling_dpi`, derived as the core derives it
 * (lib/stream-density.js): from a manual resolution when one is set, since
 * that framebuffer decides how large the desktop draws its UI and the local
 * screen says nothing about it, and otherwise from the local display scaling
 * (devicePixelRatio), so the remote desktop's fonts and UI match the local
 * environment. A manual resolution is read off its shorter side -- an
 * ultrawide is wide rather than dense -- and either density is snapped to the
 * nearest option and clamped at both ends.
 *
 * The ladder that governs the desktop is an operator override (which the
 * server refuses to let clients clobber), then the stored pick, then this
 * derived default; the cores send stored-else-derived on every connect.
 */
const deriveDpi = (manual: { w: number; h: number }): number => {
    const rows = Math.min(manual.w || 0, manual.h || 0);
    const dpr = window.devicePixelRatio || 1;
    const target = rows > 0 ? 96 * rows / DPI_UNITY_ROWS : Math.round(dpr * 4) * 24;
    return dpiScalingOptions.reduce((prev, curr) =>
        Math.abs(curr.value - target) < Math.abs(prev.value - target) ? curr : prev
    ).value;
};

/**
 * The manual resolution in force, as the cores resolve it: a deployment that
 * forces one and names its size wins, since the core takes the server's over
 * the client's there; otherwise the stored pick, and zeroes for a session
 * sized to its window.
 */
const manualResolution = (serverSettings: ServerSettings | null): { w: number; h: number } => {
    const forced = serverSettings?.manual_resolution?.value === true;
    const server = {
        w: parseInt(serverSettings?.manual_width?.value, 10) || 0,
        h: parseInt(serverSettings?.manual_height?.value, 10) || 0,
    };
    if (forced && server.w > 0 && server.h > 0) return server;
    return {
        w: parseInt(localStorage.getItem(getPrefixedKey('manual_width')) ?? '', 10) || 0,
        h: parseInt(localStorage.getItem(getPrefixedKey('manual_height')) ?? '', 10) || 0,
    };
};

/** The presets the resolution menu offers. */
export const RESOLUTION_PRESETS = [
    "1920x1080", "1280x720", "1366x768", "1920x1200", "2560x1440", "3840x2160",
    "1024x768", "800x600", "640x480", "320x240",
];

const encoderOptions = ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "h264enc-striped", "jpeg"];
/** WebRTC encoders offered until the server payload arrives; its `encoder` allowed list is already filtered. */
const encoderOptionsRTC = ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc"];
/** `webcam_encoder` values; labels come from `displayLabel`. */
export const WEBCAM_ENCODER_OPTIONS = ["auto", "h264", "h265", "vp8", "vp9", "av1", "mjpeg"];
/** Encoders that support both CBR and CRF (constant-QP) rate control. */
const VIDEO_ENCODERS = ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "h264enc-striped"];

const readStored = (key: string) => localStorage.getItem(getPrefixedKey(key));

/**
 * Suffix of the marker an explicit choice writes beside its value; settings
 * that are also derived read storage only through the marker, so a derived
 * write never pins them. The marker key is suffixed onto the already-prefixed
 * value key so it inherits the per-display suffix.
 */
const EXPLICIT_CHOICE_SUFFIX = "_explicit_choice";
const explicitChoiceKey = (spec: any) => `${getPrefixedKey(spec.storageKey)}${EXPLICIT_CHOICE_SUFFIX}`;
const isExplicitChoice = (spec: any) => localStorage.getItem(explicitChoiceKey(spec)) === "true";
const readExplicitStored = (spec: any) => (key: string) => (isExplicitChoice(spec) ? readStored(key) : null);

/**
 * Drives a conditional setting: lazy init, then a re-resolve whenever the
 * server settings or any dependency in `deps` changes (server sync and a
 * dependency's re-derivation alike). The resolver honors explicit choices, so
 * a re-resolve never clobbers a pinned value. Re-resolving writes state rather
 * than deriving during render because the caller edits the value afterwards.
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
/** The transports the mode menu offers. */
export const STREAMING_MODES = [STREAM_MODE_WEBSOCKETS, STREAM_MODE_WEBRTC];

const rateControlOptions = ["cbr", "crf"];
const readHidpiStored = readExplicitStored(HIDPI_SPEC);
const readRateControlStored = readExplicitStored(RATE_CONTROL_SPEC);
const readPaintOverStored = readExplicitStored(USE_PAINT_OVER_QUALITY_SPEC);
const readCpuThreadingStored = readExplicitStored(CPU_ENCODE_THREADING_SPEC);
/** Default `video_bitrate` in kbps, the unit the slider and the wire share. */
const DEFAULT_VIDEO_BITRATE = 8000;

const roundDownToEven = (num: number) => {
    const n = parseInt(num.toString(), 10);
    return isNaN(n) ? 0 : Math.floor(n / 2) * 2;
};

/**
 * Trailing debounce of settings posts: a burst coalesces into one post
 * carrying every setting changed in it, each at its last value, so a derived
 * change never drops the one that caused it.
 */
function settingsPoster(delay: number) {
    let pending: Record<string, unknown> = {};
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    return (setting: Record<string, unknown>) => {
        Object.assign(pending, setting);
        clearTimeout(timeoutId);
        timeoutId = setTimeout(() => {
            const settings = pending;
            pending = {};
            window.postMessage({ type: "settings", settings }, window.location.origin);
        }, delay);
    };
}

/** An audio device as the picker lists it. */
export interface AudioDevice { deviceId: string; label: string }

/**
 * The stream settings' state and handlers. Server settings are seeded from the
 * cached broadcast because the provider may mount after the core connects,
 * and every value stays editable afterwards with localStorage taking
 * precedence. Each conditional setting is one `useConditionalSetting` call
 * over a shared spec: the hook owns init and server sync, and client-driven
 * changes go through `writeConditional`.
 * @returns Everything the video, audio, and screen panels render from.
 */
export function useStreamSettings() {
    const [serverSettings, setServerSettings] = useState<ServerSettings | null>(getLastServerSettings);
    const renderable = useMemo(() => computeRenderableSettings(serverSettings), [serverSettings]);

    const [streamMode, setStreamMode] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("stream_mode"));
        if (saved && STREAMING_MODES.includes(saved)) return saved;
        const runtimeMode = (window as any).__SELKIES_STREAMING_MODE__;
        if (runtimeMode && STREAMING_MODES.includes(runtimeMode)) return runtimeMode;
        return STREAM_MODE_WEBSOCKETS;
    });
    const isWebrtc = streamMode === STREAM_MODE_WEBRTC;

    /**
     * The encoders the menu lists: the static list seeds it and the server's
     * own allowed list replaces it as soon as settings arrive. Every entry is
     * listed; the ones this browser cannot play on the transport are disabled
     * and say so (`canPlayEncoder`).
     */
    const [encoderOptionsShown, setEncoderOptionsShown] = useState<string[]>(isWebrtc ? encoderOptionsRTC : encoderOptions);
    // The decoder probe answers after the first render; the menu is rebuilt
    // from whatever list is current once it has.
    const serverEncoderList: string[] | undefined = serverSettings?.encoder?.allowed;
    useEffect(() => {
        let live = true;
        decoderSupportReady.then(() => {
            if (!live) return;
            setEncoderOptionsShown((serverEncoderList || (isWebrtc ? encoderOptionsRTC : encoderOptions)).slice());
        });
        return () => { live = false; };
    }, [serverEncoderList, isWebrtc]);

    const [manualWidth, setManualWidth] = useState(() => readStored("manual_width") || '');
    const [manualHeight, setManualHeight] = useState(() => readStored("manual_height") || '');
    const [presetValue, setPresetValue] = useState("");
    const [scaleLocally, setScaleLocally] = useState(() => {
        const saved = readStored("scaleLocallyManual");
        return saved !== null ? saved === 'true' : true;
    });
    const [selectedDpi, setSelectedDpi] = useState(() =>
        parseInt(readStored("scaling_dpi") ?? "", 10) || deriveDpi(manualResolution(serverSettings)));
    const [videoBitRate, setVideoBitRate] = useState(() => {
        const parsed = parseInt(readStored("video_bitrate") ?? "", 10);
        return !isNaN(parsed) ? parsed : DEFAULT_VIDEO_BITRATE;
    });
    const [audioBitRate, setAudioBitRate] = useState(() => parseInt(readStored("audio_bitrate") ?? "", 10) || DEFAULT_AUDIO_BITRATE);
    const [encoder, setEncoder] = useState(() => readStored("encoder") || "h264enc");
    const [webcamEncoderChoice, setWebcamEncoderChoice] = useState(() => readStored("webcam_encoder"));
    const [framerate, setFramerate] = useState(() => parseFloat(readStored("framerate") ?? "") || 60);
    /** The stored frame-rate choice: a rate, `FRAMERATE_DISPLAY`, or null for none. */
    const [framerateChoice, setFramerateChoice] = useState<string | null>(() => readStored("framerate"));
    /** The display's refresh the core measured, null until it has. */
    const [displayRate, setDisplayRate] = useState<number | null>(() => (window as any).displayRefreshRate ?? null);
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = readStored("video_crf");
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    /**
     * State the conditional settings read; rebuilt each render so the hooks
     * below re-resolve against current values when their deps change.
     * `encoderBackends` decides whether the software encoding switch is shown.
     */
    const conditionalCtx = {
        manualActive: !!readStored("manual_width") || serverSettings?.manual_resolution?.value === true,
        encoderBackends: serverSettings?.encoder_backends?.value,
        allowedRateControl: serverSettings?.rate_control_mode?.allowed || rateControlOptions,
        macDesktop: isMacDesktop(),
    };
    /**
     * Paint-over also reads the encoder and Turbo, Turbo resolved here rather
     * than taken from its state, which trails the `serverSettings` sync by a render.
     */
    const paintOverCtx = {
        ...conditionalCtx,
        encoder,
        videoStreamingMode: resolveSpec(VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, readStored),
    };
    const debouncedPostSetting = useMemo(() => settingsPoster(500), []);

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
    const writeConditional = (spec: any, uiValue: any, setValue: any, opts: { persist?: boolean } = {}) => {
        setValue(uiValue);
        if (opts.persist) {
            localStorage.setItem(getPrefixedKey(spec.storageKey), spec.serialize ? spec.serialize(uiValue) : String(uiValue));
            localStorage.setItem(explicitChoiceKey(spec), "true");
        }
        spec.propagate(spec.toServer ? spec.toServer(uiValue) : uiValue, conditionalCtx, conditionalIo);
    };

    const [hidpiEnabled, setHidpiEnabled] = useConditionalSetting(HIDPI_SPEC, serverSettings, conditionalCtx, [serverSettings], readHidpiStored);
    const [rateControlMode, setRateControlMode] = useConditionalSetting(RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings], readRateControlStored);
    /**
     * With rate control disabled the server ignores rate_control_mode and
     * keeps the encoder's built-in default, so the dashboard neither pushes a
     * mode nor lets its own pick decide which quality slider is shown.
     */
    const rateControlEnabled = renderable.enableRateControl;
    // Stale-echo rule (module docblock): a stored mode without an explicit
    // pick echoes a value a dashboard derived, and is dropped once it stops
    // matching the ladder, which resolves to the server's own value.
    useEffect(() => {
        if (!serverSettings) return;
        if (serverSettings.enable_rate_control?.value === false) return;
        const rcKey = RATE_CONTROL_SPEC.storageKey;
        const resolved = resolveSpec(RATE_CONTROL_SPEC, serverSettings, conditionalCtx, readRateControlStored);
        if (!isExplicitChoice(RATE_CONTROL_SPEC) && readStored(rcKey) !== null && readStored(rcKey) !== resolved) {
            localStorage.removeItem(getPrefixedKey(rcKey));
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings]);
    // Same stale-echo rule for HiDPI, or a derived pick outlives its resolution
    // mode, and a push of the resolved value: the core starts from its own
    // stored default, so a deployment that configures a resolution would stream
    // pixel-perfect on every load with the toggle reading off.
    useEffect(() => {
        if (!serverSettings) return;
        const key = HIDPI_SPEC.storageKey;
        const resolved = resolveSpec(HIDPI_SPEC, serverSettings, conditionalCtx, readHidpiStored);
        if (!isExplicitChoice(HIDPI_SPEC) && readStored(key) !== null && readStored(key) !== HIDPI_SPEC.serialize(resolved)) {
            localStorage.removeItem(getPrefixedKey(key));
        }
        if (serverSettings.enable_resize?.value === false) return;
        if (isSettingPinned(HIDPI_SPEC, serverSettings, readHidpiStored)) return;
        const serverValue = serverSettings[HIDPI_SPEC.serverKey]?.value;
        if (serverValue !== undefined && HIDPI_SPEC.toServer(resolved) !== serverValue) {
            writeConditional(HIDPI_SPEC, resolved, setHidpiEnabled, { persist: false });
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings]);
    const [videoFullColor, setVideoFullColor] = useConditionalSetting(VIDEO_FULLCOLOR_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // Full color is 4:4:4; where the decoder has no such profile the core
    // turns it off, so offering the switch would offer nothing.
    const [fullColorDecodable, setFullColorDecodable] = useState(true);
    const fullColorCodec = codecOfEncoder(encoder);
    useEffect(() => {
        let live = true;
        canDecodeFullColor(fullColorCodec).then((ok: boolean) => { if (live) setFullColorDecodable(ok); });
        return () => { live = false; };
    }, [fullColorCodec]);
    const [videoStreamingMode, setVideoStreamingMode] = useConditionalSetting(VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // Pre-settings fallbacks mirror the server defaults (settings.py).
    const [jpegQuality, setJpegQuality] = useState(() => parseInt(readStored("jpeg_quality") ?? "", 10) || 40);
    const [paintOverJpegQuality, setPaintOverJpegQuality] = useState(() => parseInt(readStored("paint_over_jpeg_quality") ?? "", 10) || 90);
    const [videoPaintoverCRF, setVideoPaintoverCRF] = useState(() => parseInt(readStored("video_paintover_crf") ?? "", 10) || 18);
    const [videoPaintoverBurstFrames, setVideoPaintoverBurstFrames] = useState(() => parseInt(readStored("video_paintover_burst_frames") ?? "", 10) || 5);
    const [usePaintOverQuality, setUsePaintOverQuality] = useConditionalSetting(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, [serverSettings], readPaintOverStored);
    // Push the resolved paint-over value so the encoder agrees.
    useEffect(() => {
        if (!serverSettings) return;
        const key = USE_PAINT_OVER_QUALITY_SPEC.storageKey;
        const resolved = resolveSpec(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, readPaintOverStored);
        // Same stale-echo rule as rate control.
        if (!isExplicitChoice(USE_PAINT_OVER_QUALITY_SPEC) && readStored(key) !== null && readStored(key) !== String(resolved)) {
            localStorage.removeItem(getPrefixedKey(key));
        }
        if (isSettingPinned(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, readPaintOverStored)) return;
        const serverValue = serverSettings.use_paint_over_quality?.value;
        if (resolved !== undefined && serverValue !== undefined && resolved !== serverValue) {
            writeConditional(USE_PAINT_OVER_QUALITY_SPEC, resolved, setUsePaintOverQuality, { persist: false });
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [serverSettings]);
    // A later encoder or Turbo change re-derives paint-over where nothing pins it.
    useEffect(() => {
        if (!serverSettings || isSettingPinned(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, readPaintOverStored)) return;
        const resolved = resolveSpec(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, readPaintOverStored);
        if (resolved !== usePaintOverQuality) {
            writeConditional(USE_PAINT_OVER_QUALITY_SPEC, resolved, setUsePaintOverQuality, { persist: false });
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [encoder, videoStreamingMode]);
    const [useCpu, setUseCpu] = useConditionalSetting(USE_CPU_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // The server's value is what the display's encoder runs with unless the user chose
    // (single-threaded for VP8): only an explicit pick read back from storage outranks it.
    const [cpuEncodeThreading, setCpuEncodeThreading] = useConditionalSetting(CPU_ENCODE_THREADING_SPEC, serverSettings, conditionalCtx, [serverSettings], readCpuThreadingStored);
    const [video10Bit, setVideo10Bit] = useConditionalSetting(VIDEO_10BIT_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    /** 10-bit is offered only where this engine shows a 10-bit picture of the stream's format. */
    const [tenBitAnswer, setTenBitAnswer] = useState({ format: "", ok: false });
    const tenBitOffered = tenBitStream(encoder, conditionalCtx.encoderBackends, useCpu, videoFullColor);
    const tenBitFullColor = !!tenBitOffered && tenBitOffered.fullcolor;
    const tenBitAsked = tenBitFormat(fullColorCodec, tenBitFullColor);
    useEffect(() => {
        let live = true;
        canDecodeTenBit(fullColorCodec, tenBitFullColor).then((ok: boolean) => {
            if (live) setTenBitAnswer({ format: tenBitFormat(fullColorCodec, tenBitFullColor), ok });
        });
        return () => { live = false; };
    }, [fullColorCodec, tenBitFullColor]);
    const tenBitDecodable = tenBitAnswer.ok && tenBitAnswer.format === tenBitAsked;

    // Anti-aliasing stays client-only (no server truth), so it keeps its own state.
    const [antiAliasing, setAntiAliasing] = useState(() => {
        const saved = readStored("antiAliasingEnabled");
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useConditionalSetting(USE_BROWSER_CURSORS_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [rawPointerMotion, setRawPointerMotion] = useConditionalSetting(RAW_POINTER_MOTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [macCmdAsCtrl, setMacCmdAsCtrl] = useConditionalSetting(MAC_CMD_AS_CTRL_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    /**
     * The cursor mode the core reports as actually in effect (multi-monitor
     * forces browser cursors on), null until reported; the toggle shows it
     * over the stored preference so it never lies about the live state.
     */
    const [effectiveCursor, setEffectiveCursor] = useState<boolean | null>(getLastEffectiveCursorState);
    const [forceAlignedResolution, setForceAlignedResolution] = useConditionalSetting(FORCE_ALIGNED_RESOLUTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);

    const [audioInputDevices, setAudioInputDevices] = useState<AudioDevice[]>([]);
    const [audioOutputDevices, setAudioOutputDevices] = useState<AudioDevice[]>([]);
    const [selectedInputDeviceId, setSelectedInputDeviceId] = useState(() => getLastAudioDevices().input ?? 'default');
    const [selectedOutputDeviceId, setSelectedOutputDeviceId] = useState(() => getLastAudioDevices().output ?? 'default');
    const [isOutputSelectionSupported, setIsOutputSelectionSupported] = useState(false);
    const [audioDeviceError, setAudioDeviceError] = useState<string | null>(null);
    const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);

    useEffect(() => {
        const handleMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message !== "object" || message === null) return;
            if (message.type === "serverSettings") setServerSettings(message.payload);
            if (message.type === "effectiveCursorState" && typeof message.value === "boolean") setEffectiveCursor(message.value);
            if (message.type === "displayRefresh" && Number.isFinite(message.rate)) setDisplayRate(message.rate);
            // The core's derived pick; a stored pick is the user's and stays.
            if (message.type === "scalingDpiFollowed" && typeof message.value === "number" && readStored("scaling_dpi") === null) {
                setSelectedDpi(message.value);
            }
            // Echo of this dashboard's own pick: the picker shows what the core was told.
            if (message.type === "audioDeviceSelected" && message.deviceId) {
                if (message.context === "input") setSelectedInputDeviceId(message.deviceId);
                else if (message.context === "output") setSelectedOutputDeviceId(message.deviceId);
            }
        };
        window.addEventListener("message", handleMessage);
        return () => window.removeEventListener("message", handleMessage);
    }, []);

    // Seeding and re-clamping write state rather than deriving in render:
    // every value stays editable afterwards, so recomputing would discard edits.
    /* eslint-disable react-hooks/set-state-in-effect */
    useEffect(() => {
        if (!serverSettings) return;
        const getStoredInt = (key: string) => parseInt(readStored(key) ?? "", 10);
        const clamped = (setting: any, key: string): number => {
            const stored = getStoredInt(key);
            return !isNaN(stored) ? Math.max(setting.min, Math.min(setting.max, stored)) : setting.default;
        };

        const s_encoder = serverSettings.encoder;
        if (s_encoder) {
            const playable = s_encoder.allowed.filter((enc: string) => canPlayEncoder(enc, isWebrtc));
            const stored = readStored("encoder");
            const final = stored !== null && playable.includes(stored) ? stored
                : (playable.includes(s_encoder.value) || playable.length === 0) ? s_encoder.value : playable[0];
            setEncoder(final);
            setEncoderOptionsShown(s_encoder.allowed);
        }
        if (serverSettings.framerate) {
            const stored = parseFloat(readStored("framerate") ?? "");
            setFramerate(!isNaN(stored)
                ? Math.max(serverSettings.framerate.min, Math.min(serverSettings.framerate.max, stored))
                : serverSettings.framerate.default);
        }
        if (serverSettings.video_bitrate) setVideoBitRate(clamped(serverSettings.video_bitrate, "video_bitrate"));
        const s_audio_bitrate = serverSettings.audio_bitrate;
        if (s_audio_bitrate) {
            const stored = getStoredInt("audio_bitrate");
            // `allowed` holds string bps ("128000"), `stored`/`value` are numbers.
            const final = !isNaN(stored)
                ? (s_audio_bitrate.allowed
                    ? (s_audio_bitrate.allowed.includes(String(stored)) ? stored : parseInt(s_audio_bitrate.value, 10))
                    : Math.max(s_audio_bitrate.min ?? stored, Math.min(s_audio_bitrate.max ?? stored, stored)))
                : parseInt(s_audio_bitrate.value, 10);
            setAudioBitRate(final);
        }
        if (serverSettings.video_crf) setVideoCRF(clamped(serverSettings.video_crf, "video_crf"));
        if (serverSettings.jpeg_quality) setJpegQuality(clamped(serverSettings.jpeg_quality, "jpeg_quality"));
        if (serverSettings.paint_over_jpeg_quality) setPaintOverJpegQuality(clamped(serverSettings.paint_over_jpeg_quality, "paint_over_jpeg_quality"));
        if (serverSettings.video_paintover_crf) setVideoPaintoverCRF(clamped(serverSettings.video_paintover_crf, "video_paintover_crf"));
        if (serverSettings.video_paintover_burst_frames) setVideoPaintoverBurstFrames(clamped(serverSettings.video_paintover_burst_frames, "video_paintover_burst_frames"));

        const s_scaling_dpi = serverSettings.scaling_dpi;
        if (s_scaling_dpi) {
            const stored = getStoredInt("scaling_dpi");
            const storedAllowed = s_scaling_dpi.allowed.includes(String(stored));
            const serverVal = parseInt(s_scaling_dpi.value, 10);
            const derived = deriveDpi(manualResolution(serverSettings));
            const willPostDerived = !storedAllowed && !s_scaling_dpi.overridden && derived !== serverVal;
            setSelectedDpi(s_scaling_dpi.overridden ? serverVal : storedAllowed ? stored : derived);
            if (willPostDerived) debouncedPostSetting({ scaling_dpi: derived });
        }
    }, [serverSettings, streamMode, debouncedPostSetting, isWebrtc]);
    /* eslint-enable react-hooks/set-state-in-effect */

    const audioDevicesRequested = useRef(false);
    /**
     * Populates the audio device lists once. Enumerating labeled devices
     * needs a getUserMedia grant, so this runs only when the audio panel is
     * actually shown: merely opening the settings must not raise a microphone
     * permission prompt.
     *
     * Output selection is probed on the sink the active core plays through,
     * `HTMLMediaElement.setSinkId` for the WebRTC core's video element and
     * `AudioContext.setSinkId` for the WebSocket core, or where that is
     * missing, as in Firefox, the media element that core then plays its
     * context through; probing the wrong one would render a picker that does
     * nothing.
     */
    const ensureAudioDevices = useCallback(() => {
        if (audioDevicesRequested.current) return;
        audioDevicesRequested.current = true;
        (async () => {
            setIsLoadingAudioDevices(true);
            setAudioDeviceError(null);
            setAudioInputDevices([]);
            setAudioOutputDevices([]);
            const supportsSinkId = 'setSinkId' in HTMLMediaElement.prototype
                || (!isWebrtc && typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype);
            setIsOutputSelectionSupported(supportsSinkId);
            try {
                const tempStream = await navigator.mediaDevices.getUserMedia({ audio: true });
                tempStream.getTracks().forEach(track => track.stop());
                const devices = await navigator.mediaDevices.enumerateDevices();
                const inputs: AudioDevice[] = [];
                const outputs: AudioDevice[] = [];
                devices.forEach((device, index) => {
                    if (!device.deviceId) return;
                    const label = device.label || t(device.kind === 'audiooutput'
                        ? 'sections.audio.defaultOutputLabelFallback' : 'sections.audio.defaultInputLabelFallback', { index: index + 1 });
                    if (device.kind === 'audioinput') inputs.push({ deviceId: device.deviceId, label });
                    else if (device.kind === 'audiooutput' && supportsSinkId) outputs.push({ deviceId: device.deviceId, label });
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
        })();
    }, [isWebrtc]);

    const selectAudioDevice = (context: "input" | "output", deviceId: string) => {
        if (context === "input") setSelectedInputDeviceId(deviceId);
        else setSelectedOutputDeviceId(deviceId);
        window.postMessage({ type: 'audioDeviceSelected', context, deviceId }, window.location.origin);
    };

    /** The core persists scaleLocallyManual itself when it applies the message. */
    const toggleScaleLocally = () => {
        const next = !scaleLocally;
        setScaleLocally(next);
        window.postMessage({ type: 'setScaleLocally', value: next }, window.location.origin);
    };

    /** An explicit toggle pins the choice; the core persists useCssScaling when it applies the message. */
    const toggleHidpi = () => writeConditional(HIDPI_SPEC, !hidpiEnabled, setHidpiEnabled, { persist: true });

    const setDpi = (value: string) => {
        const newDpi = parseInt(value, 10);
        setSelectedDpi(newDpi);
        localStorage.setItem(getPrefixedKey('scaling_dpi'), newDpi.toString());
        debouncedPostSetting({ scaling_dpi: newDpi });
    };

    /** Switches the transport (`switchStreamMode`); the core reloads into it. */
    const setStreamModeChoice = async (mode: string) => {
        if (mode === streamMode) return;
        if (await switchStreamMode(mode)) setStreamMode(mode);
    };

    /** Stores a plain setting and posts it to the server. */
    const storeAndPost = (key: string, value: string | number, setValue: (v: any) => void) => {
        setValue(value);
        localStorage.setItem(getPrefixedKey(key), String(value));
        debouncedPostSetting({ [key]: value });
    };

    // Derived, not synced: the server default stands in for a missing choice, locked overrides it.
    const wceServer = serverSettings?.webcam_encoder;
    const wceServerValue = WEBCAM_ENCODER_OPTIONS.includes(wceServer?.value ?? "") ? wceServer.value : null;
    const wceChoice = WEBCAM_ENCODER_OPTIONS.includes(webcamEncoderChoice ?? "") ? webcamEncoderChoice : null;
    const webcamEncoder: string = (wceServer?.locked && wceServerValue) || wceChoice || wceServerValue || "auto";

    /**
     * The slider stops inside the server's ranges; a stored value between stops
     * (a server default, a clamp) shows at the nearest one.
     */
    const videoBitrateChoices: number[] = stopsWithin(BITRATE_STOPS, serverSettings?.video_bitrate?.min ?? 100, serverSettings?.video_bitrate?.max ?? 1000000);
    const framerateSpan = serverSettings?.framerate ? { min: serverSettings.framerate.min, max: serverSettings.framerate.max } : null;
    const displayFramerate: number | null = displayRate ? matchDisplay(displayRate, framerateSpan?.min ?? 8, framerateSpan?.max ?? 240) : null;
    const framerateOptions = withDisplayStop(stopsWithin(FRAMERATE_STOPS, framerateSpan?.min ?? 8, framerateSpan?.max ?? 240), displayFramerate);
    const framerateFollows = followsDisplay(framerateChoice, framerateSpan) && displayFramerate !== null;
    const framerateIndex: number = framerateStopIndex(framerateOptions, framerate, framerateFollows);
    const videoCRFChoices: number[] = stopsWithin(CRF_STOPS, serverSettings?.video_crf?.min ?? 5, serverSettings?.video_crf?.max ?? 50);
    const videoPaintoverCRFChoices: number[] = stopsWithin(CRF_STOPS, serverSettings?.video_paintover_crf?.min ?? 5, serverSettings?.video_paintover_crf?.max ?? 50);
    const audioBitrateChoices: number[] = (serverSettings?.audio_bitrate?.allowed?.map((v: string) => parseInt(v, 10))) || audioBitrateOptions;
    const dpiScalingChoices: { label: string; value: number }[] = (serverSettings?.scaling_dpi?.allowed?.map((v: string) => {
        const value = parseInt(v, 10);
        return { label: `${Math.round((value / 96) * 100)}%`, value };
    })) || dpiScalingOptions;

    /** The display's own stop asks for the display's refresh wherever it moves. */
    const setFramerateIndex = (index: number) => {
        const selected = framerateOptions.stops[index];
        if (selected === undefined) return;
        const choice = index === framerateOptions.display ? FRAMERATE_DISPLAY : String(selected);
        setFramerate(selected);
        setFramerateChoice(choice);
        localStorage.setItem(getPrefixedKey('framerate'), choice);
        debouncedPostSetting({ framerate: choice === FRAMERATE_DISPLAY ? choice : selected });
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
     * A resolution the operator sets carries its own UI-scaling default, since
     * the framebuffer asked for decides how large the desktop draws its UI.
     * Not stored, so it stays a default: a stored pick, or a locked or
     * operator-explicit server value, outranks it as it does at connect.
     */
    const deriveDpiForResolution = () => {
        const s = serverSettings?.scaling_dpi;
        if (s?.locked || s?.overridden) return;
        if (s?.allowed?.includes(String(parseInt(readStored('scaling_dpi') ?? '', 10)))) return;
        const derived = deriveDpi(manualResolution(serverSettings));
        setSelectedDpi(derived);
        debouncedPostSetting({ scaling_dpi: derived });
    };

    /** Applies a manual size: stores it, tells the core, and re-derives HiDPI and the UI scaling. */
    const applyManualResolution = (width: number, height: number) => {
        const evenWidth = roundDownToEven(width);
        const evenHeight = roundDownToEven(height);
        setManualWidth(evenWidth.toString());
        setManualHeight(evenHeight.toString());
        localStorage.setItem(getPrefixedKey('manual_width'), evenWidth.toString());
        localStorage.setItem(getPrefixedKey('manual_height'), evenHeight.toString());
        window.postMessage({ type: 'setManualResolution', width: evenWidth, height: evenHeight }, window.location.origin);
        deriveHidpiForResolution(true);
        deriveDpiForResolution();
    };

    const setManualResolution = () => {
        const width = parseInt(manualWidth.trim(), 10);
        const height = parseInt(manualHeight.trim(), 10);
        if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
            alert(t('alerts.invalidResolution'));
            return;
        }
        setPresetValue("");
        applyManualResolution(width, height);
    };

    const choosePreset = (preset: string) => {
        setPresetValue(preset);
        const [width, height] = preset.split('x').map((part) => parseInt(part, 10));
        if (width > 0 && height > 0) applyManualResolution(width, height);
    };

    /**
     * Restores HiDPI and the UI scaling to their derived defaults on
     * reset-to-window. Unlike the resolution-derived writes, which respect a
     * pinned choice, a reset means "back to defaults", so the client's own pin
     * is dropped even under an operator-explicit value: `use_css_scaling`
     * overridden does not imply locked, and a kept pin would keep outranking
     * the operator's value in the resolution ladder. Only a locked value, or
     * for the scaling an operator override, leaves everything alone.
     */
    const resetResolution = () => {
        setManualWidth('');
        setManualHeight('');
        setPresetValue("");
        localStorage.removeItem(getPrefixedKey('manual_width'));
        localStorage.removeItem(getPrefixedKey('manual_height'));
        window.postMessage({ type: 'resetResolutionToWindow' }, window.location.origin);
        const css = serverSettings?.use_css_scaling;
        if (!css?.locked) {
            localStorage.removeItem(getPrefixedKey(HIDPI_SPEC.storageKey));
            localStorage.removeItem(explicitChoiceKey(HIDPI_SPEC));
            writeConditional(HIDPI_SPEC, css?.overridden ? css.value !== true : true, setHidpiEnabled, { persist: false });
        }
        const dpi = serverSettings?.scaling_dpi;
        if (!(dpi?.locked || dpi?.overridden)) {
            localStorage.removeItem(getPrefixedKey('scaling_dpi'));
            const derived = deriveDpi(manualResolution(serverSettings));
            setSelectedDpi(derived);
            debouncedPostSetting({ scaling_dpi: derived });
        }
    };

    /** Anti-aliasing is client-only; the core persists antiAliasingEnabled itself. */
    const toggleAntiAliasing = () => {
        const next = !antiAliasing;
        setAntiAliasing(next);
        window.postMessage({ type: 'setAntiAliasing', value: next }, window.location.origin);
    };

    const isVideoEncoder = VIDEO_ENCODERS.includes(encoder);
    const showJpegOptions = !isWebrtc && encoder === 'jpeg';
    /**
     * The mode the encoder is actually using, which the quality slider must
     * belong to: with rate control disabled that is the server's mode.
     */
    const appliedRateControlMode: string = rateControlEnabled ? rateControlMode : (serverSettings?.rate_control_mode?.value ?? rateControlMode);
    // Under CBR a video stream's still screen is cleaned up to the paint-over quality whatever the switch says.
    const cbrVideo = isVideoEncoder && appliedRateControlMode === 'cbr';
    const locked = (key: string) => !serverSettings || !!serverSettings[key]?.locked;
    const fixed = (key: string) => !serverSettings || serverSettings[key]?.min === serverSettings[key]?.max;

    const show = {
        streamMode: renderable.enableDualMode || !!(window as any).__SELKIES_DUAL_MODE__,
        encoder: renderable.encoder,
        webcamEncoder: renderable.webcamEncoder && !isWebrtc,
        fullColor: isVideoEncoder && codecCarriesFullColor(codecOfEncoder(encoder)) && renderable.videoFullColor && fullColorDecodable,
        tenBit: isVideoEncoder && codecCarriesTenBit(codecOfEncoder(encoder)) && !!tenBitOffered && renderable.video10Bit && tenBitDecodable,
        framerate: renderable.framerate,
        rateControl: isVideoEncoder && rateControlEnabled,
        bitrate: isVideoEncoder && appliedRateControlMode === 'cbr' && renderable.videoBitrate,
        crf: isVideoEncoder && appliedRateControlMode === 'crf' && renderable.videoCRF,
        turbo: isVideoEncoder && renderable.videoStreamingMode,
        jpegQuality: showJpegOptions && renderable.jpegQuality,
        paintOver: ((isVideoEncoder && !cbrVideo) || encoder === 'jpeg') && renderable.usePaintOverQuality,
        paintoverCrf: isVideoEncoder && (usePaintOverQuality || cbrVideo) && renderable.videoPaintoverCRF,
        paintoverBurst: isVideoEncoder && (usePaintOverQuality || cbrVideo) && renderable.videoPaintoverBurstFrames,
        paintOverJpeg: showJpegOptions && usePaintOverQuality && renderable.paintOverJpegQuality,
        cpu: softwareChoiceAvailable(encoder, conditionalCtx.encoderBackends) && renderable.useCpu,
        cpuThreading: cpuEncodeThreadingApplies(encoder, conditionalCtx.encoderBackends, useCpu, videoFullColor, video10Bit) && renderable.cpuEncodeThreading,
        audioBitrate: renderable.audioBitrate,
        hidpi: !isSecondaryDisplay && renderable.hidpi,
        forceAligned: !isSecondaryDisplay && renderable.forceAlignedResolution,
        uiScaling: !isSecondaryDisplay && renderable.uiScaling,
        useBrowserCursors: renderable.useBrowserCursors,
        rawPointerMotion: renderable.rawPointerMotion,
        macCmdAsCtrl: renderable.macCmdAsCtrl,
        resolutionControls: !serverSettings?.manual_resolution?.locked && (isSecondaryDisplay || serverSettings?.enable_resize?.value !== false),
        videoPanel: renderable.videoSettings,
        audioPanel: renderable.audioSettings,
        screenPanel: renderable.screenSettings,
    };

    return {
        serverSettings,
        renderable,
        isWebrtc,
        show,
        streamMode, setStreamMode: setStreamModeChoice,
        encoder, encoderOptions: encoderOptionsShown,
        setEncoder: (value: string) => storeAndPost('encoder', value, setEncoder),
        webcamEncoder, webcamEncoderLocked: !!wceServer?.locked,
        setWebcamEncoder: (value: string) => storeAndPost('webcam_encoder', value, setWebcamEncoderChoice),
        videoFullColor, toggleFullColor: () => writeConditional(VIDEO_FULLCOLOR_SPEC, !videoFullColor, setVideoFullColor, { persist: true }),
        fullColorLocked: locked('video_fullcolor'),
        video10Bit, toggle10Bit: () => writeConditional(VIDEO_10BIT_SPEC, !video10Bit, setVideo10Bit, { persist: true }),
        tenBitLocked: locked('video_10bit'),
        framerate, framerateFollows, displayFramerate, framerateStops: framerateOptions.stops as number[], framerateIndex, setFramerateIndex,
        framerateFixed: fixed('framerate'),
        rateControlMode, rateControlOptions: (serverSettings?.rate_control_mode?.allowed || rateControlOptions) as string[],
        setRateControlMode: (mode: string) => writeConditional(RATE_CONTROL_SPEC, mode, setRateControlMode, { persist: true }),
        videoBitRate, videoBitrateChoices, bitrateIndex: stopIndex(videoBitrateChoices, videoBitRate) as number,
        setVideoBitRate: (value: number) => storeAndPost('video_bitrate', value, setVideoBitRate), bitrateFixed: fixed('video_bitrate'),
        videoCRF, videoCRFChoices, crfIndex: stopIndex(videoCRFChoices, videoCRF) as number,
        setVideoCRF: (value: number) => storeAndPost('video_crf', value, setVideoCRF), crfFixed: fixed('video_crf'),
        videoStreamingMode, toggleTurbo: () => writeConditional(VIDEO_STREAMING_MODE_SPEC, !videoStreamingMode, setVideoStreamingMode, { persist: true }),
        turboLocked: locked('video_streaming_mode'),
        jpegQuality, setJpegQuality: (value: number) => storeAndPost('jpeg_quality', value, setJpegQuality),
        jpegQualityRange: { min: serverSettings?.jpeg_quality?.min || 1, max: serverSettings?.jpeg_quality?.max || 100 }, jpegQualityFixed: fixed('jpeg_quality'),
        usePaintOverQuality, togglePaintOver: () => writeConditional(USE_PAINT_OVER_QUALITY_SPEC, !usePaintOverQuality, setUsePaintOverQuality, { persist: true }),
        paintOverLocked: locked('use_paint_over_quality'),
        videoPaintoverCRF, videoPaintoverCRFChoices, paintoverCrfIndex: stopIndex(videoPaintoverCRFChoices, videoPaintoverCRF) as number,
        setVideoPaintoverCRF: (value: number) => storeAndPost('video_paintover_crf', value, setVideoPaintoverCRF), paintoverCrfFixed: fixed('video_paintover_crf'),
        videoPaintoverBurstFrames, setVideoPaintoverBurstFrames: (value: number) => storeAndPost('video_paintover_burst_frames', value, setVideoPaintoverBurstFrames),
        paintoverBurstRange: { min: serverSettings?.video_paintover_burst_frames?.min || 1, max: serverSettings?.video_paintover_burst_frames?.max || 30 },
        paintoverBurstFixed: fixed('video_paintover_burst_frames'),
        paintOverJpegQuality, setPaintOverJpegQuality: (value: number) => storeAndPost('paint_over_jpeg_quality', value, setPaintOverJpegQuality),
        paintOverJpegRange: { min: serverSettings?.paint_over_jpeg_quality?.min || 1, max: serverSettings?.paint_over_jpeg_quality?.max || 100 },
        paintOverJpegFixed: fixed('paint_over_jpeg_quality'),
        useCpu, toggleCpu: () => writeConditional(USE_CPU_SPEC, !useCpu, setUseCpu, { persist: true }), cpuLocked: locked('use_cpu'),
        cpuEncodeThreading, toggleCpuThreading: () => writeConditional(CPU_ENCODE_THREADING_SPEC, !cpuEncodeThreading, setCpuEncodeThreading, { persist: true }),
        cpuThreadingLocked: locked('cpu_encode_threading'),
        audioBitRate, audioBitrateChoices,
        setAudioBitRate: (value: number) => storeAndPost('audio_bitrate', value, setAudioBitRate),
        audioInputDevices, audioOutputDevices, selectedInputDeviceId, selectedOutputDeviceId, isOutputSelectionSupported,
        audioDeviceError, isLoadingAudioDevices, ensureAudioDevices, selectAudioDevice,
        antiAliasing, toggleAntiAliasing,
        useBrowserCursors: effectiveCursor !== null ? effectiveCursor : useBrowserCursors,
        /**
         * Propagates the new preference and lets the core, which owns persistence,
         * report the effective (possibly multi-monitor-forced) value back. Derived
         * from the displayed value: while multi-monitor forces the toggle on, the
         * base preference may be off, and negating the base would silently persist
         * the forced value over the user's real choice.
         */
        toggleBrowserCursors: () => writeConditional(USE_BROWSER_CURSORS_SPEC, !(effectiveCursor ?? useBrowserCursors), setUseBrowserCursors, { persist: false }),
        rawPointerMotion, toggleRawPointerMotion: () => writeConditional(RAW_POINTER_MOTION_SPEC, !rawPointerMotion, setRawPointerMotion, { persist: false }),
        macCmdAsCtrl, toggleMacCmdAsCtrl: () => writeConditional(MAC_CMD_AS_CTRL_SPEC, !macCmdAsCtrl, setMacCmdAsCtrl, { persist: false }),
        hidpiEnabled, toggleHidpi,
        hidpiDisabledReason: serverSettings?.enable_resize?.value === false ? 'sections.screen.hidpiDisabledNoResizeTitle'
            : conditionalCtx.manualActive ? 'sections.screen.hidpiDisabledManualTitle' : null,
        forceAlignedResolution, toggleForceAligned: () => writeConditional(FORCE_ALIGNED_RESOLUTION_SPEC, !forceAlignedResolution, setForceAlignedResolution, { persist: true }),
        selectedDpi, dpiScalingChoices, setDpi,
        /** A single allowed stop, or an operator-set DPI, leaves nothing to change. */
        dpiDisabled: !serverSettings || serverSettings.scaling_dpi?.allowed?.length <= 1 || serverSettings.scaling_dpi?.overridden === true,
        manualWidth, manualHeight, presetValue,
        setManualWidth: (value: string) => { setManualWidth(value); setPresetValue(""); },
        setManualHeight: (value: string) => { setManualHeight(value); setPresetValue(""); },
        setManualResolution, choosePreset, resetResolution,
        scaleLocally, toggleScaleLocally,
    };
}

/** What `useStreamSettings` returns. */
export type StreamSettings = ReturnType<typeof useStreamSettings>;
