/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The one `useStreamSettings` instance, held above the layout and handed to
 * every settings panel, so a switch between layouts keeps every value and
 * every pending push.
 * @module
 */
import * as React from "react";
import { useStreamSettings, type StreamSettings } from "@/state/settings";

const SettingsContext = React.createContext<StreamSettings | null>(null);

/** Holds the one settings instance for every panel under it. */
export function SettingsProvider({ children }: { children: React.ReactNode }) {
  const settings = useStreamSettings();
  return <SettingsContext.Provider value={settings}>{children}</SettingsContext.Provider>;
}

/** The shared settings; throws outside the provider. */
export function useSettings(): StreamSettings {
  const settings = React.useContext(SettingsContext);
  if (!settings) throw new Error("useSettings needs a SettingsProvider");
  return settings;
}

/** Which settings panels have anything to show, and which halves of the video panel do. */
export function useSettingsPanels() {
  const s = useSettings();
  const format = s.show.streamMode || s.show.encoder || s.show.webcamEncoder || s.show.fullColor || s.show.tenBit;
  const performance = s.show.framerate || s.show.rateControl || s.show.bitrate || s.show.crf || s.show.turbo || s.show.jpegQuality
    || s.show.paintOver || s.show.paintoverCrf || s.show.paintoverBurst || s.show.paintOverJpeg || s.show.cpu || s.show.cpuThreading;
  return {
    video: s.show.videoPanel && (format || performance),
    audio: s.show.audioPanel,
    screen: s.show.screenPanel,
    format,
    performance,
  };
}
