/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Card, CardContent } from "@/components/ui/card";
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

// --- Multi-display Constants ---
const urlHash = window.location.hash;
const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';

const PER_DISPLAY_SETTINGS = [
    'videoBitRate', 'videoFramerate', 'videoCRF', 'video_fullcolor',
    'video_streaming_mode', 'jpegQuality', 'paintOverJpegQuality', 'useCpu',
    'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
    'resizeRemote', 'isManualResolutionMode', 'manualWidth', 'manualHeight',
    'encoder', 'scaleLocallyManual', 'rateControlMode'
];

// --- Storage Key Prefixing ---
const getStorageAppName = () => {
    if (typeof window === 'undefined') return '';
    // Origin + pathname only (NOT the full URL): a per-session ?token=... must not mint
    // a new localStorage namespace each connect. Must match the cores' derivation.
    const urlForKey = window.location.origin + window.location.pathname;
    // Must match the streaming cores' prefix sanitizer ([._-] literal class, not
    // a [.-_] range) so dashboard and cores share one storage prefix.
    return urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
};
const storageAppName = getStorageAppName();
const getPrefixedKey = (key: string) => {
    const prefixedKey = `${storageAppName}_${key}`;
    if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
        return `${prefixedKey}_display2`;
    }
    return prefixedKey;
};

// Constants
const audioBitrateOptions = [32000, 64000, 96000, 128000, 192000, 256000, 320000, 512000];
const DEFAULT_AUDIO_BITRATE = 320000;

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

// Every H.264 encoder supports both CBR and CRF (constant-QP) rate control.
const H264_ENCODERS = ["h264enc", "h264enc-striped", "openh264enc", "nvh264enc"];

const framerateOptions = [8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144];

const videoCRFOptions = [50, 45, 40, 35, 30, 25, 20, 10, 1];

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
    // --- Server Settings ---
    const [serverSettings, setServerSettings] = useState<any>(null);
    const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState(encoderOptions);

    // Screen Settings State (with proper localStorage keys)
    const [manualWidth, setManualWidth] = useState(() =>
        localStorage.getItem(getPrefixedKey("manualWidth")) || ''
    );
    const [manualHeight, setManualHeight] = useState(() =>
        localStorage.getItem(getPrefixedKey("manualHeight")) || ''
    );
    const [presetValue, setPresetValue] = useState("");
    const [scaleLocally, setScaleLocally] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("scaleLocallyManual"));
        return saved !== null ? saved === 'true' : true;
    });

    // HiDPI and UI Scaling State (with proper localStorage keys)
    const [hidpiEnabled, setHidpiEnabled] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("useCssScaling"));
        return saved !== "true";
    });
    const [selectedDpi, setSelectedDpi] = useState(() => {
        return parseInt(localStorage.getItem(getPrefixedKey("SCALING_DPI")), 10) || DEFAULT_SCALING_DPI;
    });

    // Video and Audio Settings State (with proper localStorage keys)
    const [videoBitRate, setVideoBitRate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("videoBitRate")), 10) || 8
    );
    const [audioBitRate, setAudioBitRate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("audioBitRate")), 10) || DEFAULT_AUDIO_BITRATE
    );
    const [videoBufferSize, setVideoBufferSize] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("videoBufferSize")), 10) || 0
    );
    const [encoder, setEncoder] = useState(() =>
        localStorage.getItem(getPrefixedKey("encoder")) || "h264enc"
    );
    const [framerate, setFramerate] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("videoFramerate")), 10) || 60
    );
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("videoCRF"));
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    const [rateControlMode, setRateControlMode] = useState(() =>
        localStorage.getItem(getPrefixedKey("rateControlMode")) || "crf"
    );
    const [videoFullColor, setVideoFullColor] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_fullcolor"));
        return saved !== null ? saved === 'true' : false;
    });
    const [videoStreamingMode, setVideoStreamingMode] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("video_streaming_mode"));
        return saved !== null ? saved === 'true' : false;
    });
    const [jpegQuality, setJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("jpegQuality")), 10) || 60
    );
    const [paintOverJpegQuality, setPaintOverJpegQuality] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("paintOverJpegQuality")), 10) || 90
    );
    const [videoPaintoverCRF, setVideoPaintoverCRF] = useState(() =>
        parseInt(localStorage.getItem(getPrefixedKey("video_paintover_crf")), 10) || 18
    );
    const [usePaintOverQuality, setUsePaintOverQuality] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("use_paint_over_quality"));
        return saved !== null ? saved === 'true' : true;
    });
    const [useCpu, setUseCpu] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("useCpu"));
        return saved !== null ? saved === 'true' : false;
    });

    // Anti-aliasing and Browser Cursors State (with proper localStorage keys)
    const [antiAliasing, setAntiAliasing] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
        return saved !== null ? saved === "true" : true;
    });
    const [useBrowserCursors, setUseBrowserCursors] = useState(() => {
        const saved = localStorage.getItem(getPrefixedKey("useBrowserCursors"));
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

    // --- Server Settings Message Listener ---
    useEffect(() => {
        const handleMessage = (event: MessageEvent) => {
            if (
                event.origin === window.location.origin &&
                event.data?.type === "serverSettings"
            ) {
                console.log("Settings received server settings:", event.data.payload);
                setServerSettings(event.data.payload);
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

        // Update encoder options from server
        const s_encoder = serverSettings.encoder;
        if (s_encoder) {
            const stored = localStorage.getItem(getPrefixedKey("encoder"));
            const final = s_encoder.allowed.includes(stored) ? stored : s_encoder.value;
            setEncoder(final);
            setDynamicEncoderOptions(s_encoder.allowed);
        }

        // Sync the CBR/CRF mode from the server so the UI shows the correct mode + slider on load.
        const s_rate_control_mode = serverSettings.rate_control_mode;
        if (s_rate_control_mode) {
            const stored = localStorage.getItem(getPrefixedKey("rateControlMode"));
            const allowed = s_rate_control_mode.allowed || ["crf", "cbr"];
            const final = allowed.includes(stored) ? stored : s_rate_control_mode.value;
            setRateControlMode(final);
        }

        // Update framerate from server constraints
        const s_framerate = serverSettings.framerate;
        if (s_framerate) {
            const stored = getStoredInt("framerate");
            const final = !isNaN(stored)
                ? Math.max(s_framerate.min, Math.min(s_framerate.max, stored))
                : s_framerate.default;
            setFramerate(final);
        }

        // Clamp the CBR bitrate (Mbps) to the server-pushed range
        const s_video_bitrate = serverSettings.video_bitrate;
        if (s_video_bitrate) {
            const stored = getStoredInt("video_bitrate");
            const final = !isNaN(stored)
                ? Math.max(s_video_bitrate.min, Math.min(s_video_bitrate.max, stored))
                : s_video_bitrate.default;
            setVideoBitRate(final);
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
            const final = s_scaling_dpi.allowed.includes(String(stored)) ? stored : parseInt(s_scaling_dpi.value, 10);
            setSelectedDpi(final);
        }

        // Anti-aliasing and Browser Cursors settings
        const s_use_browser_cursors = serverSettings.use_browser_cursors;
        if (s_use_browser_cursors) {
            const final = s_use_browser_cursors.locked ? s_use_browser_cursors.value : getStoredBool("use_browser_cursors");
            setUseBrowserCursors(final);
        }
    }, [serverSettings]);

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
        localStorage.setItem(getPrefixedKey('manualWidth'), value);
    };

    const handleManualHeightChange = (event: React.ChangeEvent<HTMLInputElement>) => {
        const value = event.target.value;
        setManualHeight(value);
        setPresetValue("");
        localStorage.setItem(getPrefixedKey('manualHeight'), value);
    };

    const handleScaleLocallyToggle = () => {
        const newState = !scaleLocally;
        setScaleLocally(newState);
        localStorage.setItem(getPrefixedKey('scaleLocallyManual'), newState.toString());
        window.postMessage({ type: 'setScaleLocally', value: newState }, window.location.origin);
    };

    // HiDPI and UI Scaling Handlers
    const handleHidpiToggle = () => {
        const newHidpiState = !hidpiEnabled;
        setHidpiEnabled(newHidpiState);
        localStorage.setItem(getPrefixedKey('useCssScaling'), (!newHidpiState).toString());
        window.postMessage(
            { type: 'setUseCssScaling', value: !newHidpiState },
            window.location.origin
        );
    };

    const handleDpiScalingChange = (value: string) => {
        const newDpi = parseInt(value, 10);
        setSelectedDpi(newDpi);
        localStorage.setItem(getPrefixedKey('SCALING_DPI'), newDpi.toString());
        debouncedPostSetting({ scaling_dpi: newDpi });
    };

    // Video Settings Handlers
    const handleEncoderChange = (selectedEncoder: string) => {
        setEncoder(selectedEncoder);
        localStorage.setItem(getPrefixedKey('encoder'), selectedEncoder);
        debouncedPostSetting({ encoder: selectedEncoder });
    };

    const handleFramerateChange = (selectedFramerate: number) => {
        setFramerate(selectedFramerate);
        localStorage.setItem(getPrefixedKey('videoFramerate'), selectedFramerate.toString());
        debouncedPostSetting({ framerate: selectedFramerate });
    };

    const handleVideoCRFChange = (selectedCRF: number) => {
        setVideoCRF(selectedCRF);
        localStorage.setItem(getPrefixedKey('videoCRF'), selectedCRF.toString());
        debouncedPostSetting({ video_crf: selectedCRF });
    };

    const handleRateControlChange = (mode: string) => {
        setRateControlMode(mode);
        localStorage.setItem(getPrefixedKey('rateControlMode'), mode);
        debouncedPostSetting({ rate_control_mode: mode });
    };

    const handleVideoBitRateChange = (selectedBitRate: number) => {
        setVideoBitRate(selectedBitRate);
        localStorage.setItem(getPrefixedKey('videoBitRate'), selectedBitRate.toString());
        // video_bitrate is Mbps on the wire; the slider works in Mbps.
        debouncedPostSetting({ video_bitrate: selectedBitRate });
    };

    const handleJpegQualityChange = (selectedQuality: number) => {
        setJpegQuality(selectedQuality);
        localStorage.setItem(getPrefixedKey('jpegQuality'), selectedQuality.toString());
        debouncedPostSetting({ jpeg_quality: selectedQuality });
    };

    const handlePaintOverJpegQualityChange = (selectedQuality: number) => {
        setPaintOverJpegQuality(selectedQuality);
        localStorage.setItem(getPrefixedKey('paintOverJpegQuality'), selectedQuality.toString());
        debouncedPostSetting({ paint_over_jpeg_quality: selectedQuality });
    };

    const handleH264PaintoverCRFChange = (selectedCRF: number) => {
        setVideoPaintoverCRF(selectedCRF);
        localStorage.setItem(getPrefixedKey('video_paintover_crf'), selectedCRF.toString());
        debouncedPostSetting({ video_paintover_crf: selectedCRF });
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
        localStorage.setItem(getPrefixedKey('useCpu'), newUseCpuState.toString());
        debouncedPostSetting({ use_cpu: newUseCpuState });
    };

    // Anti-aliasing and Browser Cursors Handlers
    const handleAntiAliasingToggle = () => {
        const newState = !antiAliasing;
        setAntiAliasing(newState);
        localStorage.setItem(getPrefixedKey('antiAliasingEnabled'), newState.toString());
        window.postMessage(
            { type: 'setAntiAliasing', value: newState },
            window.location.origin
        );
    };

    const handleUseBrowserCursorsToggle = () => {
        const newState = !useBrowserCursors;
        setUseBrowserCursors(newState);
        localStorage.setItem(getPrefixedKey('useBrowserCursors'), newState.toString());
        window.postMessage(
            { type: 'setUseBrowserCursors', value: newState },
            window.location.origin
        );
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
        window.postMessage({ type: 'setManualResolution', width: evenWidth, height: evenHeight }, window.location.origin);
    };

    const handleResetResolution = () => {
        setManualWidth('');
        setManualHeight('');
        setPresetValue("");
        window.postMessage({ type: 'resetResolutionToWindow' }, window.location.origin);
    };

    return (
        <Card className="w-[300px] p-0 pb-4 bg-background/95 backdrop-blur-sm border shadow-sm">
            <Tabs defaultValue="video" className="w-full">
                <TabsList className="grid w-full grid-cols-3 bg-muted/50">
                    <TabsTrigger value="video">Video</TabsTrigger>
                    <TabsTrigger value="audio">Audio</TabsTrigger>
                    <TabsTrigger value="resolution">Resolution</TabsTrigger>
                </TabsList>

                <TabsContent value="resolution">
                    <CardContent className="space-y-4">
                        {/* HiDPI Toggle */}
                        <div className="flex items-center justify-between">
                            <div className="space-y-0.5">
                                <label className="text-sm font-medium">HiDPI (Pixel Perfect)</label>
                            </div>
                            <Switch
                                checked={hidpiEnabled}
                                onCheckedChange={handleHidpiToggle}
                            />
                        </div>

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

                        {/* Use CSS Cursors Toggle */}
                        <div className="flex items-center justify-between">
                            <div className="space-y-0.5">
                                <label className="text-sm font-medium">Use CSS Cursors</label>
                            </div>
                            <Switch
                                checked={useBrowserCursors}
                                onCheckedChange={handleUseBrowserCursorsToggle}
                            />
                        </div>

                        {/* UI Scaling */}
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

                                                        window.postMessage({ type: 'setManualResolution', width: evenWidth, height: evenHeight }, window.location.origin);
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

                        <Button
                            variant={scaleLocally ? "default" : "outline"}
                            className="w-full"
                            onClick={handleScaleLocallyToggle}
                        >
                            Scale Locally: {scaleLocally ? "On" : "Off"}
                        </Button>
                    </CardContent>
                </TabsContent>

                <TabsContent value="video">
                    <CardContent className="space-y-4">
                        <div className="space-y-2">
                            <label className="text-sm font-medium">Encoder</label>
                            <DropdownMenu>
                                <DropdownMenuTrigger asChild>
                                    <Button variant="outline" className="w-full justify-between">
                                        {encoder}
                                        <ChevronUp className="h-4 w-4 rotate-180" />
                                    </Button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent className="w-full">
                                    {dynamicEncoderOptions.map(enc => (
                                        <DropdownMenuItem
                                            key={enc}
                                            onClick={() => handleEncoderChange(enc)}
                                        >
                                            {enc}
                                        </DropdownMenuItem>
                                    ))}
                                </DropdownMenuContent>
                            </DropdownMenu>
                        </div>

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


                        {H264_ENCODERS.includes(encoder) && (
                            <>
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">Rate Control</label>
                                    <DropdownMenu>
                                        <DropdownMenuTrigger asChild>
                                            <Button variant="outline" className="w-full justify-between">
                                                {rateControlMode.toUpperCase()}
                                                <ChevronUp className="h-4 w-4 rotate-180" />
                                            </Button>
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent className="w-full">
                                            {["crf", "cbr"].map((mode) => (
                                                <DropdownMenuItem key={mode} onClick={() => handleRateControlChange(mode)}>
                                                    {mode.toUpperCase()}
                                                </DropdownMenuItem>
                                            ))}
                                        </DropdownMenuContent>
                                    </DropdownMenu>
                                </div>

                                {rateControlMode === 'cbr' && (
                                <div className="space-y-2">
                                    <label className="text-sm font-medium">Video Bitrate ({videoBitRate} Mbps)</label>
                                    <div className="flex items-center gap-2">
                                        <Slider
                                            min={serverSettings?.video_bitrate?.min || 1}
                                            max={serverSettings?.video_bitrate?.max || 100}
                                            step={1}
                                            value={[videoBitRate]}
                                            onValueChange={(value) => handleVideoBitRateChange(value[0])}
                                            disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>
                                )}

                                {rateControlMode === 'crf' && (
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
                            </>
                        )}

                        {encoder === 'jpeg' && (
                            <>
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
                            </>
                        )}

                        {/* Server honors paint-over quality for every H.264 encoder and jpeg. */}
                        {(H264_ENCODERS.includes(encoder) || encoder === 'jpeg') && (
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

                        {/* No toggle for openh264enc: the server forces use_cpu=True for it. */}
                        {(encoder === 'h264enc' || encoder === 'h264enc-striped' || encoder === 'jpeg') && (
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

                <TabsContent value="audio">
                    <CardContent className="space-y-4">
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
                                            localStorage.setItem(getPrefixedKey('audioBitRate'), selectedBitrate.toString());
                                            debouncedPostSetting({ audioBitRate: selectedBitrate });
                                        }
                                    }}
                                    className="flex-1"
                                />
                            </div>
                        </div>

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
            </Tabs>
        </Card>
    );
} 