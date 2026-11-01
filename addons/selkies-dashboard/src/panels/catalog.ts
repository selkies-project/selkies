/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The panel catalog: every panel a layout can show, in the order every
 * layout shows them, with what decides whether the page shows it and what it
 * draws. A layout decides only the arrangement and the skin: the sidebar
 * folds each panel into a section, the toolbar puts the settings panels in
 * its card of tabs, floats what floats beside its bar, and lists the rest in
 * its menu. Adding a panel is one entry here.
 *
 * The stream toggles every layout carries (video, audio, microphone, webcam,
 * and the gamepad input) are a catalog of their own, `STREAMS`.
 * @module
 */
import type * as React from "react";
import { Clipboard as ClipboardIcon, FileText, Gamepad2, Joystick, Keyboard, LayoutGrid, Mic, Monitor, Printer, Share2, Volume2, Webcam,
  type LucideIcon } from "lucide-react";
import { AppsAction, FilesActions, GamepadsBody, SharingBody } from "@/panels/catalog-bodies";
import { Clipboard } from "@/panels/clipboard";
import { Printing } from "@/panels/printing";
import { AudioDeviceSettings, AudioQualitySettings, DisplaySettings, FormatSettings, PerformanceSettings, ResolutionSettings } from "@/panels/settings";
import { ShortcutsMenu } from "@/panels/shortcuts";
import { useDashboard, type DashboardState } from "@/state/dashboard";
import { useSettings, useSettingsPanels } from "@/state/settings-context";

/** Where a panel is drawn: a section of its own, a menu's popup, or a panel floating beside a bar. */
export type PanelPlace = "section" | "menu" | "float";

/** What decides whether a panel shows. */
export interface PanelContext {
  d: DashboardState;
  /** Which settings panels and cards have anything to show. */
  panels: ReturnType<typeof useSettingsPanels>;
  s: ReturnType<typeof useSettings>;
  /** This page is the primary display's, not a second screen's. */
  primary: boolean;
}

/** One card of a settings panel: a section lists the cards one after the other, a tab folds them in an accordion. */
export interface PanelCard {
  value: string;
  /** Dictionary key of its title. */
  title: string;
  shown: (ctx: PanelContext) => boolean;
  Body: React.ComponentType;
}

/** One panel of the catalog. */
export interface PanelSpec {
  id: string;
  /** The id of its content element, which the suites address. */
  contentId: string;
  /** Dictionary key of its title. */
  title: string;
  /** Dictionary key of its short name, where a layout tabs it. */
  tab?: string;
  /** Its mark in a menu or on a bar. */
  icon?: LucideIcon;
  /**
   * `settings`: the stream's settings, in cards. `stats`: the stats, which a
   * layout lays out itself (`LayoutSpec.stats` over the stream). `panel`:
   * anything else, drawn by `Body`.
   */
  kind: "settings" | "stats" | "panel";
  shown: (ctx: PanelContext, place: PanelPlace) => boolean;
  cards?: PanelCard[];
  Body?: React.ComponentType<{ place: PanelPlace }>;
  /** A menu opens this dialog rather than drawing the body. */
  dialog?: "apps" | "files";
  /** Classes of the menu popup that draws the body, where it needs a width of its own. */
  menuClass?: string;
  /** A layout with a bar floats it beside the bar rather than listing it in a menu. */
  floats?: boolean;
}

export const PANELS: PanelSpec[] = [
  {
    id: "video", contentId: "settings-content", title: "sections.video.title", tab: "settingsTabs.video", kind: "settings",
    shown: ({ panels }) => panels.video,
    cards: [
      { value: "format", title: "settingsSections.format", shown: ({ panels }) => panels.format, Body: FormatSettings },
      { value: "performance", title: "settingsSections.performance", shown: ({ panels }) => panels.performance, Body: PerformanceSettings },
    ],
  },
  {
    id: "screen", contentId: "screen-settings-content", title: "sections.screen.title", tab: "settingsTabs.resolution", kind: "settings", shown: ({ panels }) => panels.screen,
    cards: [
      { value: "display", title: "settingsSections.display", shown: () => true, Body: DisplaySettings },
      { value: "resolution", title: "settingsSections.resolution", shown: () => true, Body: ResolutionSettings },
    ],
  },
  {
    id: "audio", contentId: "audio-settings-content", title: "sections.audio.title", tab: "settingsTabs.audio", kind: "settings", shown: ({ panels }) => panels.audio,
    cards: [
      { value: "quality", title: "settingsSections.quality", shown: ({ s }) => s.show.audioBitrate, Body: AudioQualitySettings },
      { value: "devices", title: "settingsSections.devices", shown: () => true, Body: AudioDeviceSettings },
    ],
  },
  {
    id: "stats", contentId: "stats-content", title: "sections.stats.title", kind: "stats",
    shown: ({ d, primary }) => primary && d.renderable.stats,
  },
  {
    id: "clipboard", contentId: "clipboard-content", title: "sections.clipboard.title", icon: ClipboardIcon, kind: "panel",
    shown: ({ d, primary }) => primary && d.renderable.clipboard, Body: Clipboard,
    menuClass: "w-[340px] max-w-[calc(100vw-1rem)]",
  },
  {
    id: "files", contentId: "files-content", title: "sections.files.title", icon: FileText, kind: "panel", dialog: "files",
    shown: ({ d, primary }) => primary && d.renderable.files && (d.renderable.fileUpload || d.renderable.fileDownload), Body: FilesActions,
  },
  {
    id: "printing", contentId: "printing-content", title: "sections.printing.title", icon: Printer, kind: "panel",
    shown: ({ d, primary }) => primary && d.printJobCount > 0, Body: Printing,
  },
  {
    id: "apps", contentId: "apps-content", title: "sections.apps.title", icon: LayoutGrid, kind: "panel", dialog: "apps",
    shown: ({ d, primary }) => primary && d.renderable.apps, Body: AppsAction,
  },
  {
    id: "sharing", contentId: "sharing-content", title: "sections.sharing.title", icon: Share2, kind: "panel",
    shown: ({ d, primary }) => primary && d.renderable.sharing,
    Body: SharingBody,
  },
  {
    id: "gamepads", contentId: "gamepads-content", title: "sections.gamepads.title", icon: Joystick, kind: "panel", floats: true,
    // Floating, the panel carries the gamepad input switch too, which a layout with sections has as a toggle of its own.
    shown: ({ d, primary }, place) => primary && (d.renderable.gamepads || (place === "float" && d.renderable.coreButtons && d.renderable.gamepadToggle)),
    Body: GamepadsBody,
  },
  {
    id: "shortcuts", contentId: "shortcuts-content", title: "sections.shortcuts.title", icon: Keyboard, kind: "panel",
    shown: ({ d }) => d.renderable.shortcuts, Body: ShortcutsMenu,
  },
];

/** What every panel's `shown` reads, from the shared state. */
export function usePanelContext(): PanelContext {
  const d = useDashboard();
  return { d, panels: useSettingsPanels(), s: useSettings(), primary: !d.isSecondaryDisplay };
}

/** The cards of a settings panel the page shows. */
export function shownCards(panel: PanelSpec, ctx: PanelContext): PanelCard[] {
  return (panel.cards ?? []).filter((card) => card.shown(ctx));
}

/** One stream toggle of the catalog. */
export interface StreamSpec {
  id: string;
  icon: LucideIcon;
  /** Dictionary key of its name in a menu. */
  label: string;
  /** Dictionary keys of what a press does, while off and while on. */
  enable: string;
  disable: string;
  /** An input rather than a stream: a layout that floats the gamepads panel toggles it there. */
  input?: boolean;
  /** Whether the server lets the page toggle it (`renderable`). */
  allowed: (r: DashboardState["renderable"]) => boolean;
  on: (d: DashboardState) => boolean;
  toggle: (d: DashboardState) => void;
}

/** The stream toggles, in order: the streams, then the gamepad input. */
export const STREAMS: StreamSpec[] = [
  { id: "video", icon: Monitor, label: "topMenu.videoStream", enable: "buttons.videoStreamEnableTitle", disable: "buttons.videoStreamDisableTitle",
    allowed: (r) => r.videoToggle, on: (d) => d.video, toggle: (d) => d.toggleVideo() },
  { id: "audio", icon: Volume2, label: "topMenu.audioStream", enable: "buttons.audioStreamEnableTitle", disable: "buttons.audioStreamDisableTitle",
    allowed: (r) => r.audioToggle, on: (d) => d.audio, toggle: (d) => d.toggleAudio() },
  { id: "microphone", icon: Mic, label: "topMenu.microphone", enable: "buttons.microphoneEnableTitle", disable: "buttons.microphoneDisableTitle",
    allowed: (r) => r.microphoneToggle, on: (d) => d.microphone, toggle: (d) => d.toggleMicrophone() },
  { id: "webcam", icon: Webcam, label: "topMenu.webcam", enable: "buttons.webcamEnableTitle", disable: "buttons.webcamDisableTitle",
    allowed: (r) => r.webcamToggle, on: (d) => d.webcam, toggle: (d) => d.toggleWebcam() },
  { id: "gamepad", icon: Gamepad2, label: "sections.gamepads.title", enable: "buttons.gamepadEnableTitle", disable: "buttons.gamepadDisableTitle",
    input: true, allowed: (r) => r.gamepadToggle, on: (d) => d.gamepad, toggle: (d) => d.toggleGamepad() },
];
