/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The controls a layout offers besides the catalog's panels (`panels/catalog.ts`),
 * decided once for every layout: which of them the session allows and this page
 * can use, what each is called, its mark, its state, and what pressing it does.
 * A layout only places them, the sidebar as tiles and header buttons, the toolbar
 * in its bar, and a new layout takes the same list. Each control carries the
 * class the suites and the core find it by, whichever layout draws it. The
 * events every layout's chrome answers (Ctrl+Shift+M, gaming mode, fullscreen)
 * arrive through `useChromeEvents`.
 * @module
 */
import * as React from "react";
import { Crosshair, Gamepad2, Gauge, Keyboard, Maximize, Touchpad } from "lucide-react";
import { STREAMS, type StreamSpec } from "@/panels/catalog";
import { useDashboard } from "@/state/dashboard";
import { onCoreMessage } from "@/utils";
import { t } from "@/i18n";

/**
 * Maximize drawn to the crosshair's circle: its corners stop a unit inside that circle, so drawn to its extent the
 * two read as one size beside each other.
 */
function FullscreenIcon(props: { className?: string }) {
  return React.createElement(Maximize, { viewBox: "1.09 1.09 21.82 21.82", ...props });
}

/** One control a layout places. */
export interface Action {
  /** The class the suites and the core find the control by. */
  className: string;
  /** Its accessible name. */
  label: string;
  /** What hovering it explains, where that says more than the name. */
  title: string;
  icon: React.ComponentType<{ className?: string }>;
  /** A toggle's state; absent for a plain action. */
  active?: boolean;
  run: () => void;
}

/** What every layout may place. A control the session or the client rules out is null, or left out. */
export interface Actions {
  /** The stream switches this page may use; none on a second display's page. */
  streams: StreamSpec[];
  /** A touch client's own, once the page has seen a touch: the touch gamepad (on the primary's page). */
  touchGamepad: Action | null;
  /** Trackpad mode on a touch client. */
  trackpad: Action | null;
  /** The floating button that pops the system's on-screen keyboard, on a touch client. */
  keyboardButton: Action | null;
  /** Whether a touch client gets the soft keys: the keys, or with only the trackpad allowed its speed while it is on. */
  softKeys: boolean;
  fullscreen: Action | null;
  gamingMode: Action | null;
  /** The stats over the stream, on the primary's page. */
  stats: Action | null;
  /** The citation link at the layout's foot, which goes with the shortcuts. */
  cite: boolean;
}

/** The controls this page offers now; see `Actions`. */
export function useActions(): Actions {
  const d = useDashboard();
  const r = d.renderable;
  const primary = !d.isSecondaryDisplay;
  const touch = d.isTouch;
  const gaming = t("gamingModeTitle");
  return {
    streams: primary && r.coreButtons ? STREAMS.filter((stream) => stream.allowed(r)) : [],
    touchGamepad: touch && primary ? {
      className: "touch-gamepad-button", label: t("topMenu.touchGamepad"), title: t("topMenu.touchGamepad"),
      icon: Gamepad2, active: d.touchGamepad, run: d.toggleTouchGamepad,
    } : null,
    trackpad: touch && r.trackpad ? {
      className: "trackpad-mode-button", label: t("trackpadModeTitle"), title: t("trackpadModeTitle"),
      icon: Touchpad, active: d.trackpad, run: d.toggleTrackpad,
    } : null,
    keyboardButton: touch && r.keyboardButton ? {
      className: "keyboard-toggle-button", label: t("keyboardButtonToggleTitle"), title: t("keyboardButtonToggleTitle"),
      icon: Keyboard, active: d.keyboardButton, run: d.toggleKeyboardButton,
    } : null,
    softKeys: touch && (r.softButtons || (r.trackpad && d.trackpad)),
    fullscreen: r.fullscreen ? {
      className: "fullscreen-button", label: t("topMenu.toggleFullscreen"), title: t("topMenu.toggleFullscreen"),
      icon: FullscreenIcon, run: d.toggleFullscreen,
    } : null,
    gamingMode: r.gamingMode ? {
      className: "gaming-mode-button", label: gaming, title: `${gaming} — ${t("gamingModeHint")}`,
      icon: Crosshair, run: d.toggleGamingMode,
    } : null,
    stats: primary && r.stats ? {
      className: "stats-button", label: t("topMenu.systemMonitoring"), title: t("topMenu.systemMonitoring"),
      icon: Gauge, active: d.statsOpen, run: () => d.setStatsOpen(!d.statsOpen),
    } : null,
    cite: r.shortcuts,
  };
}

/** How a layout's chrome answers the events every layout meets (`useChromeEvents`). */
export interface ChromeEvents {
  /** Whether its chrome is open over the stream now, which the core is told. */
  open: boolean;
  /** Ctrl+Shift+M, owned by the core: show or hide the chrome. */
  onToggle: () => void;
  /** Gaming mode starting: the chrome steps out of the game's way. */
  onGamingMode?: () => void;
  /** The page entering fullscreen, from any source: the user lands in the session. */
  onFullscreen?: () => void;
}

/**
 * Wires a layout's chrome to the core's messages and the page's fullscreen, and
 * tells the core while the chrome is open. The handlers may change between
 * renders; the latest ones answer.
 */
export function useChromeEvents(events: ChromeEvents): void {
  const { reportChromeOpen } = useDashboard();
  const latest = React.useRef(events);
  React.useEffect(() => { latest.current = events; });
  React.useEffect(() => {
    const offMessages = onCoreMessage((message) => {
      if (message.type === "toggleDashboard") latest.current.onToggle();
      if (message.type === "gamingModeUpdate" && message.active) latest.current.onGamingMode?.();
    });
    const onFullscreen = () => { if (document.fullscreenElement) latest.current.onFullscreen?.(); };
    document.addEventListener("fullscreenchange", onFullscreen);
    return () => {
      offMessages();
      document.removeEventListener("fullscreenchange", onFullscreen);
    };
  }, []);
  React.useEffect(() => { reportChromeOpen(events.open); }, [events.open, reportChromeOpen]);
}
