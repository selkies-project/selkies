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
import React, { useState, useEffect } from "react";

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
  "x264enc",
  "x264enc-striped",
  "jpeg",
];

const framerateOptions = [8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144];

const videoCRFOptions = [50, 45, 40, 35, 30, 25, 20, 10, 1];

const roundDownToEven = (num: number) => {
    const n = parseInt(num.toString(), 10);
    if (isNaN(n)) return 0;
    return Math.floor(n / 2) * 2;
};

interface SettingsProps {
    scale?: number;
}

export function Settings() {
    // Screen Settings State
    const [manualWidth, setManualWidth] = useState('');
    const [manualHeight, setManualHeight] = useState('');
    const [presetValue, setPresetValue] = useState("");
    const [scaleLocally, setScaleLocally] = useState(() => {
        const saved = localStorage.getItem('scaleLocallyManual');
        return saved !== null ? saved === 'true' : true;
    });

    // HiDPI and UI Scaling State
    const [hidpiEnabled, setHidpiEnabled] = useState(() => {
        const saved = localStorage.getItem('hidpiEnabled');
        return saved !== null ? saved === 'true' : true;
    });
    const [selectedDpi, setSelectedDpi] = useState(() => {
        return parseInt(localStorage.getItem('scalingDPI'), 10) || DEFAULT_SCALING_DPI;
    });

    // Video and Audio Settings State
    const [videoBitRate, setVideoBitRate] = useState(4000);
    const [audioBitRate, setAudioBitRate] = useState(parseInt(localStorage.getItem('audioBitRate'), 10) || DEFAULT_AUDIO_BITRATE);
    const [videoBufferSize, setVideoBufferSize] = useState(0);
    const [encoder, setEncoder] = useState("x264enc");
    const [framerate, setFramerate] = useState(60);
    const [videoCRF, setVideoCRF] = useState(() => {
        const saved = localStorage.getItem('videoCRF');
        return saved !== null ? parseInt(saved, 10) : 25;
    });
    const [h264FullColor, setH264FullColor] = useState(() => {
        const saved = localStorage.getItem('h264_fullcolor');
        return saved !== null ? saved === 'true' : false;
    });
    const [audioInputDevices, setAudioInputDevices] = useState([]);
    const [audioOutputDevices, setAudioOutputDevices] = useState([]);
    const [selectedInputDeviceId, setSelectedInputDeviceId] = useState('default');
    const [selectedOutputDeviceId, setSelectedOutputDeviceId] = useState('default');
    const [isOutputSelectionSupported, setIsOutputSelectionSupported] = useState(false);
    const [audioDeviceError, setAudioDeviceError] = useState(null);
    const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);

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
        localStorage.setItem('scaleLocallyManual', newState.toString());
        window.postMessage({ type: 'setScaleLocally', value: newState }, window.location.origin);
    };

    // HiDPI and UI Scaling Handlers
    const handleHidpiToggle = () => {
        const newHidpiState = !hidpiEnabled;
        setHidpiEnabled(newHidpiState);
        localStorage.setItem('hidpiEnabled', newHidpiState.toString());
        window.postMessage(
            { type: 'setUseCssScaling', value: !newHidpiState },
            window.location.origin
        );
    };

    const handleDpiScalingChange = (value: string) => {
        const newDpi = parseInt(value, 10);
        setSelectedDpi(newDpi);
        localStorage.setItem('scalingDPI', newDpi.toString());
        window.postMessage(
            { type: 'settings', settings: { SCALING_DPI: newDpi } },
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
                                    {encoderOptions.map(enc => (
                                        <DropdownMenuItem
                                            key={enc}
                                            onClick={() => {
                                                setEncoder(enc);
                                                localStorage.setItem('encoder', enc);
                                                window.postMessage({ type: 'settings', settings: { encoder: enc } }, window.location.origin);
                                            }}
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
                                            setFramerate(selectedFramerate);
                                            localStorage.setItem('videoFramerate', selectedFramerate.toString());
                                            window.postMessage({ type: 'settings', settings: { videoFramerate: selectedFramerate } }, window.location.origin);
                                        }
                                    }}
                                    className="flex-1"
                                />
                            </div>
                        </div>


                        {(encoder === 'x264enc' || encoder === 'x264enc-striped') && (
                            <>
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
                                                setVideoCRF(newCRF);
                                                localStorage.setItem('videoCRF', newCRF.toString());
                                                window.postMessage({ type: 'settings', settings: { videoCRF: newCRF } }, window.location.origin);
                                            }}
                                            className="flex-1"
                                        />
                                    </div>
                                </div>

                                <div className="flex items-center justify-between">
                                    <div className="space-y-0.5">
                                        <label className="text-sm font-medium">Full Color (4:4:4)</label>
                                    </div>
                                    <Switch
                                        checked={h264FullColor}
                                        onCheckedChange={(checked) => {
                                            setH264FullColor(checked);
                                            localStorage.setItem('h264_fullcolor', checked.toString());
                                            window.postMessage({ type: 'settings', settings: { h264_fullcolor: checked } }, window.location.origin);
                                        }}
                                    />
                                </div>
                            </>
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
                                            localStorage.setItem('audioBitRate', selectedBitrate.toString());
                                            window.postMessage({ type: 'settings', settings: { audioBitRate: selectedBitrate } }, window.location.origin);
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
                                        {audioInputDevices.find(d => d.deviceId === selectedInputDeviceId)?.label || 'Default'}
                                        <ChevronUp className="h-4 w-4 rotate-180" />
                                    </Button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent className="w-full">
                                    {audioInputDevices.map(device => (
                                        <DropdownMenuItem
                                            key={device.deviceId}
                                            onClick={() => {
                                                setSelectedInputDeviceId(device.deviceId);
                                                window.postMessage({ type: 'audioDeviceSelected', context: 'input', deviceId: device.deviceId }, window.location.origin);
                                            }}
                                        >
                                            {device.label}
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
                                            {audioOutputDevices.find(d => d.deviceId === selectedOutputDeviceId)?.label || 'Default'}
                                            <ChevronUp className="h-4 w-4 rotate-180" />
                                        </Button>
                                    </DropdownMenuTrigger>
                                    <DropdownMenuContent className="w-full">
                                        {audioOutputDevices.map(device => (
                                            <DropdownMenuItem
                                                key={device.deviceId}
                                                onClick={() => {
                                                    setSelectedOutputDeviceId(device.deviceId);
                                                    window.postMessage({ type: 'audioDeviceSelected', context: 'output', deviceId: device.deviceId }, window.location.origin);
                                                }}
                                            >
                                                {device.label}
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