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
 * arrive through `useChromeEvents`, and the chrome preferences each keeps per
 * browser (the figures strip, how see-through the panels are) is `useBarPrefs`.
 * @module
 */
import * as React from "react";
import { Crosshair, Gamepad2, Gauge, Keyboard, Maximize, Touchpad } from "lucide-react";
import { STREAMS, type StreamSpec } from "@/panels/catalog";
import { useDashboard } from "@/state/dashboard";
import { getPrefixedKey, onCoreMessage } from "@/utils";
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

/** How see-through the layout's panels are: solid, light, medium, or strong. */
export type Translucency = 0 | 1 | 2 | 3;

/** The two chrome preferences every bar layout shares, each kept per browser under its prefixed key. */
export interface BarPrefs {
  /** The figures strip a bar's middle shows; false mounts nothing of it. */
  barStrip: boolean;
  toggleBarStrip: () => void;
  /** How see-through the panels are: 0 solid, 1 light, 2 strong, the shared
   * pages' own surface tokens tinted (the `data-translucency` rules in
   * `index.css`), so one point of adjustment covers the bar and every panel. */
  translucency: Translucency;
  /** Lands the slider's stop; clamps anything outside the three levels to solid. */
  setTranslucency: (level: Translucency) => void;
}

/** What `useBarPrefs` returns: the module's one copy of the two preferences. */
interface ChromePrefs {
  barStrip: boolean;
  translucency: Translucency;
}

/** The preferences storage has; defaults where it has none yet. */
function readBarPrefs(): ChromePrefs {
  const strip = localStorage.getItem(getPrefixedKey("bar_strip"));
  const translucency = Number(localStorage.getItem(getPrefixedKey("bar_translucency")));
  return { barStrip: strip === null ? true : strip === "true", translucency: translucency === 1 || translucency === 2 || translucency === 3 ? translucency : 0 };
}

/** The live value of the two preferences: replaced on every write, so its identity is the change. */
let barPrefs: ChromePrefs = readBarPrefs();
/** The components to re-render on a write. */
const barPrefsListeners = new Set<() => void>();

/** Replaces both preferences, keeps the keys, and wakes every reader. */
function writeBarPrefs(next: ChromePrefs): void {
  barPrefs = next;
  localStorage.setItem(getPrefixedKey("bar_strip"), String(next.barStrip));
  localStorage.setItem(getPrefixedKey("bar_translucency"), String(next.translucency));
  barPrefsListeners.forEach((listener) => listener());
}

/**
 * The chrome preferences every bar layout shares; see `BarPrefs`. One snapshot
 * on the module replayed through `useSyncExternalStore`: a hook per caller
 * would keep its own copy, and the button that flips one would leave every
 * surface that draws by it on the old value until remount.
 */
export function useBarPrefs(): BarPrefs {
  const subscribe = React.useCallback((notify: () => void) => {
    barPrefsListeners.add(notify);
    return () => { barPrefsListeners.delete(notify); };
  }, []);
  const prefs = React.useSyncExternalStore(subscribe, () => barPrefs);
  return {
    barStrip: prefs.barStrip,
    translucency: prefs.translucency,
    toggleBarStrip: React.useCallback(() => writeBarPrefs({ ...barPrefs, barStrip: !barPrefs.barStrip }), []),
    setTranslucency: React.useCallback((level: Translucency) => {
      writeBarPrefs({ ...barPrefs, translucency: level === 1 || level === 2 || level === 3 ? level : 0 });
    }, []),
  };
}

/** What a bar layout (the toolbar or the edge layout) keeps about what hangs from its bar. */
export interface BarState {
  settingsOpen: boolean;
  menuOpen: boolean;
  /** The floating panels open, by catalog id. */
  floating: Record<string, boolean>;
  /** Folded away, leaving a tab to bring it back. */
  collapsed: boolean;
  setCollapsed: React.Dispatch<React.SetStateAction<boolean>>;
  softKeysOpen: boolean;
  /** Opens or shuts the settings card, shutting the menu as it opens. */
  toggleSettings: () => void;
  closeSettings: () => void;
  /** The overflow menu's `onOpenChange`: opening it shuts the settings card. */
  onMenuOpenChange: (open: boolean) => void;
  toggleFloating: (id: string) => void;
  toggleSoftKeys: () => void;
  /** Shuts the settings card and the menu. */
  fold: () => void;
  /** Folds the bar away behind its edge. */
  hide: () => void;
}

/**
 * The open and folded state of a bar layout. Ctrl+Shift+M folds the bar away
 * or brings it back, gaming mode folds it away so the session has the
 * pointer, and entering fullscreen shuts the settings card; all three, and
 * telling the core while the settings card, a floating panel, or the stats
 * are open, are `useChromeEvents`. The menu is controlled so its open state
 * survives the submenu swaps; shutting it folds the submenus with it.
 * @returns The state and what changes it.
 */
export function useBarState(): BarState {
  const d = useDashboard();
  const [settingsOpen, setSettingsOpen] = React.useState(false);
  const [menuOpen, setMenuOpen] = React.useState(false);
  const [floating, setFloating] = React.useState<Record<string, boolean>>({});
  const [collapsed, setCollapsed] = React.useState(false);
  const [softKeysOpen, setSoftKeysOpen] = React.useState(false);

  const closeSettings = React.useCallback(() => setSettingsOpen(false), []);
  const fold = React.useCallback(() => {
    setSettingsOpen(false);
    setMenuOpen(false);
  }, []);
  const hide = React.useCallback(() => {
    fold();
    setCollapsed(true);
  }, [fold]);
  const toggleSettings = React.useCallback(() => {
    setMenuOpen(false);
    setSettingsOpen((open) => !open);
  }, []);
  const onMenuOpenChange = React.useCallback((open: boolean) => {
    setMenuOpen(open);
    if (open) setSettingsOpen(false);
  }, []);
  const toggleFloating = React.useCallback((id: string) => setFloating((open) => ({ ...open, [id]: !open[id] })), []);
  const toggleSoftKeys = React.useCallback(() => setSoftKeysOpen((open) => !open), []);

  useChromeEvents({
    open: settingsOpen || Object.values(floating).some(Boolean) || d.statsOpen,
    onToggle: () => { fold(); setCollapsed((value) => !value); },
    onGamingMode: hide,
    onFullscreen: closeSettings,
  });

  return {
    settingsOpen, menuOpen, floating, collapsed, setCollapsed, softKeysOpen,
    toggleSettings, closeSettings, onMenuOpenChange, toggleFloating, toggleSoftKeys, fold, hide,
  };
}
