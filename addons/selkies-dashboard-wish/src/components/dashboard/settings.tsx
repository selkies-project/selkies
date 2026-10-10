/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Settings panel of the wish dashboard: Video, Audio, and Resolution tabs
 * over the streaming core.
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
 *
 * Each tab is an accordion of cards (Video: Format, Performance; Audio:
 * Quality, Devices; Resolution: Display, Resolution). Opening one card closes
 * the others in its tab, the first visible card starts open, and a card none
 * of whose controls is visible is not rendered.
 * @module
 */

import { Card, CardContent } from "@/components/ui/card";
import { displayLabel, canPlayEncoder, decoderSupportReady, canDecodeFullColor, canDecodeTenBit, tenBitFormat, codecOfEncoder, codecCarriesFullColor, codecCarriesTenBit, isMacDesktop } from "../../../../selkies-web-core/lib/util.js";
import { switchStreamMode } from "../../../../selkies-web-core/lib/mode-switch.js";
import { BITRATE_STOPS, CRF_STOPS, FRAMERATE_STOPS, framerateStopIndex, stopIndex, stopsWithin, withDisplayStop } from "../../../../selkies-web-core/lib/slider-stops.js";
import { FRAMERATE_DISPLAY, followsDisplay, framerateLabel, matchDisplay } from "../../../../selkies-web-core/lib/display-refresh.js";
import { resolveSpec, isSettingPinned, HIDPI_SPEC, RATE_CONTROL_SPEC,
    USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_10BIT_SPEC, VIDEO_STREAMING_MODE_SPEC,
    USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC, FORCE_ALIGNED_RESOLUTION_SPEC, softwareChoiceAvailable,
    tenBitStream,
    RAW_POINTER_MOTION_SPEC, MAC_CMD_AS_CTRL_SPEC } from "../../../../selkies-web-core/lib/conditional-settings.js";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Slider } from "@/components/ui/slider";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { ChevronDown } from "lucide-react";
import React, { useState, useEffect, useCallback, useMemo } from "react";
import { SectionAccordion, SectionItem } from "@/components/dashboard/section-accordion";
import { getPrefixedKey, computeRenderableSettings, getLastServerSettings,
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
const manualResolution = (serverSettings: any): { w: number; h: number } => {
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
    "h265enc",
    "vp8enc",
    "vp9enc",
    "av1enc",
    "h264enc-striped",
    "jpeg",
];

/**
 * WebRTC encoders offered until the server payload arrives; its `encoder`
 * allowed list is already filtered to what the webrtc pipeline produces.
 */
/** `webcam_encoder` values; labels come from `displayLabel`. */
const webcamEncoderOptions = ["auto", "h264", "h265", "vp8", "vp9", "av1", "mjpeg"];

const encoderOptionsRTC = [
    "h264enc",
    "h265enc",
    "vp8enc",
    "vp9enc",
    "av1enc",
];

/** Encoders that support both CBR and CRF (constant-QP) rate control. */
const VIDEO_ENCODERS = ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "h264enc-striped"];

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
 * server settings or any dependency in `deps` changes (server sync and a
 * dependency's re-derivation alike). The resolver honors
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

/**
 * The Settings panel: Video, Audio, and Resolution tabs, each hidden when the
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
     * The encoders the menu lists: the static list seeds it and the server's
     * own allowed list replaces it as soon as settings arrive. Every entry is
     * listed; the ones this browser cannot play on the transport are disabled
     * and say so (`canPlayEncoder`).
     */
    const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState<string[]>(
        isWebrtc ? encoderOptionsRTC : encoderOptions
    );
    // The decoder probe answers after the first render; the menu is rebuilt from
    // whatever list is current once it has.
    const serverEncoderList = serverSettings?.encoder?.allowed;
    useEffect(() => {
        let live = true;
        decoderSupportReady.then(() => {
            if (!live) return;
            setDynamicEncoderOptions((serverEncoderList || (isWebrtc ? encoderOptionsRTC : encoderOptions)).slice());
        });
        return () => { live = false; };
    }, [serverEncoderList, isWebrtc]);

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
        return parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")) ?? "", 10) || deriveDpi(manualResolution(serverSettings));
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
    const [webcamEncoderChoice, setWebcamEncoderChoice] = useState(() =>
        localStorage.getItem(getPrefixedKey("webcam_encoder"))
    );
    const [framerate, setFramerate] = useState(() =>
        parseFloat(localStorage.getItem(getPrefixedKey("framerate")) ?? "") || 60
    );
    /** The stored frame-rate choice: a rate, `FRAMERATE_DISPLAY`, or null for none. */
    const [framerateChoice, setFramerateChoice] = useState<string | null>(() =>
        localStorage.getItem(getPrefixedKey("framerate"))
    );
    /** The display's refresh the core measured, null until it has. */
    const [displayRate, setDisplayRate] = useState<number | null>(() =>
        (window as any).displayRefreshRate ?? null
    );
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_crf"));
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
     * than taken from its state, which trails the `serverSettings` sync by a
     * render.
     */
    const paintOverCtx = {
        ...conditionalCtx,
        encoder,
        videoStreamingMode: resolveSpec(VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, readStored),
    };
    const DEBOUNCE_DELAY = 500;
    const debouncedPostSetting = useMemo(() => settingsPoster(DEBOUNCE_DELAY), []);

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
        RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings], readRateControlStored);
    /**
     * With rate control disabled the server ignores rate_control_mode and
     * keeps the encoder's built-in default, so the dashboard neither pushes a
     * mode nor lets its own pick decide which quality slider is shown.
     */
    const rateControlEnabled = renderableSettings.enableRateControl ?? true;
    // Stale-echo rule (module docblock): a stored mode without an explicit
    // pick echoes a value a dashboard derived, and is dropped once it stops
    // matching the ladder, which resolves to the server's own value; kept, it
    // would hold the session to it and outlive an operator override.
    useEffect(() => {
        if (!serverSettings) return;
        if (serverSettings.enable_rate_control?.value === false) return;
        const rcKey = RATE_CONTROL_SPEC.storageKey;
        const resolved = resolveSpec(
            RATE_CONTROL_SPEC, serverSettings, conditionalCtx, readRateControlStored);
        if (!isExplicitChoice(RATE_CONTROL_SPEC)
            && readStored(rcKey) !== null && readStored(rcKey) !== resolved) {
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
        const resolved = resolveSpec(
            HIDPI_SPEC, serverSettings, conditionalCtx, readHidpiStored);
        if (!isExplicitChoice(HIDPI_SPEC)
            && readStored(key) !== null
            && readStored(key) !== HIDPI_SPEC.serialize(resolved)) {
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
    const [videoFullColor, setVideoFullColor] = useConditionalSetting(
        VIDEO_FULLCOLOR_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // Full color is 4:4:4 H.264; where the decoder has no such profile the core
    // turns it off, so offering the switch would offer nothing.
    const [fullColorDecodable, setFullColorDecodable] = useState(true);
    /** Full color is offered only where the codec carries it and this engine decodes it. */
    const fullColorCodec = codecOfEncoder(encoder);
    useEffect(() => {
        let live = true;
        canDecodeFullColor(fullColorCodec).then((ok) => { if (live) setFullColorDecodable(ok); });
        return () => { live = false; };
    }, [fullColorCodec]);
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
    const [usePaintOverQuality, setUsePaintOverQuality] = useConditionalSetting(
        USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, [serverSettings], readPaintOverStored);
    // Push the resolved paint-over value so the encoder agrees.
    useEffect(() => {
        if (!serverSettings) return;
        const key = USE_PAINT_OVER_QUALITY_SPEC.storageKey;
        const resolved = resolveSpec(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, readPaintOverStored);
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
    const [useCpu, setUseCpu] = useConditionalSetting(
        USE_CPU_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [video10Bit, setVideo10Bit] = useConditionalSetting(
        VIDEO_10BIT_SPEC, serverSettings, conditionalCtx, [serverSettings]);
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
        const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useConditionalSetting(
        USE_BROWSER_CURSORS_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [rawPointerMotion, setRawPointerMotion] = useConditionalSetting(
        RAW_POINTER_MOTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [macCmdAsCtrl, setMacCmdAsCtrl] = useConditionalSetting(
        MAC_CMD_AS_CTRL_SPEC, serverSettings, conditionalCtx, [serverSettings]);
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
            if (event.data?.type === "displayRefresh" && Number.isFinite(event.data.rate)) {
                setDisplayRate(event.data.rate);
            }
            // The core's derived pick; a stored pick is the user's and stays.
            if (event.data?.type === "scalingDpiFollowed" && typeof event.data.value === "number"
                    && localStorage.getItem(getPrefixedKey("scaling_dpi")) === null) {
                setSelectedDpi(event.data.value);
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
            const playable = s_encoder.allowed.filter((enc: string) => canPlayEncoder(enc, isWebrtc));
            const stored = localStorage.getItem(getPrefixedKey("encoder"));
            const final = stored !== null && playable.includes(stored) ? stored
                : (playable.includes(s_encoder.value) || playable.length === 0) ? s_encoder.value : playable[0];
            setEncoder(final);
            setDynamicEncoderOptions(s_encoder.allowed);
        }

        const s_framerate = serverSettings.framerate;
        if (s_framerate) {
            const stored = parseFloat(localStorage.getItem(getPrefixedKey("framerate")) ?? "");
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
            const derived = deriveDpi(manualResolution(serverSettings));
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
    }, [serverSettings, streamMode, debouncedPostSetting, isWebrtc]);
    /* eslint-enable react-hooks/set-state-in-effect */

    const audioDevicesRequested = React.useRef(false);
    /**
     * Populates the audio device lists once. Enumerating labeled devices
     * needs a getUserMedia grant, so this runs only when the Audio tab is
     * actually shown: merely opening Settings must not raise a microphone
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
        const populateAudioDevices = async () => {
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
     * and UI-scaling derivations read, so only Set, a preset, and Reset write them.
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

    /** Switches the transport (`switchStreamMode`); the core reloads into it. */
    const handleStreamModeChange = async (mode: string) => {
        if (mode === streamMode) return;
        if (await switchStreamMode(mode)) setStreamMode(mode);
    };

    const handleEncoderChange = (selectedEncoder: string) => {
        setEncoder(selectedEncoder);
        localStorage.setItem(getPrefixedKey('encoder'), selectedEncoder);
        debouncedPostSetting({ encoder: selectedEncoder });
    };

    const handleWebcamEncoderChange = (preference: string) => {
        setWebcamEncoderChoice(preference);
        localStorage.setItem(getPrefixedKey("webcam_encoder"), preference);
        debouncedPostSetting({ webcam_encoder: preference });
    };
    // Derived, not synced: the server default stands in for a missing choice,
    // locked overrides it.
    const wceServer = serverSettings?.webcam_encoder;
    const wceServerValue = webcamEncoderOptions.includes(wceServer?.value ?? "") ? wceServer.value : null;
    const wceChoice = webcamEncoderOptions.includes(webcamEncoderChoice ?? "") ? webcamEncoderChoice : null;
    const webcamEncoder = (wceServer?.locked && wceServerValue) || wceChoice || wceServerValue || "auto";

    /** The display's own stop asks for the display's refresh wherever it moves. */
    const handleFramerateChange = (index: number) => {
        const selectedFramerate = framerateOptions.stops[index];
        if (selectedFramerate === undefined) return;
        const choice = index === framerateOptions.display ? FRAMERATE_DISPLAY : String(selectedFramerate);
        setFramerate(selectedFramerate);
        setFramerateChoice(choice);
        localStorage.setItem(getPrefixedKey('framerate'), choice);
        debouncedPostSetting({ framerate: choice === FRAMERATE_DISPLAY ? choice : selectedFramerate });
    };

    const handleVideoCRFChange = (selectedCRF: number) => {
        setVideoCRF(selectedCRF);
        localStorage.setItem(getPrefixedKey('video_crf'), selectedCRF.toString());
        debouncedPostSetting({ video_crf: selectedCRF });
    };

    /** An explicit choice is persisted, which pins it over the server's default. */
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

    const handle10BitToggle = () => {
        writeConditional(VIDEO_10BIT_SPEC, !video10Bit, setVideo10Bit, { persist: true });
    };

    const handleH264StreamingModeToggle = () => {
        writeConditional(VIDEO_STREAMING_MODE_SPEC, !videoStreamingMode, setVideoStreamingMode, { persist: true });
    };

    const handleUsePaintOverQualityToggle = () => {
        writeConditional(USE_PAINT_OVER_QUALITY_SPEC, !usePaintOverQuality, setUsePaintOverQuality, { persist: true });
    };

    const handleUseCpuToggle = () => {
        writeConditional(USE_CPU_SPEC, !useCpu, setUseCpu, { persist: true });
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

    /** Raw pointer motion toggle; the core owns persistence, as for browser cursors. */
    const handleRawPointerMotionToggle = () => {
        writeConditional(RAW_POINTER_MOTION_SPEC, !rawPointerMotion, setRawPointerMotion, { persist: false });
    };
    const handleMacCmdAsCtrlToggle = () => {
        writeConditional(MAC_CMD_AS_CTRL_SPEC, !macCmdAsCtrl, setMacCmdAsCtrl, { persist: false });
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
     * Returns UI scaling to its derived default on reset-to-window, which with
     * no resolution of its own left to read is the local display's scaling:
     * the pinned client choice is dropped and the derived value propagates
     * like a user change. A locked or operator-overridden value governs
     * scaling instead, the same gate as the startup derived-default post, so
     * nothing happens then.
     */
    const resetDpiToDerivedDefault = () => {
        const s = serverSettings?.scaling_dpi;
        if (s?.locked || s?.overridden) return;
        localStorage.removeItem(getPrefixedKey('scaling_dpi'));
        const derived = deriveDpi(manualResolution(serverSettings));
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
        deriveDpiForResolution();
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

    /**
     * The slider stops inside the server's ranges; a stored value between stops
     * (a server default, a clamp) shows at the nearest one.
     */
    const videoBitrateOptions = stopsWithin(BITRATE_STOPS, serverSettings?.video_bitrate?.min ?? 100, serverSettings?.video_bitrate?.max ?? 1000000);
    const bitrateIndex = stopIndex(videoBitrateOptions, videoBitRate);
    const framerateSpan = serverSettings?.framerate
        ? { min: serverSettings.framerate.min, max: serverSettings.framerate.max }
        : null;
    const displayFramerate = displayRate ? matchDisplay(displayRate, framerateSpan?.min ?? 8, framerateSpan?.max ?? 240) : null;
    const framerateOptions = withDisplayStop(stopsWithin(FRAMERATE_STOPS, framerateSpan?.min ?? 8, framerateSpan?.max ?? 240), displayFramerate);
    const framerateFollows = followsDisplay(framerateChoice, framerateSpan) && displayFramerate !== null;
    const framerateIndex = framerateStopIndex(framerateOptions, framerate, framerateFollows);
    const videoCRFChoices = stopsWithin(CRF_STOPS, serverSettings?.video_crf?.min ?? 5, serverSettings?.video_crf?.max ?? 50);
    const videoCRFIndex = stopIndex(videoCRFChoices, videoCRF);
    const videoPaintoverCRFChoices = stopsWithin(CRF_STOPS, serverSettings?.video_paintover_crf?.min ?? 5, serverSettings?.video_paintover_crf?.max ?? 50);
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
    const isH264 = VIDEO_ENCODERS.includes(activeEncoder);
    const showFullColor = isH264 && codecCarriesFullColor(codecOfEncoder(activeEncoder));
    const show10Bit = isH264 && codecCarriesTenBit(codecOfEncoder(activeEncoder))
        && !!tenBitOffered;
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

    const showStreamMode = !!(renderableSettings.enableDualMode ?? (window as any).__SELKIES_DUAL_MODE__ ?? false);
    const showFullColorSwitch = isH264 && showFullColor && (renderableSettings.videoFullColor ?? true) && fullColorDecodable;
    const showTenBitSwitch = isH264 && show10Bit && (renderableSettings.video10Bit ?? true) && tenBitDecodable;
    const showFramerate = renderableSettings.framerate ?? true;
    const showRateControlSelect = isH264 && showRateControl;
    const showBitrateSlider = isH264 && appliedRateControlMode === 'cbr' && (renderableSettings.videoBitrate ?? true);
    const showCrfSlider = isH264 && appliedRateControlMode === 'crf' && (renderableSettings.videoCRF ?? true);
    const showTurbo = isH264 && (renderableSettings.videoStreamingMode ?? true);
    const showJpegQualitySlider = showJpegOptions && (renderableSettings.jpegQuality ?? true);
    const showPaintOverSwitch = (isH264 || activeEncoder === 'jpeg') && (renderableSettings.usePaintOverQuality ?? true);
    const showPaintoverCrf = isH264 && usePaintOverQuality && (renderableSettings.videoPaintoverCRF ?? true);
    const showPaintoverBurst = isH264 && usePaintOverQuality && (renderableSettings.videoPaintoverBurstFrames ?? true);
    const showPaintOverJpeg = showJpegOptions && usePaintOverQuality && (renderableSettings.paintOverJpegQuality ?? true);
    const showCpuSwitch = softwareChoiceAvailable(activeEncoder, conditionalCtx.encoderBackends) && (renderableSettings.useCpu ?? true);
    const showAudioBitrate = renderableSettings.audioBitrate ?? true;
    const showHidpi = !isSecondaryDisplay && (renderableSettings.hidpi ?? true);
    const showForceAligned = !isSecondaryDisplay && (renderableSettings.forceAlignedResolution ?? true);
    const showUiScaling = !isSecondaryDisplay && (renderableSettings.uiScaling ?? true);
    const showResolutionControls = !serverSettings?.manual_resolution?.locked
        && (isSecondaryDisplay || serverSettings?.enable_resize?.value !== false);

    const showFormatCard = showStreamMode || encoderRenderable || webcamEncoderRenderable
        || showFullColorSwitch || showTenBitSwitch;
    const showPerformanceCard = showFramerate || showRateControlSelect || showBitrateSlider || showCrfSlider
        || showTurbo || showJpegQualitySlider || showPaintOverSwitch || showPaintoverCrf
        || showPaintoverBurst || showPaintOverJpeg || showCpuSwitch;
    const showQualityCard = showAudioBitrate;

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
        <Card className="w-[264px] gap-1 py-1">
            <Tabs
                defaultValue={defaultTab}
                onValueChange={(value) => { if (value === "audio") ensureAudioDevices(); }}
                className="w-full px-1"
            >
                <TabsList className="w-full">
                    {showVideoTab && <TabsTrigger value="video">{t('settingsTabs.video')}</TabsTrigger>}
                    {showAudioTab && <TabsTrigger value="audio">{t('settingsTabs.audio')}</TabsTrigger>}
                    {showResolutionTab && <TabsTrigger value="resolution">{t('settingsTabs.resolution')}</TabsTrigger>}
                </TabsList>

                {showResolutionTab && (
                <TabsContent value="resolution">
                    <CardContent className="px-0">

                        <SectionAccordion defaultValue={["display"]}>
                        <SectionItem value="display" title={t('settingsSections.display')}>
                            {/* Per-display capable settings (the core routes them with a
                                _display2 suffix): available on secondary displays too. */}
                            <div className="flex items-center justify-between">
                                <Label>{t('sections.screen.antiAliasingLabel')}</Label>
                                <Switch size="sm"
                                    checked={antiAliasing}
                                    onCheckedChange={handleAntiAliasingToggle}
                                />
                            </div>

                            {(renderableSettings.useBrowserCursors ?? true) && (
                                <div className="flex items-center justify-between">
                                    <Label>{t('sections.screen.useNativeCursorStylesLabel')}</Label>
                                    <Switch size="sm"
                                        checked={effectiveCursor !== null ? effectiveCursor : useBrowserCursors}
                                        onCheckedChange={handleUseBrowserCursorsToggle}
                                    />
                                </div>
                            )}

                            {(renderableSettings.rawPointerMotion ?? true) && (
                                <div className="flex items-center justify-between">
                                    <Label
                                            title={t(rawPointerMotion
                                                ? 'sections.screen.rawPointerMotionDisableTitle'
                                                : 'sections.screen.rawPointerMotionEnableTitle')}>
                                            {t('sections.screen.rawPointerMotionLabel')}
                                        </Label>
                                    <Switch size="sm"
                                        checked={rawPointerMotion}
                                        onCheckedChange={handleRawPointerMotionToggle}
                                    />
                                </div>
                            )}

                            {(renderableSettings.macCmdAsCtrl ?? false) && (
                                <div className="flex items-center justify-between">
                                    <Label
                                            title={t(macCmdAsCtrl
                                                ? 'sections.screen.macCmdAsCtrlDisableTitle'
                                                : 'sections.screen.macCmdAsCtrlEnableTitle')}>
                                            {t('sections.screen.macCmdAsCtrlLabel')}
                                        </Label>
                                    <Switch size="sm"
                                        checked={macCmdAsCtrl}
                                        onCheckedChange={handleMacCmdAsCtrlToggle}
                                    />
                                </div>
                            )}

                            {showHidpi && (
                                <div className="flex items-center justify-between">
                                    <Label
                                            title={serverSettings?.enable_resize?.value === false
                                                ? t('sections.screen.hidpiDisabledNoResizeTitle')
                                                : conditionalCtx.manualActive
                                                ? t('sections.screen.hidpiDisabledManualTitle')
                                                : undefined}>{t('sections.screen.hidpiLabel')}</Label>
                                    <Switch size="sm"
                                        checked={hidpiEnabled}
                                        onCheckedChange={handleHidpiToggle}
                                        disabled={serverSettings?.enable_resize?.value === false
                                            || conditionalCtx.manualActive}
                                    />
                                </div>
                            )}

                            {showForceAligned && (
                                <div className="flex items-center justify-between">
                                    <Label title={t('sections.screen.forceAlignedResolutionDetails')}>{t('sections.screen.forceAlignedResolutionLabel')}</Label>
                                    <Switch size="sm"
                                        checked={forceAlignedResolution}
                                        onCheckedChange={handleForceAlignedResolutionToggle}
                                    />
                                </div>
                            )}

                            {showUiScaling && (
                                <div className="space-y-2">
                                    <Label>{t('sections.screen.uiScalingLabel')}</Label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger
                                            render={<Button variant="outline" size="xs" className="w-full justify-between" disabled={dpiScalingDisabled} />}
                                        >
                                            {dpiScalingChoices.find(option => option.value === selectedDpi)?.label || "100%"}
                                            <ChevronDown />
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent>
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
                        </SectionItem>

                        <SectionItem value="resolution" title={t('settingsSections.resolution')}>
                            {showResolutionControls && (
                                <>
                                    <div className="space-y-2">
                                        <Label>{tl('sections.screen.presetLabel')}</Label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger
                                                render={<Button variant="outline" size="xs" className="w-full justify-between" />}
                                            >
                                                {presetValue || t('sections.screen.resolutionPresetSelect')}
                                                <ChevronDown />
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent>
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
                                                                    deriveDpiForResolution();
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
                                            <Label>{tl('sections.screen.widthLabel')}</Label>
                                            <Input
                                                type="number"
                                                value={manualWidth}
                                                onChange={handleManualWidthChange}
                                                placeholder={t('sections.screen.widthPlaceholder')}
                                                min="1"
                                                step="2"
                                            />
                                        </div>
                                        <div className="flex-1 space-y-2">
                                            <Label>{tl('sections.screen.heightLabel')}</Label>
                                            <Input
                                                type="number"
                                                value={manualHeight}
                                                onChange={handleManualHeightChange}
                                                placeholder={t('sections.screen.heightPlaceholder')}
                                                min="1"
                                                step="2"
                                            />
                                        </div>
                                    </div>

                                    <div className="flex gap-2">
                                        <Button
                                            variant="outline"
                                            size="xs"
                                            className="flex-1"
                                            onClick={handleSetManualResolution}
                                        >
                                            {t('screen.setButton')}
                                        </Button>
                                        <Button
                                            variant="outline"
                                            size="xs"
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
                                size="xs"
                                className="w-full"
                                onClick={handleScaleLocallyToggle}
                            >
                                {tl('sections.screen.scaleLocallyLabel')}: {t(scaleLocally ? 'sections.screen.scaleLocallyOn' : 'sections.screen.scaleLocallyOff')}
                            </Button>
                        </SectionItem>
                        </SectionAccordion>
                    </CardContent>
                </TabsContent>
                )}

                {showVideoTab && (
                <TabsContent value="video">
                    <CardContent className="px-0">

                        <SectionAccordion defaultValue={[showFormatCard ? "format" : "performance"]}>
                        {showFormatCard && (
                            <SectionItem value="format" title={t('settingsSections.format')}>
                                {showStreamMode && (
                                    <div className="space-y-2">
                                        <Label>{t('streamingModeTitle')}</Label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger
                                                render={<Button variant="outline" size="xs" className="w-full justify-between" />}
                                            >
                                                {displayLabel(streamMode)}
                                                <ChevronDown />
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent>
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
                                        <Label>{tl('sections.video.encoderLabel')}</Label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger
                                                render={<Button variant="outline" size="xs" className="w-full justify-between" />}
                                            >
                                                {displayLabel(activeEncoder)}
                                                <ChevronDown />
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent>
                                                {dynamicEncoderOptions.map(enc => (
                                                    <DropdownMenuItem
                                                        key={enc}
                                                        disabled={!canPlayEncoder(enc, isWebrtc)}
                                                        onClick={() => handleEncoderChange(enc)}
                                                    >
                                                        {displayLabel(enc)}
                                                        {!canPlayEncoder(enc, isWebrtc) && (
                                                            <span className="ml-auto pl-3 text-sm text-muted-foreground">
                                                                {tl('sections.video.encoderUnsupported')}
                                                            </span>
                                                        )}
                                                    </DropdownMenuItem>
                                                ))}
                                            </DropdownMenuContent>
                                        </DropdownMenu>
                                    </div>
                                )}

                                {webcamEncoderRenderable && (
                                    <div className="space-y-2">
                                        <Label>{tl('sections.video.webcamEncoderLabel')}</Label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger
                                                render={
                                                    <Button
                                                        variant="outline"
                                                        size="xs" className="w-full justify-between"
                                                        disabled={!!serverSettings?.webcam_encoder?.locked}
                                                    />
                                                }
                                            >
                                                {displayLabel(webcamEncoder)}
                                                <ChevronDown />
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent>
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

                                {/* Paint-over, Turbo, and 4:4:4 are pixelflux encoder features shared by both transports. */}
                                {showFullColorSwitch && (
                                    <div className="flex items-center justify-between">
                                        <Label>{t('sections.video.fullColorLabel')}</Label>
                                        <Switch size="sm"
                                            checked={videoFullColor}
                                            onCheckedChange={handleH264FullColorToggle}
                                            disabled={!serverSettings || serverSettings.video_fullcolor?.locked}
                                        />
                                    </div>
                                )}

                                {showTenBitSwitch && (
                                    <div className="flex items-center justify-between">
                                        <Label>{t('sections.video.tenBitLabel')}</Label>
                                        <Switch size="sm"
                                            checked={video10Bit}
                                            onCheckedChange={handle10BitToggle}
                                            disabled={!serverSettings || serverSettings.video_10bit?.locked}
                                        />
                                    </div>
                                )}
                            </SectionItem>
                        )}

                        {showPerformanceCard && (
                            <SectionItem value="performance" title={t('settingsSections.performance')}>
                                {showFramerate && (
                                    <div className="space-y-2">
                                        <Label>
                                            {tl(framerateFollows ? 'sections.video.framerateDisplayLabel' : 'sections.video.framerateLabel',
                                                { framerate: framerateLabel(framerateFollows && displayFramerate !== null ? displayFramerate : framerate) })}
                                        </Label>
                                        <Slider
                                            min={0}
                                            max={framerateOptions.stops.length - 1}
                                            step={1}
                                            value={[framerateIndex]}
                                            onValueChange={(value) => handleFramerateChange(Array.isArray(value) ? value[0] : value)}
                                        />
                                    </div>
                                )}

                                {showRateControlSelect && (
                                    <div className="space-y-2">
                                        <Label>{t('sections.video.rateControlLabel')}</Label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger
                                                render={<Button variant="outline" size="xs" className="w-full justify-between" />}
                                            >
                                                {displayLabel(rateControlMode)}
                                                <ChevronDown />
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent>
                                                {(serverSettings?.rate_control_mode?.allowed || rateControlOptions).map((mode: string) => (
                                                    <DropdownMenuItem key={mode} onClick={() => handleRateControlChange(mode)}>
                                                        {displayLabel(mode)}
                                                    </DropdownMenuItem>
                                                ))}
                                            </DropdownMenuContent>
                                        </DropdownMenu>
                                    </div>
                                )}

                                {showBitrateSlider && (
                                    <div className="space-y-2">
                                        <Label>{tl('sections.video.bitrateLabel', { bitrate: formatBitrate(videoBitRate) })}</Label>
                                        <Slider
                                            min={0}
                                            max={videoBitrateOptions.length - 1}
                                            step={1}
                                            value={[bitrateIndex]}
                                            onValueChange={(value) => {
                                                const selected = videoBitrateOptions[Array.isArray(value) ? value[0] : value];
                                                if (selected !== undefined) handleVideoBitRateChange(selected);
                                            }}
                                            disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}
                                        />
                                    </div>
                                )}

                                {showCrfSlider && (
                                    <div className="space-y-2">
                                        <Label>{tl('sections.video.crfLabel', { crf: videoCRF })}</Label>
                                        <Slider
                                            min={0}
                                            max={videoCRFChoices.length - 1}
                                            step={1}
                                            value={[videoCRFIndex]}
                                            onValueChange={(value) => {
                                                const newCRF = videoCRFChoices[Array.isArray(value) ? value[0] : value];
                                                if (newCRF !== undefined) handleVideoCRFChange(newCRF);
                                            }}
                                            disabled={!serverSettings || serverSettings.video_crf?.min === serverSettings.video_crf?.max}
                                        />
                                    </div>
                                )}

                                {showTurbo && (
                                    <div className="flex items-center justify-between">
                                        <Label title={t('sections.video.streamingModeDetails')}>{t('sections.video.streamingModeLabel')}</Label>
                                        <Switch size="sm"
                                            checked={videoStreamingMode}
                                            onCheckedChange={handleH264StreamingModeToggle}
                                            disabled={!serverSettings || serverSettings.video_streaming_mode?.locked}
                                        />
                                    </div>
                                )}

                                {/* Base JPEG quality is independent of paint-over. */}
                                {showJpegQualitySlider && (
                                    <div className="space-y-2">
                                        <Label>{t('sections.video.jpegQualityLabel', { jpegQuality })}</Label>
                                        <Slider
                                            min={serverSettings?.jpeg_quality?.min || 1}
                                            max={serverSettings?.jpeg_quality?.max || 100}
                                            step={1}
                                            value={[jpegQuality]}
                                            onValueChange={(value) => handleJpegQualityChange(Array.isArray(value) ? value[0] : value)}
                                            disabled={!serverSettings || serverSettings.jpeg_quality?.min === serverSettings.jpeg_quality?.max}
                                        />
                                    </div>
                                )}

                                {/* Server honors paint-over quality for every H.264 encoder and jpeg. */}
                                {showPaintOverSwitch && (
                                    <div className="flex items-center justify-between">
                                        <Label>{t('sections.video.usePaintOverQualityLabel')}</Label>
                                        <Switch size="sm"
                                            checked={usePaintOverQuality}
                                            onCheckedChange={handleUsePaintOverQualityToggle}
                                            disabled={!serverSettings || serverSettings.use_paint_over_quality?.locked}
                                        />
                                    </div>
                                )}

                                {showPaintoverCrf && (
                                    <div className="space-y-2">
                                        <Label>{tl('sections.video.paintoverCrfLabel', { crf: videoPaintoverCRF })}</Label>
                                        <Slider
                                            min={0}
                                            max={videoPaintoverCRFChoices.length - 1}
                                            step={1}
                                            value={[stopIndex(videoPaintoverCRFChoices, videoPaintoverCRF)]}
                                            onValueChange={(value) => {
                                                const newCRF = videoPaintoverCRFChoices[Array.isArray(value) ? value[0] : value];
                                                if (newCRF !== undefined) handleH264PaintoverCRFChange(newCRF);
                                            }}
                                            disabled={!serverSettings || serverSettings.video_paintover_crf?.min === serverSettings.video_paintover_crf?.max}
                                        />
                                    </div>
                                )}
                                {showPaintoverBurst && (
                                    <div className="space-y-2">
                                        <Label>{tl('sections.video.paintoverBurstLabel', { frames: videoPaintoverBurstFrames })}</Label>
                                        <Slider
                                            min={serverSettings?.video_paintover_burst_frames?.min || 1}
                                            max={serverSettings?.video_paintover_burst_frames?.max || 30}
                                            step={1}
                                            value={[videoPaintoverBurstFrames]}
                                            onValueChange={(value) => handleH264PaintoverBurstChange(Array.isArray(value) ? value[0] : value)}
                                            disabled={!serverSettings || serverSettings.video_paintover_burst_frames?.min === serverSettings.video_paintover_burst_frames?.max}
                                        />
                                    </div>
                                )}

                                {showPaintOverJpeg && (
                                    <div className="space-y-2">
                                        <Label>{t('sections.video.paintOverJpegQualityLabel', { paintOverJpegQuality })}</Label>
                                        <Slider
                                            min={serverSettings?.paint_over_jpeg_quality?.min || 1}
                                            max={serverSettings?.paint_over_jpeg_quality?.max || 100}
                                            step={1}
                                            value={[paintOverJpegQuality]}
                                            onValueChange={(value) => handlePaintOverJpegQualityChange(Array.isArray(value) ? value[0] : value)}
                                            disabled={!serverSettings || serverSettings.paint_over_jpeg_quality?.min === serverSettings.paint_over_jpeg_quality?.max}
                                        />
                                    </div>
                                )}

                                {showCpuSwitch && (
                                    <div className="flex items-center justify-between">
                                        <Label>{t('sections.video.useCpuLabel')}</Label>
                                        <Switch size="sm"
                                            checked={useCpu}
                                            onCheckedChange={handleUseCpuToggle}
                                            disabled={!serverSettings || serverSettings.use_cpu?.locked}
                                        />
                                    </div>
                                )}
                            </SectionItem>
                        )}
                        </SectionAccordion>
                    </CardContent>
                </TabsContent>
                )}

                {showAudioTab && (
                <TabsContent value="audio">
                    <CardContent className="px-0">

                        <SectionAccordion defaultValue={[showQualityCard ? "quality" : "devices"]}>
                        {showQualityCard && (
                            <SectionItem value="quality" title={t('settingsSections.quality')}>
                                <div className="space-y-2">
                                    <Label>{tl('sections.audio.bitrateLabel', { bitrate: audioBitRate / 1000 })}</Label>
                                    <Slider
                                        min={0}
                                        max={audioBitrateChoices.length - 1}
                                        step={1}
                                        value={[Math.max(0, audioBitrateChoices.indexOf(audioBitRate))]}
                                        onValueChange={(value) => {
                                            const index = Array.isArray(value) ? value[0] : value;
                                            const selectedBitrate = audioBitrateChoices[index];
                                            if (selectedBitrate !== undefined) {
                                                setAudioBitRate(selectedBitrate);
                                                localStorage.setItem(getPrefixedKey('audio_bitrate'), selectedBitrate.toString());
                                                debouncedPostSetting({ audio_bitrate: selectedBitrate });
                                            }
                                        }}
                                    />
                                </div>
                            </SectionItem>
                        )}

                        <SectionItem value="devices" title={t('settingsSections.devices')}>
                            {audioDeviceError && (
                                <div className="text-sm text-red-500">{audioDeviceError}</div>
                            )}

                            <div className="space-y-2">
                                <Label>{tl('sections.audio.inputLabel')}</Label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger
                                        render={<Button variant="outline" size="xs" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError} />}
                                    >
                                        <span className="truncate">
                                            {audioInputDevices.find(d => d.deviceId === selectedInputDeviceId)?.label || t('audio.defaultDevice')}
                                        </span>
                                        <ChevronDown />
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-[280px] max-w-[90vw]">
                                        {audioInputDevices.map(device => (
                                            <DropdownMenuItem
                                                key={device.deviceId}
                                                onClick={() => {
                                                    setSelectedInputDeviceId(device.deviceId);
                                                    window.postMessage({ type: 'audioDeviceSelected', context: 'input', deviceId: device.deviceId }, window.location.origin);
                                                }}
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
                                    <Label>{tl('sections.audio.outputLabel')}</Label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger
                                            render={<Button variant="outline" size="xs" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError} />}
                                        >
                                            <span className="truncate">
                                                {audioOutputDevices.find(d => d.deviceId === selectedOutputDeviceId)?.label || t('audio.defaultDevice')}
                                            </span>
                                            <ChevronDown />
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent className="w-[280px] max-w-[90vw]">
                                            {audioOutputDevices.map(device => (
                                                <DropdownMenuItem
                                                    key={device.deviceId}
                                                    onClick={() => {
                                                        setSelectedOutputDeviceId(device.deviceId);
                                                        window.postMessage({ type: 'audioDeviceSelected', context: 'output', deviceId: device.deviceId }, window.location.origin);
                                                    }}
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
                        </SectionItem>
                        </SectionAccordion>
                    </CardContent>
                </TabsContent>
                )}
            </Tabs>
        </Card>
    );
}
