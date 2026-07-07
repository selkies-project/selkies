/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Card, CardContent } from "@/components/ui/card";
import { displayLabel } from "../../../../selkies-web-core/lib/util.js";
import { resolveSpec, isSettingPinned, HIDPI_SPEC, RATE_CONTROL_SPEC } from "../../../../selkies-web-core/lib/conditional-settings.js";
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
const DEFAULT_SCALING_DPI = 96;
// scaling_dpi DEFAULT synced to the local display scaling (devicePixelRatio) so the remote
// desktop's fonts/UI match the local environment; an explicit slider value diverges (wins).
// Same formula as the core (selkies-wr-core autoDeriveDpi). Independent of the resolution.
const deriveDpiFromDpr = (): number => {
    const dpr = window.devicePixelRatio || 1;
    const target = Math.round(dpr * 4) * 24;
    return (dpr > 1 && [120, 144, 168, 192, 216, 240, 288].includes(target)) ? target : DEFAULT_SCALING_DPI;
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

const framerateOptions = [8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144];

const videoCRFOptions = [50, 45, 40, 35, 30, 25, 20, 10, 1];

// Sub-Mbps CBR stops for constrained links, ahead of the whole-Mbps range.
const SUB_MBPS_BITRATE_STEPS = [0.1, 0.25, 0.5, 0.75];

const readStored = (key: string) => localStorage.getItem(getPrefixedKey(key));

// Drives a conditional setting: lazy init + re-resolve whenever the server
// settings or any dependency in `deps` changes (server-sync AND encoder/manual-
// resolution re-derivation, uniformly). The resolver honors explicit choices,
// so a re-resolve never clobbers a pinned value. Returns [value, setValue].
function useConditionalSetting(spec: any, serverSettings: any, ctx: any, deps: any[]) {
    const compute = () => resolveSpec(spec, serverSettings, ctx, readStored);
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
const DEFAULT_VIDEO_BITRATE = 8;

const roundDownToEven = (num: number) => {
    const n = parseInt(num.toString(), 10);
    if (isNaN(n)) return 0;
    return Math.floor(n / 2) * 2;
};

// Debounce function
function debounce(func: Function, delay: number) {
    let timeoutId: NodeJS.Timeout;
    return function (...args: any[]) {
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
        return parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")), 10) || deriveDpiFromDpr();
    });

    // Video and Audio Settings State
    const [videoBitRate, setVideoBitRate] = useState(() => {
        // Normalize legacy values stored in kbps down to Mbps (pure read;
        // the normalized value is re-persisted on the next change).
        // Fractional Mbps values are legal (sub-Mbps stops).
        const parsed = parseFloat(localStorage.getItem(getPrefixedKey("video_bitrate")));
        if (!isNaN(parsed) && parsed > 1000) return Math.round(parsed / 1000);
        return !isNaN(parsed) ? parsed : DEFAULT_VIDEO_BITRATE;
    });
    const [audioBitRate, setAudioBitRate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("audio_bitrate")), 10) || DEFAULT_AUDIO_BITRATE
    );
    const [encoder, setEncoder] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder")) || "h264enc"
    );
    const [encoderRTC, setEncoderRTC] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder_rtc")) || "h264enc"
    );
    const [framerate, setFramerate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("framerate")), 10) || 60
    );
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_crf"));
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    // State the conditional settings read; rebuilt each render so the hooks
    // below re-resolve against current values when their deps change.
    const conditionalCtx = {
        manualActive: !!readStored("manual_width") || serverSettings?.is_manual_resolution_mode?.value === true,
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
        HIDPI_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [rateControlMode, setRateControlMode] = useConditionalSetting(
        RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings]);
    const [videoFullColor, setVideoFullColor] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_fullcolor"));
        return saved !== null ? saved === 'true' : false;
    });
    const [videoStreamingMode, setVideoStreamingMode] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_streaming_mode"));
        return saved !== null ? saved === 'true' : false;
    });
    const [jpegQuality, setJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("jpeg_quality")), 10) || 60
    );
    const [paintOverJpegQuality, setPaintOverJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("paint_over_jpeg_quality")), 10) || 90
    );
    const [videoPaintoverCRF, setVideoPaintoverCRF] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("video_paintover_crf")), 10) || 18
    );
    const [videoPaintoverBurstFrames, setVideoPaintoverBurstFrames] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("video_paintover_burst_frames")), 10) || 5
    );
    const [usePaintOverQuality, setUsePaintOverQuality] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("use_paint_over_quality"));
        return saved !== null ? saved === 'true' : true;
    });
    const [useCpu, setUseCpu] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("use_cpu"));
        return saved !== null ? saved === 'true' : false;
    });

    // Anti-aliasing and Browser Cursors State
    const [antiAliasing, setAntiAliasing] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("use_browser_cursors"));
        return saved !== null ? saved === "true" : false;
    });
    const [forceAlignedResolution, setForceAlignedResolution] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("force_aligned_resolution"));
        return saved !== null ? saved === "true" : false;
    });

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
            // Keep the dropdowns in sync when a device is picked elsewhere
            // (e.g. the core's dev sidebar posts the same message type).
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

        const getStoredInt = (key: string) => parseInt(localStorage.getItem(getPrefixedKey(key)), 10);
        const getStoredBool = (key: string) => localStorage.getItem(getPrefixedKey(key)) === 'true';

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

        // Clamp the CBR bitrate (Mbps, fractional allowed) to the server range
        const s_video_bitrate = serverSettings.video_bitrate;
        if (s_video_bitrate) {
            const stored = parseFloat(localStorage.getItem(getPrefixedKey("video_bitrate")));
            const final = !isNaN(stored)
                ? Math.max(s_video_bitrate.min, Math.min(s_video_bitrate.max, stored))
                : s_video_bitrate.default;
            setVideoBitRate(final);
        }

        const s_audio_bitrate = serverSettings.audio_bitrate;
        if (s_audio_bitrate) {
            const stored = getStoredInt("audio_bitrate");
            const final = !isNaN(stored)
                ? (s_audio_bitrate.allowed
                    ? (s_audio_bitrate.allowed.includes(stored) ? stored : s_audio_bitrate.value)
                    : Math.max(s_audio_bitrate.min ?? stored, Math.min(s_audio_bitrate.max ?? stored, stored)))
                : s_audio_bitrate.value;
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

        // Boolean settings
        const s_video_fullcolor = serverSettings.video_fullcolor;
        if (s_video_fullcolor) {
            const final = s_video_fullcolor.locked ? s_video_fullcolor.value : getStoredBool("video_fullcolor");
            setVideoFullColor(final);
        }

        const s_video_streaming_mode = serverSettings.video_streaming_mode;
        if (s_video_streaming_mode) {
            const final = s_video_streaming_mode.locked ? s_video_streaming_mode.value : getStoredBool("video_streaming_mode");
            setVideoStreamingMode(final);
        }

        const s_use_paint_over_quality = serverSettings.use_paint_over_quality;
        if (s_use_paint_over_quality) {
            const stored = localStorage.getItem(getPrefixedKey("use_paint_over_quality"));
            const final = s_use_paint_over_quality.locked ? s_use_paint_over_quality.value : (stored !== null ? stored === 'true' : s_use_paint_over_quality.value);
            setUsePaintOverQuality(final);
        }

        const s_use_cpu = serverSettings.use_cpu;
        if (s_use_cpu) {
            const final = s_use_cpu.locked ? s_use_cpu.value : getStoredBool("use_cpu");
            setUseCpu(final);
        }

        const s_scaling_dpi = serverSettings.scaling_dpi;
        if (s_scaling_dpi) {
            const stored = getStoredInt("scaling_dpi");
            // Precedence: explicit client (stored) > explicit server (overridden) > local-scaling
            // default. A non-overridden server value is the built-in 96, which the default replaces.
            const final = s_scaling_dpi.allowed.includes(String(stored)) ? stored
                : (s_scaling_dpi.overridden ? parseInt(s_scaling_dpi.value, 10) : deriveDpiFromDpr());
            setSelectedDpi(final);
        }

        // Anti-aliasing and Browser Cursors settings
        const s_use_browser_cursors = serverSettings.use_browser_cursors;
        if (s_use_browser_cursors) {
            const final = s_use_browser_cursors.locked ? s_use_browser_cursors.value : getStoredBool("use_browser_cursors");
            setUseBrowserCursors(final);
        }
        const s_force_aligned = serverSettings.force_aligned_resolution;
        if (s_force_aligned) {
            setForceAlignedResolution(s_force_aligned.locked ? s_force_aligned.value : getStoredBool("force_aligned_resolution"));
        }
    }, [serverSettings, streamMode]);

    // Audio device population
    useEffect(() => {
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
                const inputs = [];
                const outputs = [];

                devices.forEach((device, index) => {
                    if (!device.deviceId) return;
                    const label = device.label || `Device ${index + 1}`;

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
                console.error('Error getting media devices:', err);
                setAudioDeviceError(err.message || 'Failed to load audio devices');
            } finally {
                setIsLoadingAudioDevices(false);
            }
        };

        populateAudioDevices();
    }, []);

    // Screen Settings Handlers
    const handleManualWidthChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        const value = event.target.value;
        setManualWidth(value);
        setPresetValue("");
        localStorage.setItem(getPrefixedKey('manual_width'), value);
    };

    const handleManualHeightChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        const value = event.target.value;
        setManualHeight(value);
        setPresetValue("");
        localStorage.setItem(getPrefixedKey('manual_height'), value);
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
            const response = await fetch(`${getRoutePrefix()}/api/switch`, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                credentials: "same-origin",
                body: JSON.stringify({ mode }),
            });
            if (!response.ok) {
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
        if (!isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readStored)) {
            const rcResolved = resolveSpec(
                RATE_CONTROL_SPEC, serverSettings,
                { ...conditionalCtx, activeEncoder: selectedEncoder }, readStored);
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
        // video_bitrate is Mbps on the wire; the slider works in Mbps.
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
        const newFullColorState = !videoFullColor;
        setVideoFullColor(newFullColorState);
        localStorage.setItem(getPrefixedKey('video_fullcolor'), newFullColorState.toString());
        debouncedPostSetting({ video_fullcolor: newFullColorState });
    };

    const handleH264StreamingModeToggle = () => {
        const newStreamingModeState = !videoStreamingMode;
        setVideoStreamingMode(newStreamingModeState);
        localStorage.setItem(getPrefixedKey('video_streaming_mode'), newStreamingModeState.toString());
        debouncedPostSetting({ video_streaming_mode: newStreamingModeState });
    };

    const handleUsePaintOverQualityToggle = () => {
        const newUsePaintOverQualityState = !usePaintOverQuality;
        setUsePaintOverQuality(newUsePaintOverQualityState);
        localStorage.setItem(getPrefixedKey('use_paint_over_quality'), newUsePaintOverQualityState.toString());
        debouncedPostSetting({ use_paint_over_quality: newUsePaintOverQualityState });
    };

    const handleUseCpuToggle = () => {
        const newUseCpuState = !useCpu;
        setUseCpu(newUseCpuState);
        localStorage.setItem(getPrefixedKey('use_cpu'), newUseCpuState.toString());
        debouncedPostSetting({ use_cpu: newUseCpuState });
    };

    // Anti-aliasing and Browser Cursors Handlers
    const handleAntiAliasingToggle = () => {
        const newState = !antiAliasing;
        setAntiAliasing(newState);
        // Core persists antiAliasingEnabled itself when handling the message.
        window.postMessage(
            { type: 'setAntiAliasing', value: newState },
            window.location.origin
        );
    };

    const handleUseBrowserCursorsToggle = () => {
        const newState = !useBrowserCursors;
        setUseBrowserCursors(newState);
        // Core persists use_browser_cursors itself when handling the message.
        window.postMessage(
            { type: 'setUseBrowserCursors', value: newState },
            window.location.origin
        );
    };

    const handleForceAlignedResolutionToggle = () => {
        const newState = !forceAlignedResolution;
        setForceAlignedResolution(newState);
        localStorage.setItem(getPrefixedKey('force_aligned_resolution'), newState.toString());
        debouncedPostSetting({ force_aligned_resolution: newState });
    };

    // Manual/preset resolutions pair with CSS scaling: HiDPI off when one is
    // set, on when reset — a derived write (not pinned), through the uniform
    // path. A server lock always wins, so skip then.
    const deriveHidpiForResolution = (manual: boolean) => {
        if (serverSettings?.use_css_scaling?.locked) return;
        writeConditional(HIDPI_SPEC, !manual, setHidpiEnabled, { persist: false });
    };

    const handleSetManualResolution = () => {
        const widthVal = manualWidth.trim();
        const heightVal = manualHeight.trim();
        const width = parseInt(widthVal, 10);
        const height = parseInt(heightVal, 10);

        if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
            alert("Invalid resolution");
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
    };

    // CBR stops: sub-Mbps steps for constrained links, then whole Mbps.
    const videoBitrateOptions = (() => {
        const min = serverSettings?.video_bitrate?.min ?? 0.1;
        const max = serverSettings?.video_bitrate?.max ?? 100;
        const stops = SUB_MBPS_BITRATE_STEPS.filter(v => v >= min && v <= max);
        for (let v = Math.max(1, Math.ceil(min)); v <= Math.floor(max); v++) stops.push(v);
        return stops.length ? stops : [min];
    })();
    const bitrateIndex = (() => {
        const exact = videoBitrateOptions.indexOf(videoBitRate);
        if (exact >= 0) return exact;
        const above = videoBitrateOptions.findIndex(v => v >= videoBitRate);
        return above >= 0 ? above : videoBitrateOptions.length - 1;
    })();
    const formatBitrate = (v: number) => v < 1 ? `${Math.round(v * 1000)} Kbps` : `${v} Mbps`;

    // --- Render Gating ---
    const activeEncoder = isWebrtc ? encoderRTC : encoder;
    const isH264 = H264_ENCODERS.includes(activeEncoder);
    const showJpegOptions = !isWebrtc && activeEncoder === 'jpeg';
    const showRateControl = (renderableSettings.enableRateControl ?? true) && isH264;
    const encoderRenderable = isWebrtc
        ? (renderableSettings.encoderRtc ?? true)
        : (renderableSettings.encoder ?? true);

    const showVideoTab = renderableSettings.videoSettings !== false;
    const showAudioTab = renderableSettings.audioSettings !== false;
    const showResolutionTab = renderableSettings.screenSettings !== false;
    const visibleTabCount = [showVideoTab, showAudioTab, showResolutionTab].filter(Boolean).length;

    if (visibleTabCount === 0) {
        return null;
    }

    return (
        <Card className="w-[300px] p-0 pb-4 bg-background/95 backdrop-blur-sm border shadow-sm">
            <Tabs defaultValue={showVideoTab ? "video" : showAudioTab ? "audio" : "resolution"} className="w-full">
                <TabsList className={`grid w-full bg-muted/50 ${visibleTabCount === 3 ? 'grid-cols-3' : visibleTabCount === 2 ? 'grid-cols-2' : 'grid-cols-1'}`}>
                    {showVideoTab && <TabsTrigger value="video">Video</TabsTrigger>}
                    {showAudioTab && <TabsTrigger value="audio">Audio</TabsTrigger>}
                    {showResolutionTab && <TabsTrigger value="resolution">Resolution</TabsTrigger>}
                </TabsList>

                {showResolutionTab && (
                <TabsContent value="resolution">
                    <CardContent className="space-y-4">
                        {!isSecondaryDisplay && (
                            <>
                                {(renderableSettings.hidpi ?? true) && (
                                    <div className="flex items-center justify-between">
                                        <div className="space-y-0.5">
                                            <label className="text-sm font-medium">HiDPI (Pixel Perfect)</label>
                                        </div>
                                        <Switch
                                            checked={hidpiEnabled}
                                            onCheckedChange={handleHidpiToggle}
                                        />
                                    </div>
                                )}

                                {/* Anti-aliasing Toggle */}
                                <div className="flex items-center justify-between">
                                    <div className="space-y-0.5">
                                        <label className="text-sm font-medium">Anti-aliasing</label>
                                    </div>
                                    <Switch
                                        checked={antiAliasing}
                                        onCheckedChange={handleAntiAliasingToggle}
                                    />
                                </div>

                                {(renderableSettings.forceAlignedResolution ?? true) && (
                                    <div className="flex items-center justify-between">
                                        <div className="space-y-0.5">
                                            <label className="text-sm font-medium">Force Aligned Resolution</label>
                                        </div>
                                        <Switch
                                            checked={forceAlignedResolution}
                                            onCheckedChange={handleForceAlignedResolutionToggle}
                                        />
                                    </div>
                                )}

                                {(renderableSettings.useBrowserCursors ?? true) && (
                                    <div className="flex items-center justify-between">
                                        <div className="space-y-0.5">
                                            <label className="text-sm font-medium">Use CSS Cursors</label>
                                        </div>
                                        <Switch
                                            checked={useBrowserCursors}
                                            onCheckedChange={handleUseBrowserCursorsToggle}
                                        />
                                    </div>
                                )}

                                {(renderableSettings.uiScaling ?? true) && (
                                    <div className="space-y-2">
                                        <label className="text-sm font-medium">UI Scaling</label>
                                        <DropdownMenu>
                                            <DropdownMenuTrigger asChild>
                                                <Button variant="outline" className="w-full justify-between">
                                                    {dpiScalingOptions.find(option => option.value === selectedDpi)?.label || "100%"}
                                                    <ChevronUp className="h-4 w-4 rotate-180" />
                                                </Button>
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent className="w-full">
                                                {dpiScalingOptions.map((option) => (
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
                                    <label className="text-sm font-medium">Resolution Preset</label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger asChild>
                                            <Button variant="outline" className="w-full justify-between">
                                                {presetValue || "-- Select Preset --"}
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
                                        <label className="text-sm font-medium">Width</label>
                                        <Input
                                            type="number"
                                            value={manualWidth}
                                            onChange={handleManualWidthChange}
                                            placeholder="Width"
                                            min="1"
                                            step="2"
                                            className="[appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                                        />
                                    </div>
                                    <div className="flex-1 space-y-2">
                                        <label className="text-sm font-medium">Height</label>
                                        <Input
                                            type="number"
                                            value={manualHeight}
                                            onChange={handleManualHeightChange}
                                            placeholder="Height"
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
                                        Set
                                    </Button>
                                    <Button
                                        variant="outline"
                                        className="flex-1"
                                        onClick={handleResetResolution}
                                    >
                                        Reset
                                    </Button>
                                </div>
                            </>
                        )}

                        <Button
                            variant={scaleLocally ? "default" : "outline"}
                            className="w-full"
                            onClick={handleScaleLocallyToggle}
                        >
                            Scale Locally: {scaleLocally ? "On" : "Off"}
                        </Button>
                    </CardContent>
                </TabsContent>
                )}

                {showVideoTab && (
                <TabsContent value="video">
                    <CardContent className="space-y-4">
                        {(renderableSettings.enableDualMode ?? (window as any).__SELKIES_DUAL_MODE__ ?? false) && (
                            <div className="space-y-2">
                                <label className="text-sm font-medium">Streaming Mode</label>
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
                                <label className="text-sm font-medium">Encoder</label>
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
                                <label className="text-sm font-medium">Frames per second ({framerate} FPS)</label>
                                <div className="flex items-center gap-2">
                                    <Slider
                                        min={0}
                                        max={framerateOptions.length - 1}
                                        step={1}
                                        value={[framerateOptions.indexOf(framerate)]}
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
                                    <label className="text-sm font-medium">Rate Control</label>
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

                                {rateControlMode === 'cbr' && (renderableSettings.videoBitrate ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">Video Bitrate ({formatBitrate(videoBitRate)})</label>
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

                                {rateControlMode === 'crf' && (renderableSettings.videoCRF ?? true) && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">Video CRF ({videoCRF})</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={0}
                                            max={videoCRFOptions.length - 1}
                                            step={1}
                                            value={[videoCRFOptions.indexOf(videoCRF)]}
                                            onValueChange={(value) => {
                                                const index = value[0];
                                                const newCRF = videoCRFOptions[index];
                                                handleVideoCRFChange(newCRF);
                                            }}
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
                                        <label className="text-sm font-medium">Full Color (4:4:4)</label>
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
                                        <label className="text-sm font-medium">Turbo Mode</label>
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
                                <label className="text-sm font-medium">JPEG Quality ({jpegQuality})</label>
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
                                    <label className="text-sm font-medium">Use Paint-Over Quality</label>
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
                                    <label className="text-sm font-medium">Paint-over CRF ({videoPaintoverCRF})</label>
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
                                    <label className="text-sm font-medium">Paint-over Burst Frames ({videoPaintoverBurstFrames})</label>
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
                                <label className="text-sm font-medium">Paint-over JPEG Quality ({paintOverJpegQuality})</label>
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

                        {/* No toggle for openh264enc (server forces use_cpu=True). h264enc
                            takes it in both transports; striped/jpeg are WS-only. */}
                        {(activeEncoder === 'h264enc' || (!isWebrtc && ['h264enc-striped', 'jpeg'].includes(activeEncoder))) && (renderableSettings.useCpu ?? true) && (
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <label className="text-sm font-medium">CPU Encoding</label>
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
                            <label className="text-sm font-medium">Audio Bitrate ({audioBitRate / 1000} kbps)</label>
                            <div className="flex items-center gap-2">
                                <Slider
                                    min={0}
                                    max={audioBitrateOptions.length - 1}
                                    step={1}
                                    value={[audioBitrateOptions.indexOf(audioBitRate)]}
                                    onValueChange={(value) => {
                                        const index = value[0];
                                        const selectedBitrate = audioBitrateOptions[index];
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
                            <label className="text-sm font-medium">Audio Input Device</label>
                            <DropdownMenu>
                                <DropdownMenuTrigger asChild>
                                    <Button variant="outline" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError}>
                                        <span className="truncate">
                                            {audioInputDevices.find(d => d.deviceId === selectedInputDeviceId)?.label || 'Default'}
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
                                <label className="text-sm font-medium">Audio Output Device</label>
                                <DropdownMenu>
                                    <DropdownMenuTrigger asChild>
                                        <Button variant="outline" className="w-full justify-between" disabled={isLoadingAudioDevices || !!audioDeviceError}>
                                            <span className="truncate">
                                                {audioOutputDevices.find(d => d.deviceId === selectedOutputDeviceId)?.label || 'Default'}
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
                            <p className="text-sm text-muted-foreground">Audio output selection is not supported</p>
                        )}
                    </CardContent>
                </TabsContent>
                )}
            </Tabs>
        </Card>
    );
}
