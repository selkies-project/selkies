/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The video, audio, and screen settings as panels: each reads the one
 * `useStreamSettings` instance `state/settings-context` holds, so a layout paints them
 * where it likes (three sections of a sidebar, three tabs of a card) and a
 * switch between layouts keeps every value and every pending push. A control
 * renders only while the server's constraints leave something to change
 * (`show`), and a panel none of whose controls is shown renders nothing.
 * @module
 */
import * as React from "react";
import { Button } from "@/components/ui/button";
import { hint } from "@/utils";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Field, NativeSelect, RangeSlider, SwitchRow, Clip } from "@/panels/controls";
import { useDashboard } from "@/state/dashboard";
import { RESOLUTION_PRESETS, STREAMING_MODES, WEBCAM_ENCODER_OPTIONS, canPlayEncoder, displayLabel, framerateLabel } from "@/state/settings";
import { useSettings, useSettingsPanels } from "@/state/settings-context";
import { t, tl } from "@/i18n";

/** Enlarges a native range on coarse pointers, where a finger is the thumb. */
const TOUCH_RANGE = "pointer-coarse:h-8";

/** The stream's format: transport, encoders, and the color switches. */
export function FormatSettings() {
  const s = useSettings();
  return (
    <>
      {s.show.streamMode && (
        <Field htmlFor="streamModeSelect" label={t('streamingModeTitle')}>
          <NativeSelect id="streamModeSelect" value={s.streamMode} onValueChange={s.setStreamMode}
            options={STREAMING_MODES.map((mode) => ({ value: mode, label: displayLabel(mode) }))} />
        </Field>
      )}
      {s.show.encoder && (
        <Field htmlFor="encoderSelect" label={tl('sections.video.encoderLabel')}>
          <NativeSelect id="encoderSelect" value={s.encoder} onValueChange={s.setEncoder}
            disabled={!s.serverSettings || s.encoderOptions.length <= 1}
            options={s.encoderOptions.map((enc) => ({
              value: enc,
              disabled: !canPlayEncoder(enc, s.isWebrtc),
              label: canPlayEncoder(enc, s.isWebrtc) ? displayLabel(enc) : `${displayLabel(enc)} (${tl('sections.video.encoderUnsupported')})`,
            }))} />
        </Field>
      )}
      {s.show.webcamEncoder && (
        <Field htmlFor="webcamEncoderSelect" label={tl('sections.video.webcamEncoderLabel')}>
          <NativeSelect id="webcamEncoderSelect" value={s.webcamEncoder} onValueChange={s.setWebcamEncoder}
            disabled={!s.serverSettings || s.webcamEncoderLocked}
            options={WEBCAM_ENCODER_OPTIONS.map((pref) => ({ value: pref, label: displayLabel(pref) }))} />
        </Field>
      )}
      {s.show.fullColor && (
        <SwitchRow id="videoFullColorToggle" label={t('sections.video.fullColorLabel')} checked={s.videoFullColor}
          onToggle={s.toggleFullColor} disabled={s.fullColorLocked} />
      )}
      {s.show.tenBit && (
        <SwitchRow id="video10BitToggle" label={t('sections.video.tenBitLabel')} checked={s.video10Bit}
          onToggle={s.toggle10Bit} disabled={s.tenBitLocked} />
      )}
    </>
  );
}

/** The stream's rate and quality: frame rate, rate control, the quality sliders, and the encoder switches. */
export function PerformanceSettings() {
  const s = useSettings();
  const formatBitrate = (v: number) => `${v / 1000} Mbps`;
  return (
    <>
      {s.show.framerate && (
        <Field htmlFor="framerateSlider" label={tl(s.framerateFollows ? 'sections.video.framerateDisplayLabel' : 'sections.video.framerateLabel',
          { framerate: framerateLabel(s.framerateFollows && s.displayFramerate !== null ? s.displayFramerate : s.framerate) })}>
          <RangeSlider id="framerateSlider" className={TOUCH_RANGE} min={0} max={s.framerateStops.length - 1} step={1}
            value={s.framerateIndex} onValueChange={s.setFramerateIndex} disabled={s.framerateFixed} />
        </Field>
      )}
      {s.show.rateControl && (
        <Field htmlFor="rateControlSelect" label={t('sections.video.rateControlLabel')}>
          <NativeSelect id="rateControlSelect" value={s.rateControlMode} onValueChange={s.setRateControlMode}
            disabled={s.rateControlOptions.length <= 1}
            options={s.rateControlOptions.map((mode) => ({ value: mode, label: displayLabel(mode) }))} />
        </Field>
      )}
      {s.show.bitrate && (
        <Field htmlFor="videoBitrateSlider" label={tl('sections.video.bitrateLabel', { bitrate: formatBitrate(s.videoBitRate) })}>
          <RangeSlider id="videoBitrateSlider" className={TOUCH_RANGE} min={0} max={s.videoBitrateChoices.length - 1} step={1}
            value={s.bitrateIndex} onValueChange={(index) => s.setVideoBitRate(s.videoBitrateChoices[index])} disabled={s.bitrateFixed} />
        </Field>
      )}
      {s.show.crf && (
        <Field htmlFor="videoCRFSlider" label={tl('sections.video.crfLabel', { crf: s.videoCRF })}>
          <RangeSlider id="videoCRFSlider" className={TOUCH_RANGE} min={0} max={s.videoCRFChoices.length - 1} step={1}
            value={s.crfIndex} onValueChange={(index) => s.setVideoCRF(s.videoCRFChoices[index])} disabled={s.crfFixed} />
        </Field>
      )}
      {s.show.turbo && (
        <SwitchRow id="videoStreamingModeToggle" label={t('sections.video.streamingModeLabel')} title={t('sections.video.streamingModeDetails')}
          checked={s.videoStreamingMode} onToggle={s.toggleTurbo} disabled={s.turboLocked} />
      )}
      {s.show.jpegQuality && (
        <Field htmlFor="jpegQualitySlider" label={t('sections.video.jpegQualityLabel', { jpegQuality: s.jpegQuality })}>
          <RangeSlider id="jpegQualitySlider" className={TOUCH_RANGE} min={s.jpegQualityRange.min} max={s.jpegQualityRange.max} step={1}
            value={s.jpegQuality} onValueChange={s.setJpegQuality} disabled={s.jpegQualityFixed} />
        </Field>
      )}
      {s.show.paintOver && (
        <SwitchRow id="usePaintOverQualityToggle" label={t('sections.video.usePaintOverQualityLabel')}
          checked={s.usePaintOverQuality} onToggle={s.togglePaintOver} disabled={s.paintOverLocked} />
      )}
      {s.show.paintoverCrf && (
        <Field htmlFor="videoPaintoverCRFSlider" label={tl('sections.video.paintoverCrfLabel', { crf: s.videoPaintoverCRF })}>
          <RangeSlider id="videoPaintoverCRFSlider" className={TOUCH_RANGE} min={0} max={s.videoPaintoverCRFChoices.length - 1} step={1}
            value={s.paintoverCrfIndex} onValueChange={(index) => s.setVideoPaintoverCRF(s.videoPaintoverCRFChoices[index])} disabled={s.paintoverCrfFixed} />
        </Field>
      )}
      {s.show.paintoverBurst && (
        <Field htmlFor="videoPaintoverBurstSlider" label={tl('sections.video.paintoverBurstLabel', { frames: s.videoPaintoverBurstFrames })}>
          <RangeSlider id="videoPaintoverBurstSlider" className={TOUCH_RANGE} min={s.paintoverBurstRange.min} max={s.paintoverBurstRange.max} step={1}
            value={s.videoPaintoverBurstFrames} onValueChange={s.setVideoPaintoverBurstFrames} disabled={s.paintoverBurstFixed} />
        </Field>
      )}
      {s.show.paintOverJpeg && (
        <Field htmlFor="paintOverJpegQualitySlider" label={t('sections.video.paintOverJpegQualityLabel', { paintOverJpegQuality: s.paintOverJpegQuality })}>
          <RangeSlider id="paintOverJpegQualitySlider" className={TOUCH_RANGE} min={s.paintOverJpegRange.min} max={s.paintOverJpegRange.max} step={1}
            value={s.paintOverJpegQuality} onValueChange={s.setPaintOverJpegQuality} disabled={s.paintOverJpegFixed} />
        </Field>
      )}
      {s.show.cpu && (
        <SwitchRow id="useCpuToggle" label={t('sections.video.useCpuLabel')} checked={s.useCpu} onToggle={s.toggleCpu} disabled={s.cpuLocked} />
      )}
      {s.show.cpuThreading && (
        <SwitchRow id="cpuEncodeThreadingToggle" label={t('sections.video.cpuEncodeThreadingLabel')} checked={s.cpuEncodeThreading}
          onToggle={s.toggleCpuThreading} disabled={s.cpuThreadingLocked} />
      )}
    </>
  );
}

/** The audio bitrate and the device pickers; mounting asks for the devices. */
export function AudioSettings() {
  const s = useSettings();
  const { ensureAudioDevices } = s;
  React.useEffect(() => { ensureAudioDevices(); }, [ensureAudioDevices]);
  const deviceOptions = (devices: typeof s.audioInputDevices, selected: string) => {
    const options = devices.map((device) => ({ value: device.deviceId, label: device.label }));
    // The pick the core holds, listed even before the devices are enumerated.
    if (!options.some((option) => option.value === selected)) options.unshift({ value: selected, label: t('audio.defaultDevice') });
    return options;
  };
  return (
    <>
      {s.show.audioBitrate && (
        <Field htmlFor="audioBitrateSlider" label={tl('sections.audio.bitrateLabel', { bitrate: s.audioBitRate / 1000 })}>
          <RangeSlider id="audioBitrateSlider" className={TOUCH_RANGE} min={0} max={s.audioBitrateChoices.length - 1} step={1}
            value={Math.max(0, s.audioBitrateChoices.indexOf(s.audioBitRate))}
            onValueChange={(index) => s.setAudioBitRate(s.audioBitrateChoices[index])} />
        </Field>
      )}
      {s.audioDeviceError && <p className="text-xs text-destructive">{s.audioDeviceError}</p>}
      <Field htmlFor="audioInputSelect" label={tl('sections.audio.inputLabel')}>
        <NativeSelect id="audioInputSelect" value={s.selectedInputDeviceId} disabled={s.isLoadingAudioDevices || !!s.audioDeviceError}
          onValueChange={(id) => s.selectAudioDevice("input", id)} options={deviceOptions(s.audioInputDevices, s.selectedInputDeviceId)} />
      </Field>
      {s.isOutputSelectionSupported ? (
        <Field htmlFor="audioOutputSelect" label={tl('sections.audio.outputLabel')}>
          <NativeSelect id="audioOutputSelect" value={s.selectedOutputDeviceId} disabled={s.isLoadingAudioDevices || !!s.audioDeviceError}
            onValueChange={(id) => s.selectAudioDevice("output", id)} options={deviceOptions(s.audioOutputDevices, s.selectedOutputDeviceId)} />
        </Field>
      ) : (!s.isLoadingAudioDevices && !s.audioDeviceError && (
        <p className="text-xs text-muted-foreground">{t('sections.audio.outputNotSupported')}</p>
      ))}
    </>
  );
}

/** The display's own switches: anti-aliasing, cursors, pointer motion, HiDPI, alignment, and the UI scaling. */
export function DisplaySettings() {
  const s = useSettings();
  const { addScreen, renderable, isSecondaryDisplay } = useDashboard();
  return (
    <>
      {!isSecondaryDisplay && renderable.secondScreen && (
        <Button variant="outline" size="xs" className="w-full min-w-0" onClick={addScreen} title={hint(t('sections.screen.addScreenButton'), t('sections.screen.addScreenTitle'))}>
          <Clip>{t('sections.screen.addScreenButton')}</Clip>
        </Button>
      )}
      <SwitchRow id="antiAliasingToggle" label={t('sections.screen.antiAliasingLabel')} checked={s.antiAliasing} onToggle={s.toggleAntiAliasing} />
      {s.show.useBrowserCursors && (
        <SwitchRow id="useBrowserCursorsToggle" label={t('sections.screen.useNativeCursorStylesLabel')} checked={s.useBrowserCursors} onToggle={s.toggleBrowserCursors} />
      )}
      {s.show.rawPointerMotion && (
        <SwitchRow id="rawPointerMotionToggle" label={t('sections.screen.rawPointerMotionLabel')}
          title={t(s.rawPointerMotion ? 'sections.screen.rawPointerMotionDisableTitle' : 'sections.screen.rawPointerMotionEnableTitle')}
          checked={s.rawPointerMotion} onToggle={s.toggleRawPointerMotion} />
      )}
      {s.show.macCmdAsCtrl && (
        <SwitchRow id="macCmdAsCtrlToggle" label={t('sections.screen.macCmdAsCtrlLabel')}
          title={t(s.macCmdAsCtrl ? 'sections.screen.macCmdAsCtrlDisableTitle' : 'sections.screen.macCmdAsCtrlEnableTitle')}
          checked={s.macCmdAsCtrl} onToggle={s.toggleMacCmdAsCtrl} />
      )}
      {s.show.hidpi && (
        <SwitchRow id="hidpiToggle" label={t('sections.screen.hidpiLabel')} title={s.hidpiDisabledReason ? t(s.hidpiDisabledReason) : undefined}
          checked={s.hidpiEnabled} onToggle={s.toggleHidpi} disabled={s.hidpiDisabledReason !== null} />
      )}
      {s.show.forceAligned && (
        <SwitchRow id="forceAlignedResolutionToggle" label={t('sections.screen.forceAlignedResolutionLabel')} title={t('sections.screen.forceAlignedResolutionDetails')}
          checked={s.forceAlignedResolution} onToggle={s.toggleForceAligned} />
      )}
      {s.show.uiScaling && (
        <Field htmlFor="uiScalingSelect" label={t('sections.screen.uiScalingLabel')}>
          <NativeSelect id="uiScalingSelect" value={String(s.selectedDpi)} onValueChange={s.setDpi} disabled={s.dpiDisabled}
            options={s.dpiScalingChoices.map((option) => ({ value: String(option.value), label: option.label }))} />
        </Field>
      )}
    </>
  );
}

/** The resolution: presets, a manual size, and local scaling. */
export function ResolutionSettings() {
  const s = useSettings();
  return (
    <>
      {s.show.resolutionControls && (
        <>
          <Field htmlFor="resolutionPresetSelect" label={tl('sections.screen.presetLabel')}>
            <NativeSelect id="resolutionPresetSelect" value={s.presetValue} onValueChange={s.choosePreset}
              placeholder={t('sections.screen.resolutionPresetSelect')}
              options={RESOLUTION_PRESETS.map((preset) => ({ value: preset, label: preset }))} />
          </Field>
          <div className="resolution-size flex gap-2">
            <Field htmlFor="manualWidthInput" label={tl('sections.screen.widthLabel')} className="min-w-0 flex-1">
              <Input id="manualWidthInput" type="number" min="1" step="2" className="allow-native-input h-7 text-xs" value={s.manualWidth}
                onChange={(event) => s.setManualWidth(event.target.value)} placeholder={t('sections.screen.widthPlaceholder')} />
            </Field>
            <Field htmlFor="manualHeightInput" label={tl('sections.screen.heightLabel')} className="min-w-0 flex-1">
              <Input id="manualHeightInput" type="number" min="1" step="2" className="allow-native-input h-7 text-xs" value={s.manualHeight}
                onChange={(event) => s.setManualHeight(event.target.value)} placeholder={t('sections.screen.heightPlaceholder')} />
            </Field>
          </div>
          {/* Side by side, each label wrapping and both growing to the taller one. */}
          <div className="resolution-actions flex items-stretch gap-2">
            <Button variant="outline" size="xs" className="h-auto min-h-6 min-w-0 flex-1 py-1 whitespace-normal" onClick={s.setManualResolution}
              title={t('sections.screen.setManualButton')}>{t('sections.screen.setManualButton')}</Button>
            <Button variant="outline" size="xs" className="h-auto min-h-6 min-w-0 flex-1 py-1 whitespace-normal" onClick={s.resetResolution}
              title={t('sections.screen.resetButton')}>{t('sections.screen.resetButton')}</Button>
          </div>
        </>
      )}
      <Button variant={s.scaleLocally ? "default" : "outline"} size="xs" className="w-full min-w-0" onClick={s.toggleScaleLocally}
        title={`${tl('sections.screen.scaleLocallyLabel')}: ${t(s.scaleLocally ? 'sections.screen.scaleLocallyOn' : 'sections.screen.scaleLocallyOff')}`}>
        <Clip>{tl('sections.screen.scaleLocallyLabel')}: {t(s.scaleLocally ? 'sections.screen.scaleLocallyOn' : 'sections.screen.scaleLocallyOff')}</Clip>
      </Button>
    </>
  );
}

/** The video controls as one list: the format first, then the performance. */
export function VideoSettings() {
  return (
    <div className="space-y-3">
      <FormatSettings />
      <PerformanceSettings />
    </div>
  );
}

/** The screen controls as one list: the display's switches, then the resolution. */
export function ScreenSettings() {
  return (
    <div className="space-y-3">
      <DisplaySettings />
      <ResolutionSettings />
    </div>
  );
}

/** The settings as a card of tabs, one per panel, which the toolbar layout hangs under its bar. */
export function SettingsCard() {
  const panels = useSettingsPanels();
  const tabs = [
    { value: "video", shown: panels.video, label: t('settingsTabs.video'), content: <VideoSettings /> },
    { value: "audio", shown: panels.audio, label: t('settingsTabs.audio'), content: <div className="space-y-3"><AudioSettings /></div> },
    { value: "resolution", shown: panels.screen, label: t('settingsTabs.resolution'), content: <ScreenSettings /> },
  ].filter((tab) => tab.shown);
  if (tabs.length === 0) return null;
  return (
    <Card className="w-[272px] max-w-[calc(100vw-1rem)] gap-1 py-1">
      <Tabs defaultValue={tabs[0].value} className="w-full px-1">
        <TabsList className="w-full">
          {tabs.map((tab) => <TabsTrigger key={tab.value} value={tab.value}>{tab.label}</TabsTrigger>)}
        </TabsList>
        {tabs.map((tab) => (
          <TabsContent key={tab.value} value={tab.value}>
            <CardContent className="max-h-[calc(100dvh-8rem)] overflow-x-hidden overflow-y-auto px-2 pb-2">{tab.content}</CardContent>
          </TabsContent>
        ))}
      </Tabs>
    </Card>
  );
}
