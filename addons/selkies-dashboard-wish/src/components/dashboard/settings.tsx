/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Card, CardContent } from "@/components/ui/card";
import { displayLabel } from "../../../../selkies-web-core/lib/util.js";
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
import React, { useState, useEffect, useCallback } from "react";
import { getPrefixedKey, getRoutePrefix, computeRenderableSettings, getLastServerSettings, isSecondaryDisplay } from "@/utils";
import { t, tl } from "@/i18n";

// Constants
// Mirror the server's audio_bitrate allowed enum (settings.py) so the slider
// never offers a value the server rejects and silently ignores (510k = libopus max).
const audioBitrateOptions = [32000, 48000, 64000, 96000, 128000, 192000, 256000, 320000, 384000, 510000];
const DEFAULT_AUDIO_BITRATE = 128000;

// DPI Scaling options for UI scaling
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
// scaling_dpi DEFAULT synced to the local display scaling (devicePixelRatio) so the remote
// desktop's fonts/UI match the local environment; an explicit slider value diverges (wins).
// Same formula as the core (autoDeriveDpi). Independent of the resolution.
// Snapping to the NEAREST option puts a density the options do not name on the
// closest one, and clamps at both ends.
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
    "openh264enc",
    "jpeg",
];

// WebRTC encoders (mirrors the server's encoder_rtc allowed list).
const encoderOptionsRTC = [
    "h264enc",
    "openh264enc",
];

// Every H.264 encoder supports both CBR and CRF (constant-QP) rate control.
const H264_ENCODERS = ["h264enc", "h264enc-striped", "openh264enc", "nvh264enc"];

const FRAMERATE_STEPS = [8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144, 165, 240];

// CRF stops stay inside the server-supported video_crf range (min 5).
const videoCRFOptions = [50, 45, 40, 35, 30, 25, 20, 10, 5];

// Sub-Mbps CBR stops (kbps) for constrained links, ahead of the whole-Mbps
// range (1000-kbps steps).
const SUB_MBPS_BITRATE_STEPS = [100, 250, 500, 750];
// Above 100000 kbps the slider coarsens to these stops; per-1000 granularity
// stops mattering there and a 1000-position slider would be unusable.
const COARSE_MBPS_BITRATE_STEPS = [150000, 200000, 300000, 400000, 500000, 750000, 1000000];

const readStored = (key: string) => localStorage.getItem(getPrefixedKey(key));

// The cores persist every value they are told to apply, so the stored key alone
// cannot tell a user's explicit pick from one the dashboard derived (HiDPI from
// the resolution mode, rate control from the encoder). An explicit choice writes
// a marker beside the value; the settings that are also derived read storage
// through this reader, so a derived write never pins them.
const EXPLICIT_CHOICE_SUFFIX = "_explicit_choice";
// Suffixed onto the already-prefixed value key so the marker inherits the
// per-display suffix and a secondary display keeps its own choice.
const explicitChoiceKey = (spec: any) => `${getPrefixedKey(spec.storageKey)}${EXPLICIT_CHOICE_SUFFIX}`;
const isExplicitChoice = (spec: any) => localStorage.getItem(explicitChoiceKey(spec)) === "true";
const readExplicitStored = (spec: any) => (key: string) => (
    isExplicitChoice(spec) ? readStored(key) : null
);

// Drives a conditional setting: lazy init + re-resolve whenever the server
// settings or any dependency in `deps` changes (server-sync AND encoder/manual-
// resolution re-derivation, uniformly). The resolver honors explicit choices,
// so a re-resolve never clobbers a pinned value. Returns [value, setValue].
function useConditionalSetting(spec: any, serverSettings: any, ctx: any, deps: any[], read: any = readStored) {
    const compute = () => resolveSpec(spec, serverSettings, ctx, read);
    const [value, setValue] = useState(compute);
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
// Expressed in kbps
const DEFAULT_VIDEO_BITRATE = 8000;

const roundDownToEven = (num: number) => {
    const n = parseInt(num.toString(), 10);
    if (isNaN(n)) return 0;
    return Math.floor(n / 2) * 2;
};

// Debounce function
function debounce(func: (...args: any[]) => void, delay: number) {
    let timeoutId: ReturnType<typeof setTimeout>;
    return function (this: unknown, ...args: any[]) {
        const context = this;
        clearTimeout(timeoutId);
        timeoutId = setTimeout(() => {
            func.apply(context, args);
        }, delay);
    };
}

interface SettingsProps {
    scale?: number;
}

export function Settings() {
    // --- Server Settings (seeded from the cached broadcast; panels mount late) ---
    const [serverSettings, setServerSettings] = useState<any>(() => getLastServerSettings());
    const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));

    // --- Streaming Mode ---
    const [streamMode, setStreamMode] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("stream_mode"));
        if (saved && STREAMING_MODES.includes(saved)) return saved;
        const runtimeMode = (window as any).__SELKIES_STREAMING_MODE__;
        if (runtimeMode && STREAMING_MODES.includes(runtimeMode)) return runtimeMode;
        return DEFAULT_STREAM_MODE;
    });
    const isWebrtc = streamMode === STREAM_MODE_WEBRTC;

    const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState(
        isWebrtc ? encoderOptionsRTC : encoderOptions
    );

    // Screen Settings State (localStorage keys mirror the streaming core's)
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

    // HiDPI and UI Scaling State
    const [selectedDpi, setSelectedDpi] = useState(() => {
        // Explicit stored value diverges (wins); otherwise default to the local display scaling.
        return parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")) ?? "", 10) || deriveDpiFromDpr();
    });

    // Video and Audio Settings State
    const [videoBitRate, setVideoBitRate] = useState(() => {
        // kbps on the wire and in storage.
        const parsed = parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")) ?? "", 10);
        return !isNaN(parsed) ? parsed : DEFAULT_VIDEO_BITRATE;
    });
    const [audioBitRate, setAudioBitRate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("audio_bitrate")) ?? "", 10) || DEFAULT_AUDIO_BITRATE
    );
    const [encoder, setEncoder] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder")) || "h264enc"
    );
    const [encoderRTC, setEncoderRTC] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder_rtc")) || "h264enc"
    );
    const [framerate, setFramerate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("framerate")) ?? "", 10) || 60
    );
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_crf"));
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    // State the conditional settings read; rebuilt each render so the hooks
    // below re-resolve against current values when their deps change.
    const conditionalCtx = {
        manualActive: !!readStored("manual_width") || serverSettings?.is_manual_resolution_mode?.value === true,
        streamMode,
        activeEncoder: (streamMode === STREAM_MODE_WEBRTC)
            ? (readStored("encoder_rtc") || encoderRTC)
            : (readStored("encoder") || encoder),
        allowedRateControl: serverSettings?.rate_control_mode?.allowed || rateControlOptions,
    };
    // Each conditional setting: one hook call over a shared spec. The hook owns
    // init + server-sync; client-driven changes (explicit toggle, or a
    // dependency like the encoder/resolution) flow through writeConditional
    // below, which sets state, persists, and propagates uniformly.
    const [hidpiEnabled, setHidpiEnabled] = useConditionalSetting(
        HIDPI_SPEC, serverSettings, conditionalCtx, [serverSettings], readHidpiStored);
    const [rateControlMode, setRateControlMode] = useConditionalSetting(
        RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings, streamMode], readRateControlStored);
    // With rate control disabled the server ignores rate_control_mode entirely
    // and keeps the encoder on its built-in default, so the dashboard neither
    // pushes a mode nor lets its own pick decide which quality slider is shown.
    const rateControlEnabled = renderableSettings.enableRateControl ?? true;
    // The hook above only sets local UI state: when the resolved default
    // diverges from what the server is applying (a transport switch seeds the
    // session with the previous mode's value), push it so the encoder follows.
    // Pinned/locked/operator-overridden values resolve to the server's value
    // and post nothing.
    useEffect(() => {
        if (!serverSettings) return;
        if (serverSettings.enable_rate_control?.value === false) return;
        const rcKey = RATE_CONTROL_SPEC.storageKey;
        const resolved = resolveSpec(
            RATE_CONTROL_SPEC, serverSettings, conditionalCtx, readRateControlStored);
        // The core persists every mode it is told to apply and resends it on the
        // next connect. Without an explicit pick that stored echo is not a choice:
        // drop it once it stops matching what the ladder resolves, or it would
        // outlive the derivation — and an operator override with it.
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
    const [videoFullColor, setVideoFullColor] = useConditionalSetting(
        VIDEO_FULLCOLOR_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [videoStreamingMode, setVideoStreamingMode] = useConditionalSetting(
        VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [jpegQuality, setJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("jpeg_quality")) ?? "", 10) || 60
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
        USE_PAINT_OVER_QUALITY_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [useCpu, setUseCpu] = useConditionalSetting(
        USE_CPU_SPEC, serverSettings, conditionalCtx, [serverSettings]);

    // Anti-aliasing stays client-only (no server truth), so it keeps its own state.
    const [antiAliasing, setAntiAliasing] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useConditionalSetting(
        USE_BROWSER_CURSORS_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    // The value the core reports as actually in effect (multi-monitor forces
    // browser cursors on); null until the core reports. The toggle displays this
    // over the stored preference so it never lies about the live state.
    const [effectiveCursor, setEffectiveCursor] = useState<boolean | null>(null);
    const [forceAlignedResolution, setForceAlignedResolution] = useConditionalSetting(
        FORCE_ALIGNED_RESOLUTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);

    // Audio device state
    const [audioInputDevices, setAudioInputDevices] = useState<any[]>([]);
    const [audioOutputDevices, setAudioOutputDevices] = useState<any[]>([]);
    const [selectedInputDeviceId, setSelectedInputDeviceId] = useState('default');
    const [selectedOutputDeviceId, setSelectedOutputDeviceId] = useState('default');
    const [isOutputSelectionSupported, setIsOutputSelectionSupported] = useState(false);
    const [audioDeviceError, setAudioDeviceError] = useState<string | null>(null);
    const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);

    // --- Debounced Settings Handler ---
    const DEBOUNCE_DELAY = 500;
    const debouncedPostSetting = useCallback(
        debounce((setting: any) => {
            window.postMessage(
                { type: "settings", settings: setting },
                window.location.origin
            );
        }, DEBOUNCE_DELAY),
        []
    );

    // Uniform write path for conditional settings: optimistic setState, optional
    // persist (explicit choices pin; derived ones don't), and propagate via the
    // spec. `io` routes the two push channels; the specs decide which to use.
    const conditionalIo = {
        postSetting: (obj: any) => debouncedPostSetting(obj),
        postToCore: (obj: any) => window.postMessage(obj, window.location.origin),
    };
    const writeConditional = (spec: any, uiValue: any, setValue: any, opts: any = {}) => {
        setValue(uiValue);
        if (opts.persist) {
            localStorage.setItem(getPrefixedKey(spec.storageKey),
                spec.serialize ? spec.serialize(uiValue) : String(uiValue));
            localStorage.setItem(explicitChoiceKey(spec), "true");
        }
        spec.propagate(spec.toServer ? spec.toServer(uiValue) : uiValue, conditionalCtx, conditionalIo);
    };

    // --- Server Settings Message Listener ---
    useEffect(() => {
        const handleMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            if (event.data?.type === "serverSettings") {
                console.log("Settings received server settings:", event.data.payload);
                setServerSettings(event.data.payload);
                setRenderableSettings(computeRenderableSettings(event.data.payload));
            }
            // The core reports the cursor value actually in effect (multi-monitor
            // forces browser cursors on); reflect it so the toggle can't lie.
            if (event.data?.type === "effectiveCursorState" && typeof event.data.value === "boolean") {
                setEffectiveCursor(event.data.value);
            }
            // Mirror the dashboard's own selection back into the dropdowns, so
            // the displayed device always matches what the core was told.
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

    // --- Server Settings Integration ---
    useEffect(() => {
        if (!serverSettings) return;

        const getStoredInt = (key: string) => parseInt(localStorage.getItem(getPrefixedKey(key)) ?? "", 10);
        const getStoredBool = (key: string, fallback = false) => {
            const v = localStorage.getItem(getPrefixedKey(key));
            return v === null ? fallback : v === 'true';
        };

        // Update encoder options from the mode-appropriate server setting.
        const s_encoder = serverSettings.encoder;
        if (s_encoder && streamMode !== STREAM_MODE_WEBRTC) {
            const stored = localStorage.getItem(getPrefixedKey("encoder"));
            const final = s_encoder.allowed.includes(stored) ? stored : s_encoder.value;
            setEncoder(final);
            setDynamicEncoderOptions(s_encoder.allowed);
        }

        const s_encoder_rtc = serverSettings.encoder_rtc;
        if (s_encoder_rtc && streamMode === STREAM_MODE_WEBRTC) {
            const stored = localStorage.getItem(getPrefixedKey("encoder_rtc"));
            const final = s_encoder_rtc.allowed.includes(stored) ? stored : s_encoder_rtc.value;
            setEncoderRTC(final);
            setDynamicEncoderOptions(s_encoder_rtc.allowed);
        }

        // HiDPI and rate control are conditional settings handled by their
        // useConditionalSetting hooks (init + sync + dependency re-derivation).

        // Update framerate from server constraints
        const s_framerate = serverSettings.framerate;
        if (s_framerate) {
            const stored = getStoredInt("framerate");
            const final = !isNaN(stored)
                ? Math.max(s_framerate.min, Math.min(s_framerate.max, stored))
                : s_framerate.default;
            setFramerate(final);
        }

        // Clamp the CBR bitrate (kbps) to the server range
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
            // `allowed` holds string bps ("128000"), `stored`/`value` are numbers:
            // compare as strings and parse the fallback (classic-dashboard parity).
            const final = !isNaN(stored)
                ? (s_audio_bitrate.allowed
                    ? (s_audio_bitrate.allowed.includes(String(stored)) ? stored : parseInt(s_audio_bitrate.value, 10))
                    : Math.max(s_audio_bitrate.min ?? stored, Math.min(s_audio_bitrate.max ?? stored, stored)))
                : parseInt(s_audio_bitrate.value, 10);
            setAudioBitRate(final);
        }

        // Update other settings from server constraints...
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

        // video_fullcolor, video_streaming_mode, use_paint_over_quality, use_cpu,
        // use_browser_cursors and force_aligned_resolution resolve through the
        // shared ladder (useConditionalSetting above), so they stay overridden- and
        // locked-aware without a bespoke sync line here.

        const s_scaling_dpi = serverSettings.scaling_dpi;
        if (s_scaling_dpi) {
            const stored = getStoredInt("scaling_dpi");
            const storedAllowed = s_scaling_dpi.allowed.includes(String(stored));
            const serverVal = parseInt(s_scaling_dpi.value, 10);
            const derived = deriveDpiFromDpr();
            // Ladder matches what actually governs the desktop: an operator override
            // (which the server refuses to let clients clobber) > the stored pick >
            // the derived local-display default (the cores send stored-else-derived
            // on every connect, independent of the resolution mode).
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
    }, [serverSettings, streamMode]);

    // Audio device population. Enumerating labelled devices needs a getUserMedia
    // grant, so it is deferred until the Audio tab is actually shown: merely
    // opening Settings must not raise a microphone permission prompt.
    const audioDevicesRequested = React.useRef(false);
    const ensureAudioDevices = useCallback(() => {
        if (audioDevicesRequested.current) return;
        audioDevicesRequested.current = true;
        const populateAudioDevices = async () => {
            setIsLoadingAudioDevices(true);
            setAudioDeviceError(null);
            setAudioInputDevices([]);
            setAudioOutputDevices([]);

            const supportsSinkId = 'setSinkId' in HTMLMediaElement.prototype;
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
                setSelectedInputDeviceId('default');
                setSelectedOutputDeviceId('default');

            } catch (err) {
                const error = err instanceof Error ? err : new Error(String(err));
                console.error('Error getting media devices:', error);
                setAudioDeviceError(error.message || t('sections.audio.deviceErrorDefault', { errorName: error.name || 'unknown' }));
            } finally {
                setIsLoadingAudioDevices(false);
            }
        };

        populateAudioDevices();
    }, []);

    // Screen Settings Handlers. A half-typed size stays in component state: the
    // stored manual_width/manual_height mean "a manual resolution is applied",
    // which the HiDPI and UI-scaling derivations read, so only Set/preset/Reset
    // may write them.
    const handleManualWidthChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setManualWidth(event.target.value);
        setPresetValue("");
    };

    const handleManualHeightChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        setManualHeight(event.target.value);
        setPresetValue("");
    };

    const handleScaleLocallyToggle = () => {
        const newState = !scaleLocally;
        setScaleLocally(newState);
        // Core persists scaleLocallyManual itself when handling the message.
        window.postMessage({ type: 'setScaleLocally', value: newState }, window.location.origin);
    };

    // HiDPI and UI Scaling Handlers. An explicit toggle pins the choice; the
    // core persists useCssScaling when it applies the message.
    const handleHidpiToggle = () => {
        writeConditional(HIDPI_SPEC, !hidpiEnabled, setHidpiEnabled, { persist: true });
    };

    const handleDpiScalingChange = (value: string) => {
        const newDpi = parseInt(value, 10);
        setSelectedDpi(newDpi);
        localStorage.setItem(getPrefixedKey('scaling_dpi'), newDpi.toString());
        debouncedPostSetting({ scaling_dpi: newDpi });
    };

    // Streaming Mode Handler: ask the server to swap transports, then let the
    // core loader persist the mode and reload the page into the new stack.
    const handleStreamModeChange = async (mode: string) => {
        if (mode === streamMode) return;
        // Mark the switch before asking the server to swap transports: /api/switch tears
        // down the old peer (WS close code 4000) before it responds, so the flag must be
        // set first or the active core surfaces a spurious "Server disconnected" alert.
        window.__selkiesModeSwitching = true;
        try {
            // /switch is gated on the master token (Bearer) when set, or Basic creds via
            // same-origin. With Basic Auth off, the Bearer is required but the dashboard
            // isn't given it: on a 401 prompt once, keep it in sessionStorage, and retry.
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
            window.__selkiesModeSwitching = false;
            console.error("Error switching stream mode:", error);
        }
    };

    // Video Settings Handlers
    const handleEncoderChange = (selectedEncoder: string) => {
        if (isWebrtc) {
            setEncoderRTC(selectedEncoder);
            localStorage.setItem(getPrefixedKey('encoder_rtc'), selectedEncoder);
            // WebRTC uses encoder_rtc; the server switches the pipeline encoder on this.
            debouncedPostSetting({ encoder_rtc: selectedEncoder });
        } else {
            setEncoder(selectedEncoder);
            localStorage.setItem(getPrefixedKey('encoder'), selectedEncoder);
            debouncedPostSetting({ encoder: selectedEncoder });
        }
        // Rate control follows the encoder unless pinned (explicit client/server
        // choice). A derived change is not persisted, so it keeps following.
        if (rateControlEnabled
            && !isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readRateControlStored)) {
            const rcResolved = resolveSpec(
                RATE_CONTROL_SPEC, serverSettings,
                { ...conditionalCtx, activeEncoder: selectedEncoder }, readRateControlStored);
            if (rcResolved !== rateControlMode) {
                writeConditional(RATE_CONTROL_SPEC, rcResolved, setRateControlMode, { persist: false });
            }
        }
    };

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

    const handleRateControlChange = (mode: string) => {
        // Explicit choice: pin it (persist) so encoder changes stop overriding.
        writeConditional(RATE_CONTROL_SPEC, mode, setRateControlMode, { persist: true });
    };

    const handleVideoBitRateChange = (selectedBitRate: number) => {
        setVideoBitRate(selectedBitRate);
        localStorage.setItem(getPrefixedKey('video_bitrate'), selectedBitRate.toString());
        // video_bitrate is kbps on the wire; the slider works in kbps.
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
    };

    // Anti-aliasing stays client-only; the core persists antiAliasingEnabled itself.
    const handleAntiAliasingToggle = () => {
        const newState = !antiAliasing;
        setAntiAliasing(newState);
        window.postMessage(
            { type: 'setAntiAliasing', value: newState },
            window.location.origin
        );
    };

    const handleUseBrowserCursorsToggle = () => {
        // The core owns persistence; propagate the new preference and let the core
        // report the effective (possibly multi-monitor-forced) value back. Derive
        // from the DISPLAYED value: while multi-monitor forces the toggle on, the
        // base preference may be off, and negating the base would silently persist
        // the forced value over the user's real choice.
        writeConditional(USE_BROWSER_CURSORS_SPEC, !(effectiveCursor ?? useBrowserCursors), setUseBrowserCursors, { persist: false });
    };

    const handleForceAlignedResolutionToggle = () => {
        writeConditional(FORCE_ALIGNED_RESOLUTION_SPEC, !forceAlignedResolution, setForceAlignedResolution, { persist: true });
    };

    // Manual/preset resolutions pair with CSS scaling: HiDPI off when one is
    // set, on when reset — a derived write (not pinned), through the uniform
    // path. An explicit toggle or a locked/overridden server value pins HiDPI
    // and stops the resolution buttons from re-deriving it.
    const deriveHidpiForResolution = (manual: boolean) => {
        if (isSettingPinned(HIDPI_SPEC, serverSettings, readHidpiStored)) return;
        writeConditional(HIDPI_SPEC, !manual, setHidpiEnabled, { persist: false });
    };

    // Reset-to-window also returns UI scaling to its derived (devicePixelRatio-
    // based) default: the pinned client choice is dropped so the derived default
    // governs again, and the value propagates like a user change (state update +
    // settings post). Locked or operator-explicit (overridden) values govern
    // scaling instead — the same gate as the startup derived-default post — so
    // skip then.
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
        deriveHidpiForResolution(false);
        resetDpiToDerivedDefault();
    };

    // CBR stops: sub-Mbps kbps steps for constrained links, whole-Mbps steps
    // to 100000, then the coarse steps to 1000000.
    const videoBitrateOptions = (() => {
        const min = serverSettings?.video_bitrate?.min ?? 100;
        const max = serverSettings?.video_bitrate?.max ?? 1000000;
        const stops = SUB_MBPS_BITRATE_STEPS.filter(v => v >= min && v <= max);
        for (let v = Math.max(1000, Math.ceil(min / 1000) * 1000); v <= Math.min(100000, Math.floor(max / 1000) * 1000); v += 1000) stops.push(v);
        stops.push(...COARSE_MBPS_BITRATE_STEPS.filter(v => v >= min && v <= max));
        return stops.length ? stops : [min];
    })();
    // Framerate stops clipped to the server-allowed span, mirroring how the
    // stored value itself is clamped.
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
    // CRF stops clipped to the server-allowed span. The list descends (higher
    // quality to the right), so the nearest fallback for an off-stop value
    // (server default, clamp) is the first stop at or below it.
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

    // --- Render Gating ---
    // Audio stops come from the server enum when connected (Opus enum list);
    // the static fallback covers the no-server-settings window.
    const audioBitrateChoices = (serverSettings?.audio_bitrate?.allowed?.map((v: string) => parseInt(v, 10))) || audioBitrateOptions;
    // UI scaling stops come from the server enum when connected; the static
    // fallback covers the no-server-settings window.
    const dpiScalingChoices: { label: string; value: number }[] = (serverSettings?.scaling_dpi?.allowed?.map((v: string) => {
        const value = parseInt(v, 10);
        return { label: `${Math.round((value / 96) * 100)}%`, value };
    })) || dpiScalingOptions;
    // A single allowed stop, or an operator-set DPI (the server drops client DPI
    // syncs while scaling_dpi is overridden), leaves nothing the picker can change.
    const dpiScalingDisabled = !serverSettings || serverSettings.scaling_dpi?.allowed?.length <= 1
        || serverSettings.scaling_dpi?.overridden === true;
    const activeEncoder = isWebrtc ? encoderRTC : encoder;
    const isH264 = H264_ENCODERS.includes(activeEncoder);
    const showJpegOptions = !isWebrtc && activeEncoder === 'jpeg';
    const showRateControl = rateControlEnabled && isH264;
    // The quality slider must belong to the mode the encoder is actually using:
    // with rate control disabled that is the server's mode, not the dashboard's.
    const appliedRateControlMode = rateControlEnabled
        ? rateControlMode
        : (serverSettings?.rate_control_mode?.value ?? rateControlMode);
    const encoderRenderable = isWebrtc
        ? (renderableSettings.encoderRtc ?? true)
        : (renderableSettings.encoder ?? true);

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
                                            <label className="text-sm font-medium">{t('sections.screen.hidpiLabel')}</label>
                                        </div>
                                        <Switch
                                            checked={hidpiEnabled}
                                            onCheckedChange={handleHidpiToggle}
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

                        {/* Server honors paint-over quality for every H.264 encoder and jpeg.
                            The toggle precedes the settings it gates. */}
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

                        {/* use_cpu only changes behavior for full-frame h264enc (HW vs x264);
                            the server forces it true for jpeg/striped/openh264 in both transports. */}
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
