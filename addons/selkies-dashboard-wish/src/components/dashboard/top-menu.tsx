/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The wish dashboard's floating toolbar and the overlays it owns: the toolbar
 * (the project logo linking to the project, the toolbox button opening the
 * overflow menu, settings, gamepads, monitoring, fullscreen, gaming mode, the
 * touch controls, and a button that slides the bar up behind the top edge,
 * leaving a tab to bring it back), the overflow menu that opens the apps
 * panel, leads with the stream switches, nests the occasional tools
 * (clipboard, files, printing, sharing, shortcuts) each in its own submenu,
 * places a second screen, and closes with the branding and the theme toggle
 * in its footer, plus the mobile soft keys and the floating, draggable
 * Gamepads and System Monitoring panels. Both panels stay open until their
 * toolbar button is pressed again and are dragged by their header; the
 * monitoring panel's open state (`stats_strip`, shared with the default
 * dashboard), position, and compact or detailed view survive a reload. On a
 * touch client the keyboard button shows or hides the floating on-screen
 * keyboard button, which stays away while a hardware keyboard is attached.
 *
 * Reads the `serverSettings`, `trackpadModeUpdate`, and `gamingModeUpdate`
 * messages the core posts on `window` (entering gaming mode, which holds the
 * pointer and keyboard, slides the bar up and leaves its tab), plus
 * `toggleDashboard` (Ctrl+Shift+M), which slides it up or back down, and posts `sidebarVisibilityChanged`, `touchinput:trackpad`,
 * `touchinput:touch`, `setSynth`, and `setTrackpadSpeed` back; held modifier keys are delivered as
 * synthetic KeyboardEvents on `window`, which the core's input handler consumes
 * like real ones. A secondary display opens as a new window on the
 * `#display2-<direction>` fragment, placed with the Window Management API
 * where the browser offers it.
 * @module
 */

import * as React from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import { Menu, MenuCheckboxItem, MenuItem, MenuPopup, MenuSeparator, MenuSub, MenuSubPopup, MenuSubTrigger, MenuTrigger } from "@/components/ui/menu";
import { ModeToggle } from "@/components/ui/ModeToggle";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Volume2,
  ChevronDown,
  ChevronUp,
  Gamepad2,
  Joystick,
  Monitor,
  Maximize,
  Mic,
  Webcam,
  Settings2,
  Gauge,
  Share2,
  Clipboard as ClipboardIcon,
  FileText,
  LayoutGrid,
  Hand,
  Keyboard,
  ToolCase,
  Touchpad,
  ScreenShare,
  Crosshair,
  Printer,
} from "lucide-react";

import { Clipboard } from "@/components/dashboard/clipboard";
import { Files, FilesDialog } from "@/components/dashboard/files";
import { Printing } from "@/components/dashboard/printing";
import { Apps } from "@/components/dashboard/apps";
import { Settings } from "@/components/dashboard/settings";
import { SystemMonitoring } from "@/components/dashboard/system-monitoring";
import { GamepadPanel, keyboardWatch, useKeyboardAttached } from "@/components/dashboard/gamepad";
import { Sharing } from "@/components/dashboard/sharing";
import { ShortcutsMenu } from "@/components/dashboard/shortcuts-menu";
import { SelkiesLogo } from "@/components/logo";
import { computeRenderableSettings, getLastServerSettings, getPrefixedKey, getPrintJobs, hardwareKeyboard, isMobileClient, isSecondaryDisplay } from "@/utils";
import { PALETTE_CHORDS, PALETTE_KEYS, TRACKPAD_SPEEDS, TRACKPAD_SPEED_KEY, USER_CHORDS_KEY, chordEvents,
  formatChord, parseChord, readUserChords, writeUserChords } from "../../../../selkies-web-core/lib/touch-controls.js";
import { fragmentWithSessionToken } from "../../../../selkies-web-core/lib/page-url.js";
import { t } from "@/i18n";

/**
 * Stream toggles owned by DashboardOverlay, which keeps the Ctrl+Shift
 * shortcuts working while this menu is unmounted.
 */
interface TopMenuProps {
  /** The video stream is running. */
  isVideoActive: boolean;
  /** Audio playback is running. */
  isAudioActive: boolean;
  /** The microphone uplink is running. */
  isMicrophoneActive: boolean;
  /** The webcam uplink is running. */
  isWebcamActive: boolean;
  /** Physical gamepad forwarding is enabled. */
  isGamepadEnabled: boolean;
  /** The on-screen touch gamepad is shown. */
  isTouchGamepadActive: boolean;
  /** Toggles the video stream. */
  onVideoToggle: () => void;
  /** Toggles audio playback. */
  onAudioToggle: () => void;
  /** Toggles the microphone uplink. */
  onMicrophoneToggle: () => void;
  /** Toggles the webcam uplink. */
  onWebcamToggle: () => void;
  /** Toggles physical gamepad forwarding. */
  onGamepadToggle: () => void;
  /** Toggles the on-screen touch gamepad. */
  onToggleTouchGamepad: () => void;
  /** The floating on-screen keyboard button is shown. */
  isKeyboardButtonVisible: boolean;
  /** Shows or hides the floating on-screen keyboard button. */
  onKeyboardButtonVisible: (visible: boolean) => void;
}

/** Where the toolbar's logo and the overflow menu's footer link to. */
const PROJECT_URL = "https://github.com/selkies-project/selkies";

/**
 * Leaves fullscreen when in it, otherwise asks the core to enter it. Entering
 * is the core's, which owns what each mode locks (plain fullscreen locks
 * neither the pointer nor the keyboard, gaming mode locks both); exiting is the
 * browser's own call through whichever prefixed API exists.
 * @param request The core message that enters the mode.
 */
function toggleFullscreen(request: "requestFullscreen" | "requestGamingMode"): void {
  if (!document.fullscreenElement) {
    window.postMessage({ type: request }, window.location.origin);
    return;
  }
  const doc = document as any;
  const exit = doc.exitFullscreen || doc.mozCancelFullScreen || doc.webkitExitFullscreen || doc.msExitFullscreen;
  if (exit) Promise.resolve(exit.call(document)).catch((err: unknown) => console.error("Error exiting fullscreen:", err));
}

/** Height of the toolbar in pixels, which sliding it up by hides it exactly. */
const TOOLBAR_HEIGHT = 42;

/**
 * Drag state for a floating panel. A press starts a drag only on an element
 * inside a `data-drag-handle` and not on a button in it, and the position is
 * held inside the window by the panel's measured size.
 * @param ref The ref on the panel element, which the caller owns so that the
 *     hook's result holds no ref to read during render.
 * @param initial Where the panel starts.
 * @param fallbackSize Size that bounds the drag until the panel is measured.
 * @param storageKey When given, the position is kept under this prefixed key
 *     and restored on the next load.
 * @returns The panel's position, whether it is being dragged, and the
 *     mouse-down handler to put on it.
 */
function useFloatingPanel(
  ref: React.RefObject<HTMLDivElement | null>,
  initial: { x: number; y: number },
  fallbackSize: { w: number; h: number },
  storageKey?: string,
) {
  const [position, setPosition] = React.useState(() => {
    if (!storageKey) return initial;
    try {
      const saved = JSON.parse(localStorage.getItem(getPrefixedKey(storageKey)) ?? "null");
      if (saved && Number.isFinite(saved.x) && Number.isFinite(saved.y)) {
        // Kept on screen: the window may be smaller than when it was saved.
        return {
          x: Math.max(0, Math.min(saved.x, window.innerWidth - 40)),
          y: Math.max(0, Math.min(saved.y, window.innerHeight - 40)),
        };
      }
    } catch { /* unreadable: the default stands */ }
    return initial;
  });
  const lastDrag = React.useRef<{ x: number; y: number } | null>(null);
  const [isDragging, setIsDragging] = React.useState(false);
  const start = React.useRef({ x: 0, y: 0 });
  const { w: fallbackW, h: fallbackH } = fallbackSize;

  const onMouseDown = (e: React.MouseEvent) => {
    const target = e.target as HTMLElement;
    if (!target.closest('[data-drag-handle]') || target.closest('button')) return;
    setIsDragging(true);
    start.current = { x: e.clientX - position.x, y: e.clientY - position.y };
  };

  React.useEffect(() => {
    if (!isDragging) return undefined;
    const handleMove = (e: MouseEvent) => {
      const el = ref.current;
      const maxX = window.innerWidth - (el ? el.offsetWidth : fallbackW);
      const maxY = window.innerHeight - (el ? el.offsetHeight : fallbackH);
      const next = {
        x: Math.max(0, Math.min(e.clientX - start.current.x, maxX)),
        y: Math.max(0, Math.min(e.clientY - start.current.y, maxY)),
      };
      lastDrag.current = next;
      setPosition(next);
    };
    const handleUp = () => {
      setIsDragging(false);
      if (storageKey && lastDrag.current) {
        localStorage.setItem(getPrefixedKey(storageKey), JSON.stringify(lastDrag.current));
      }
    };
    document.addEventListener('mousemove', handleMove);
    document.addEventListener('mouseup', handleUp);
    return () => {
      document.removeEventListener('mousemove', handleMove);
      document.removeEventListener('mouseup', handleUp);
    };
  }, [ref, isDragging, fallbackW, fallbackH, storageKey]);

  return { position, isDragging, onMouseDown };
}

/**
 * The menu bar, its panels, and the overlays around it. Server settings are
 * seeded from the cached broadcast because the menu mounts after the core
 * connects; the server's UI customization decides which entries render.
 */
export function TopMenu({
  isVideoActive,
  isAudioActive,
  isMicrophoneActive,
  isWebcamActive,
  isGamepadEnabled,
  isTouchGamepadActive,
  onVideoToggle,
  onAudioToggle,
  onMicrophoneToggle,
  onWebcamToggle,
  onGamepadToggle,
  onToggleTouchGamepad,
  isKeyboardButtonVisible,
  onKeyboardButtonVisible }: TopMenuProps) {

  const [activePanel, setActivePanel] = React.useState<string | null>(null);
  // The overflow menu is controlled so its open state survives the submenu
  // swaps; closing it folds the submenus with it.
  const [overflowOpen, setOverflowOpen] = React.useState(false);
  const [showAppsModal, setShowAppsModal] = React.useState(false);
  const [showFilesModal, setShowFilesModal] = React.useState(false);
  const [printJobCount, setPrintJobCount] = React.useState(() => getPrintJobs().length);
  // Kept across reloads under the key the default dashboard's stats strip uses.
  const [showSystemMonitoring, setShowSystemMonitoring] = React.useState(
    () => localStorage.getItem(getPrefixedKey("stats_strip")) === "true");
  const [isDragging, setIsDragging] = React.useState(false);
  const [showGamepads, setShowGamepads] = React.useState(false);
  // Slid up behind the top edge of the window, leaving a tab to bring it back.
  const [isCollapsed, setIsCollapsed] = React.useState(false);
  const [position, setPosition] = React.useState(() => {
    // Rough centering off an assumed 400px menu; the measured width recenters
    // it after mount.
    const x = window.innerWidth / 2 - 200;
    return { x, y: 0 };
  });
  const monitoringRef = React.useRef<HTMLDivElement>(null);
  const gamepadRef = React.useRef<HTMLDivElement>(null);
  const monitoringPanel = useFloatingPanel(monitoringRef, { x: 16, y: 64 }, { w: 300, h: 200 }, "stats_panel_position");
  const gamepadPanel = useFloatingPanel(gamepadRef, { x: Math.max(16, window.innerWidth - 316), y: 64 }, { w: 300, h: 200 });

  const [serverSettings, setServerSettings] = React.useState<any>(() => getLastServerSettings());
  const [renderableSettings, setRenderableSettings] = React.useState<any>(() => computeRenderableSettings(getLastServerSettings()));
  const uiTitle: string = serverSettings?.ui_title?.value ?? 'Selkies';
  const uiShowLogo: boolean = serverSettings?.ui_show_logo?.value ?? true;

  const isMobile = isMobileClient;
  const [hasDetectedTouch, setHasDetectedTouch] = React.useState(false);
  const [isTrackpadModeActive, setIsTrackpadModeActive] = React.useState(false);
  // An attached keyboard keeps the system's on-screen one down, so the floating
  // button that pops it goes while one is in use (lib/hardware-keyboard.js).
  const keyboardAttached = useKeyboardAttached();
  const toggleKeyboardButton = () => {
    // Asked for back while a keyboard was assumed: the user knows better.
    if (keyboardAttached) {
      keyboardWatch.reset();
      onKeyboardButtonVisible(true);
      return;
    }
    onKeyboardButtonVisible(!isKeyboardButtonVisible);
  };

  const [availablePlacements, setAvailablePlacements] = React.useState<any>(null);

  const [heldKeys, setHeldKeys] = React.useState({
    Control: false,
    Alt: false,
    Meta: false,
  });
  const [isKeyPaletteOpen, setIsKeyPaletteOpen] = React.useState(false);
  // How far the soft keys reach up from the bottom, published as --soft-keys-top
  // for the poor-connection mark to sit above them.
  const softKeysObserver = React.useRef<ResizeObserver | null>(null);
  const softKeysRef = React.useCallback((bar: HTMLDivElement | null) => {
    const root = document.documentElement;
    softKeysObserver.current?.disconnect();
    softKeysObserver.current = null;
    if (!bar) {
      root.style.removeProperty('--soft-keys-top');
      return;
    }
    const place = () => root.style.setProperty(
      '--soft-keys-top', `${parseFloat(getComputedStyle(bar).bottom) + bar.offsetHeight}px`);
    softKeysObserver.current = new ResizeObserver(place);
    softKeysObserver.current.observe(bar);
    place();
  }, []);
  const [userChords, setUserChords] = React.useState<string[]>(() =>
    readUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY)));
  const [chordDraft, setChordDraft] = React.useState("");
  const [chordRefused, setChordRefused] = React.useState(false);
  const [trackpadSpeed, setTrackpadSpeed] = React.useState<number>(() => {
    const stored = parseFloat(localStorage.getItem(getPrefixedKey(TRACKPAD_SPEED_KEY)) ?? "");
    return TRACKPAD_SPEEDS.includes(stored) ? stored : 1;
  });

  const dragRef = React.useRef<HTMLDivElement>(null);
  const panelRef = React.useRef<HTMLDivElement>(null);

  const startPosRef = React.useRef({ x: 0, y: 0 });

  React.useEffect(() => {
    const handleMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      if (event.data?.type === "serverSettings") {
        console.log("Dashboard received server settings:", event.data.payload);
        setServerSettings(event.data.payload);
        setRenderableSettings(computeRenderableSettings(event.data.payload));
      }
      if (event.data?.type === 'gamingModeUpdate' && event.data.active) {
        setIsCollapsed(true);
      }
      // Ctrl+Shift+M, owned by the core: the bar slides up or back down.
      if (event.data?.type === 'toggleDashboard') {
        setActivePanel(null);
        setOverflowOpen(false);
        setIsCollapsed((collapsed) => !collapsed);
      }
      if (event.data?.type === 'trackpadModeUpdate') {
        if (typeof event.data.enabled === 'boolean') {
          setIsTrackpadModeActive(event.data.enabled);
        }
      }
    };
    const countPrintJobs = () => setPrintJobCount(getPrintJobs().length);
    window.addEventListener("message", handleMessage);
    window.addEventListener("printJobsChanged", countPrintJobs);
    return () => {
      window.removeEventListener("message", handleMessage);
      window.removeEventListener("printJobsChanged", countPrintJobs);
    };
  }, []);

  React.useEffect(() => {
    localStorage.setItem(getPrefixedKey("stats_strip"), String(showSystemMonitoring));
  }, [showSystemMonitoring]);

  // The core reacts to panels opening and closing (input focus); the monitoring
  // overlay is not an activePanel and counts as open too.
  React.useEffect(() => {
    window.postMessage(
      { type: 'sidebarVisibilityChanged', isOpen: !!activePanel || showSystemMonitoring || showGamepads },
      window.location.origin
    );
  }, [activePanel, showSystemMonitoring, showGamepads]);

  // Entering fullscreen (button, gaming mode, Ctrl+Shift+F, or browser UI)
  // folds the dashboard so the user lands in the session.
  React.useEffect(() => {
    const foldOnFullscreen = () => {
      if (document.fullscreenElement) {
        setActivePanel(null);
      }
    };
    document.addEventListener("fullscreenchange", foldOnFullscreen);
    return () => document.removeEventListener("fullscreenchange", foldOnFullscreen);
  }, []);

  // The first touch enables the touch-specific entries for the session.
  React.useEffect(() => {
    const detectTouch = () => {
      console.log("Dashboard: First touch detected. Enabling touch-specific features.");
      setHasDetectedTouch(true);
      window.removeEventListener('touchstart', detectTouch, { capture: true });
    };
    // In the capture phase: in trackpad mode the stream's own handler stops
    // the touch from bubbling, and the first touch is usually on the stream.
    window.addEventListener('touchstart', detectTouch, { passive: true, capture: true } as AddEventListenerOptions);
    return () => {
      window.removeEventListener('touchstart', detectTouch, { capture: true });
    };
  }, []);

  // Recenters the menu on its measured width.
  React.useEffect(() => {
    if (dragRef.current) {
      const menuWidth = dragRef.current.offsetWidth;
      const centerX = (window.innerWidth - menuWidth) / 2;
      setPosition(prev => ({ ...prev, x: centerX }));
    }
  }, []);

  /** Starts dragging the menu bar. */
  const handleMouseDown = (e: React.MouseEvent) => {
    setIsDragging(true);
    startPosRef.current = {
      x: e.clientX - position.x,
      y: e.clientY - position.y,
    };
  };

  React.useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (!isDragging) return;

      const newX = e.clientX - startPosRef.current.x;
      const newY = e.clientY - startPosRef.current.y;

      // Measured size bounds the drag; the constants stand in until the ref is set.
      const menuElement = dragRef.current;
      const menuWidth = menuElement ? menuElement.offsetWidth : 600;
      const menuHeight = menuElement ? menuElement.offsetHeight : 100;

      const maxX = window.innerWidth - menuWidth;
      const maxY = window.innerHeight - menuHeight;

      setPosition({
        x: Math.max(0, Math.min(newX, maxX)),
        y: Math.max(0, Math.min(newY, maxY)),
      });
    };

    const handleMouseUp = () => {
      setIsDragging(false);
    };

    if (isDragging) {
      document.addEventListener('mousemove', handleMouseMove);
      document.addEventListener('mouseup', handleMouseUp);
    }

    return () => {
      document.removeEventListener('mousemove', handleMouseMove);
      document.removeEventListener('mouseup', handleMouseUp);
    };
  }, [isDragging]);

  // An outside click closes the active panel; the System Monitoring overlay
  // is not a panel and stays.
  React.useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (!activePanel) return;
      const target = event.target as Node;

      const isOutsideMainMenu = dragRef.current && !dragRef.current.contains(target);
      const isOutsidePanel = panelRef.current && !panelRef.current.contains(target);

      // Base UI portals a menu popup to the body — the Settings dropdowns —
      // outside the panel element, so a click in one still belongs to the
      // panel's controls.
      const element = target instanceof Element ? target : null;
      const isOnMenuPopup = element !== null && element.closest('[data-slot="dropdown-menu-content"], [data-base-ui-portal]') !== null;
      const isOnMenuTrigger = element !== null && element.closest('[data-slot="dropdown-menu-trigger"]') !== null;

      if (isOutsideMainMenu && isOutsidePanel && !isOnMenuPopup && !isOnMenuTrigger) {
        setActivePanel(null);
      }
    };

    document.addEventListener('mousedown', handleClickOutside);

    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
    };
  }, [activePanel]);

  // Escape closes the active panel the way it closes the menus; the capture
  // phase keeps the key from reaching the stream as well.
  React.useEffect(() => {
    if (!activePanel) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      // A popup layered on the panel — a settings dropdown — takes the Escape
      // first, so the panel survives the key that only closes the popup.
      const popup = document.querySelector('[data-slot="dropdown-menu-content"][data-open]');
      if (popup) return;
      event.preventDefault();
      event.stopPropagation();
      setActivePanel(null);
    };
    window.addEventListener('keydown', handleKeyDown, true);
    return () => window.removeEventListener('keydown', handleKeyDown, true);
  }, [activePanel]);

  /**
   * Opens or closes the Settings panel. Apps is a modal and monitoring is an
   * overlay that closes the panel; the overflow menu carries the rest of the
   * tools in its own popup.
   */
  const handlePanelToggle = (panelName: string) => {
    if (panelName === 'apps') {
      setShowAppsModal(true);
      return;
    }

    if (panelName === 'monitoring') {
      setShowSystemMonitoring(prev => !prev);
      setActivePanel(null);
      return;
    }

    const newPanel = activePanel === panelName ? null : panelName;
    setActivePanel(newPanel);
    if (newPanel) {
      setShowSystemMonitoring(false);
    }
  };

  /** Slides the toolbar up behind the top edge, folding what hangs from it. */
  const collapseToolbar = () => {
    setActivePanel(null);
    setOverflowOpen(false);
    setIsCollapsed(true);
  };

  /** Switches touch input between trackpad and direct-touch mode on the core. */
  const handleToggleTrackpadMode = React.useCallback(() => {
    const newActiveState = !isTrackpadModeActive;
    setIsTrackpadModeActive(newActiveState);
    const message = newActiveState ? "touchinput:trackpad" : "touchinput:touch";
    console.log(`Dashboard: Toggling trackpad mode. Sending: ${message}`);
    window.postMessage({ type: message }, window.location.origin);
  }, [isTrackpadModeActive]);

  /**
   * Opens the secondary display in a new window, sized to `screen` when the
   * Window Management API supplied one.
   * @param direction Side of the primary the new display attaches to.
   * @param screen A ScreenDetailed to place the window on, or null.
   * @returns Whether the window opened.
   */
  const launchWindow = (direction: string, screen: any = null) => {
    const url = `${window.location.href.split('#')[0]}${fragmentWithSessionToken(`display2-${direction}`)}`;
    // Not `noopener` in the features: that makes window.open return null even
    // when it opened, leaving a refusal indistinguishable from success. The
    // opener is severed on the handle instead.
    let features = 'resizable=yes,scrollbars=yes';
    if (screen) {
      features += `,left=${screen.availLeft},top=${screen.availTop},width=${screen.availWidth},height=${screen.availHeight}`;
    }
    const opened = window.open(url, '_blank', features);
    if (!opened) {
      // Refused from an async continuation, whose click activation is spent (all
      // the more after a permission prompt). An arrow click is a fresh one.
      console.warn('Second display window was blocked; leaving the placement arrows up.');
      return false;
    }
    try { opened.opener = null; } catch { /* already navigated away */ }
    setAvailablePlacements(null);
    return true;
  };

  /** Every side, with no screen to place the window on: what the arrows offer
   *  when nothing can be measured to choose a side from. */
  const ANY_SIDE = { up: null, down: null, left: null, right: null };

  /**
   * Adds a secondary display. With the Window Management API a single adjacent
   * screen is used directly; anything else offers the placement arrows. Asking
   * beats guessing: the API answers nothing without the window-management
   * permission, and a display silently opened to the right of a monitor that
   * sits above or left of this one is what the arrows exist to avoid. A
   * refused popup falls back to the arrows too, so the button is never seen to
   * do nothing at all.
   */
  const handleAddScreenClick = async () => {
    if (!('getScreenDetails' in window)) {
      console.warn("Window Management API not supported; asking which side.");
      setAvailablePlacements(ANY_SIDE);
      return;
    }

    try {
      const screenDetails = await (window as any).getScreenDetails();
      const currentScreen = screenDetails.currentScreen;
      const otherScreens = screenDetails.screens.filter((s: any) => s !== currentScreen);

      if (otherScreens.length === 0) {
        console.log("No other screens detected; asking which side.");
        setAvailablePlacements(ANY_SIDE);
        return;
      }

      const placements: any = {};
      for (const s of otherScreens) {
        if (!placements.right && s.left >= currentScreen.left + currentScreen.width) {
          placements.right = s;
        }
        if (!placements.left && s.left + s.width <= currentScreen.left) {
          placements.left = s;
        }
        if (!placements.down && s.top >= currentScreen.top + currentScreen.height) {
          placements.down = s;
        }
        if (!placements.up && s.top + s.height <= currentScreen.top) {
          placements.up = s;
        }
      }

      const availableDirections = Object.keys(placements);

      if (availableDirections.length === 1) {
        const direction = availableDirections[0];
        const screen = placements[direction];
        console.log(`Auto-placing single screen to the ${direction}.`);
        if (!launchWindow(direction, screen)) setAvailablePlacements(placements);
      } else if (availableDirections.length > 1) {
        console.log("Multiple placement options found. Showing arrows.");
        setAvailablePlacements(placements);
      } else {
        console.log("No adjacent screens found in cardinal directions; asking which side.");
        setAvailablePlacements(ANY_SIDE);
      }
    } catch (err: any) {
      // A refused permission is an ordinary outcome — the arrows still ask,
      // with no screen to place the window on — so it is not a fault.
      if (err && err.name === "NotAllowedError") {
        console.warn("Window Management permission refused; asking which side.");
      } else {
        console.error("Error with Window Management API:", err);
      }
      setAvailablePlacements(ANY_SIDE);
    }
  };

  /**
   * Pops the on-screen keyboard by focusing the core's keyboard-assist input,
   * releasing it again on the next touch of the interaction overlay.
   */
  const handleShowVirtualKeyboard = React.useCallback(() => {
    console.log("Dashboard: Directly handling virtual keyboard pop.");
    // Asked for the on-screen keyboard: an attached one is no longer assumed, and the
    // floating button that pops it comes back (lib/hardware-keyboard.js).
    hardwareKeyboard().reset();
    const kbdAssistInput = document.getElementById('keyboard-input-assist');
    const mainInteractionOverlay = document.getElementById('overlayInput');
    if (kbdAssistInput) {
      (kbdAssistInput as HTMLInputElement).removeAttribute('aria-hidden');
      (kbdAssistInput as HTMLInputElement).value = '';
      (kbdAssistInput as HTMLInputElement).focus();
      console.log("Focused #keyboard-input-assist element to pop keyboard.");
      if (mainInteractionOverlay) {
        mainInteractionOverlay.addEventListener(
          "touchstart",
          () => {
            if (document.activeElement === kbdAssistInput) {
              (kbdAssistInput as HTMLInputElement).blur();
              console.log("Blurred #keyboard-input-assist on main overlay touch.");
              kbdAssistInput.setAttribute('aria-hidden', 'true');
            }
          }, {
          once: true,
          passive: true
        }
        );
      } else {
        console.warn("Could not find #overlayInput to attach blur listener.");
      }
    } else {
      console.error("Could not find #keyboard-input-assist element to focus.");
    }
  }, []);

  /** Dispatches a synthetic KeyboardEvent on `window` for the core's input handler. */
  const sendKeyEvent = (type: string, key: string, code: string, modifierState: any) => {
    const event = new KeyboardEvent(type, {
      key: key,
      code: code,
      ctrlKey: modifierState.Control,
      altKey: modifierState.Alt,
      metaKey: modifierState.Meta,
      shiftKey: !!modifierState.Shift,
      bubbles: true,
      cancelable: true,
    });
    window.dispatchEvent(event);
  };

  /**
   * Toggles a held modifier soft key; the core's synthetic-key mode is raised
   * while any modifier is held.
   */
  const handleHoldKeyClick = (key: string, code: string) => {
    const isCurrentlyHeld = heldKeys[key as keyof typeof heldKeys];
    const currentHeldCount = Object.values(heldKeys).filter(Boolean).length;
    if (!isCurrentlyHeld && currentHeldCount === 0) {
      window.postMessage({ type: 'setSynth', value: true }, window.location.origin);
    } else if (isCurrentlyHeld && currentHeldCount === 1) {
      window.postMessage({ type: 'setSynth', value: false }, window.location.origin);
    }
    const nextHeldState = {
      ...heldKeys,
      [key]: !isCurrentlyHeld,
    };
    setHeldKeys(nextHeldState);
    if (isCurrentlyHeld) {
      sendKeyEvent('keyup', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keyup for ${key} with state:`, nextHeldState);
    } else {
      sendKeyEvent('keydown', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keydown for ${key} with state:`, nextHeldState);
    }
  };

  /** Presses a soft key once, with the held modifiers applied. */
  const handleOnceKeyClick = (key: string, code: string) => {
    console.log(`Dashboard: Dispatching key press for ${key} with modifiers:`, heldKeys);
    sendKeyEvent('keydown', key, code, heldKeys);
    setTimeout(() => {
      sendKeyEvent('keyup', key, code, heldKeys);
    }, 50);
  };

  /**
   * Plays a palette chord (`lib/touch-controls.js`) as the soft keys do, with
   * synthetic mode on for its length unless a soft modifier already has it:
   * its modifiers and key pressed, then after 50 ms released in reverse. A
   * modifier held on a soft key stays held.
   */
  const playChord = (text: string) => {
    const chord = parseChord(text);
    if (!chord) return;
    const holding = Object.values(heldKeys).some(Boolean);
    if (!holding) window.postMessage({ type: 'setSynth', value: true }, window.location.origin);
    const events = chordEvents(chord, heldKeys);
    const firstUp = events.findIndex((e: any) => e.type === 'keyup');
    events.slice(0, firstUp).forEach((e: any) => sendKeyEvent(e.type, e.key, e.code, e.state));
    setTimeout(() => {
      events.slice(firstUp).forEach((e: any) => sendKeyEvent(e.type, e.key, e.code, e.state));
      if (!holding) window.postMessage({ type: 'setSynth', value: false }, window.location.origin);
    }, 50);
  };

  /** Adds the chord typed into the palette to the user's own, kept per origin. */
  const handleAddChord = (event: React.FormEvent) => {
    event.preventDefault();
    const chord = parseChord(chordDraft);
    if (!chord) {
      setChordRefused(true);
      return;
    }
    const name = formatChord(chord);
    const next = userChords.includes(name) ? userChords : [...userChords, name];
    setUserChords(next);
    writeUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY), next);
    setChordDraft("");
    setChordRefused(false);
  };

  /** Forgets one of the user's own chords. */
  const handleRemoveChord = (name: string) => {
    const next = userChords.filter((c) => c !== name);
    setUserChords(next);
    writeUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY), next);
  };

  /** The trackpad's speed is client-only; the core persists trackpad_speed itself. */
  const handleTrackpadSpeed = (value: number) => {
    setTrackpadSpeed(value);
    window.postMessage({ type: 'setTrackpadSpeed', value }, window.location.origin);
  };

  /** Opens the Download Files dialog and folds the overflow menu behind it. */
  const openDownloadsModal = () => {
    setOverflowOpen(false);
    setShowFilesModal(true);
  };



  // The overflow menu leads with the stream switches, so they render only
  // when at least one of them is available.
  const streamsAvailable =
    (renderableSettings.videoToggle ?? true) ||
    (renderableSettings.audioToggle ?? true) ||
    (renderableSettings.microphoneToggle ?? true) ||
    (renderableSettings.webcamToggle ?? true);

  // The overflow menu carries the stream switches and the tools a session
  // reaches for only occasionally, so it renders only when at least one of
  // them is available.
  const overflowAvailable =
    streamsAvailable ||
    ((renderableSettings.apps ?? true) && !isSecondaryDisplay) ||
    (renderableSettings.shortcuts ?? true) ||
    (!isSecondaryDisplay &&
      ((renderableSettings.clipboard ?? true) ||
        (renderableSettings.files ?? true) ||
        printJobCount > 0 ||
        (renderableSettings.sharing ?? true) ||
        !!serverSettings?.second_screen?.value));

  return (
    <>
      <motion.div
        ref={dragRef}
        className={`fixed left-0 z-50 w-fit rounded-lg border bg-background shadow-lg`}
        style={{
          transform: `translate(${position.x}px, ${position.y}px)`,
          // `top` carries the slide so the horizontal placement never animates.
          top: isCollapsed ? -(TOOLBAR_HEIGHT + position.y) : 0,
          transition: 'top 300ms',
        }}
      >
        <div className="flex items-center gap-1.5 px-2 py-2">
          {uiShowLogo && (
            <a
              href={PROJECT_URL}
              target="_blank"
              rel="noopener noreferrer"
              aria-label={uiTitle}
              className="flex items-center pr-1 hover:text-primary transition-colors"
            >
              <SelkiesLogo width={20} height={20} />
            </a>
          )}

          {/* The overflow menu: the stream switches, the apps panel, then the
              tools a session reaches for only occasionally, and second-screen
              placement. Each tool nests its own popup off the list, so the
              list stays put while the tool is used. */}
          {overflowAvailable && (
            <Menu
              open={overflowOpen}
              onOpenChange={(open) => {
                setOverflowOpen(open);
                // The menu and the settings panel never share the screen:
                // the menu folds the panel as the panel's button folds the
                // menu.
                if (open) setActivePanel(null);
              }}
            >
              <Tooltip>
                <TooltipTrigger
                  render={
                    <MenuTrigger
                      render={
                        <Button
                          variant="secondary"
                          size="icon"
                          className="h-6 w-6"
                          aria-label={t('topMenu.menu')}
                        >
                          <ToolCase className="h-4 w-4" />
                        </Button>
                      }
                    />
                  }
                />
                <TooltipContent>{t('topMenu.menu')}</TooltipContent>
              </Tooltip>
              {/* The offset is measured from the trigger, which sits inside the
                  bar's padding, so 15px puts the popup 6px under the bar as the
                  Settings panel is. */}
              <MenuPopup align="start" sideOffset={15}>
                {/* Stream switches, one per direction the session carries,
                    above the occasional tools. */}
                {!isSecondaryDisplay && (renderableSettings.coreButtons ?? true) && streamsAvailable && (
                  <>
                    {(renderableSettings.videoToggle ?? true) && (
                      <MenuCheckboxItem
                        variant="switch"
                        data-testid="video-stream-toggle"
                        checked={isVideoActive}
                        onCheckedChange={onVideoToggle}
                      >
                        <span className="flex items-center gap-2">
                          <Monitor />
                          {t('topMenu.videoStream')}
                        </span>
                      </MenuCheckboxItem>
                    )}
                    {(renderableSettings.audioToggle ?? true) && (
                      <MenuCheckboxItem
                        variant="switch"
                        data-testid="audio-stream-toggle"
                        checked={isAudioActive}
                        onCheckedChange={onAudioToggle}
                      >
                        <span className="flex items-center gap-2">
                          <Volume2 />
                          {t('topMenu.audioStream')}
                        </span>
                      </MenuCheckboxItem>
                    )}
                    {(renderableSettings.microphoneToggle ?? true) && (
                      <MenuCheckboxItem
                        variant="switch"
                        data-testid="microphone-toggle"
                        checked={isMicrophoneActive}
                        onCheckedChange={onMicrophoneToggle}
                      >
                        <span className="flex items-center gap-2">
                          <Mic />
                          {t('topMenu.microphone')}
                        </span>
                      </MenuCheckboxItem>
                    )}
                    {(renderableSettings.webcamToggle ?? true) && (
                      <MenuCheckboxItem
                        variant="switch"
                        data-testid="webcam-toggle"
                        checked={isWebcamActive}
                        onCheckedChange={onWebcamToggle}
                      >
                        <span className="flex items-center gap-2">
                          <Webcam />
                          {t('topMenu.webcam')}
                        </span>
                      </MenuCheckboxItem>
                    )}
                    <MenuSeparator />
                  </>
                )}
                {/* The apps modal opens over the session, so alone of the
                    items it folds the menu. */}
                {(renderableSettings.apps ?? true) && !isSecondaryDisplay && (
                  <MenuItem onClick={() => handlePanelToggle('apps')}>
                    <LayoutGrid />
                    {t('sections.apps.title')}
                  </MenuItem>
                )}
                {!isSecondaryDisplay && (renderableSettings.clipboard ?? true) && (
                  <MenuSub>
                    <MenuSubTrigger>
                      <ClipboardIcon />
                      {t('sections.clipboard.title')}
                    </MenuSubTrigger>
                    <MenuSubPopup className="w-[340px]">
                      <Clipboard />
                    </MenuSubPopup>
                  </MenuSub>
                )}
                {!isSecondaryDisplay && (renderableSettings.files ?? true) && (
                  <MenuSub>
                    <MenuSubTrigger>
                      <FileText />
                      {t('sections.files.title')}
                    </MenuSubTrigger>
                    <MenuSubPopup>
                      <Files onOpenDownloads={openDownloadsModal} />
                    </MenuSubPopup>
                  </MenuSub>
                )}
                {!isSecondaryDisplay && printJobCount > 0 && (
                  <MenuSub>
                    <MenuSubTrigger>
                      <Printer />
                      {t('sections.printing.title')}
                    </MenuSubTrigger>
                    <MenuSubPopup>
                      <Printing />
                    </MenuSubPopup>
                  </MenuSub>
                )}
                {!isSecondaryDisplay && (renderableSettings.sharing ?? true) && (
                  <MenuSub>
                    <MenuSubTrigger>
                      <Share2 />
                      {t('sections.sharing.title')}
                    </MenuSubTrigger>
                    <MenuSubPopup>
                      <Sharing show={true} />
                    </MenuSubPopup>
                  </MenuSub>
                )}
                {(renderableSettings.shortcuts ?? true) && (
                  <MenuSub>
                    <MenuSubTrigger>
                      <Keyboard />
                      {t('sections.shortcuts.title')}
                    </MenuSubTrigger>
                    <MenuSubPopup>
                      <ShortcutsMenu />
                    </MenuSubPopup>
                  </MenuSub>
                )}
                {/* second_screen is effective availability (admin flag AND backend
                    capacity) and the server rejects secondaries it cannot back, so
                    the entry follows it rather than offering a window that would
                    be killed. */}
                {!isSecondaryDisplay && serverSettings?.second_screen?.value && (
                  <>
                    <MenuSeparator />
                    <MenuItem onClick={handleAddScreenClick}>
                      <ScreenShare />
                      {t('sections.screen.addScreenTitle')}
                    </MenuItem>
                  </>
                )}
                {/* The footer: the branding and the theme toggle at its end,
                    the toggle pushed right whether or not the branding
                    shows. */}
                <MenuSeparator />
                <div className="flex items-center px-2 py-1.5">
                  {(uiShowLogo || uiTitle) && (
                    <a
                      href={PROJECT_URL}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground transition-colors hover:text-primary"
                    >
                      {uiShowLogo && <SelkiesLogo width={16} height={16} />}
                      <span>{uiTitle}</span>
                    </a>
                  )}
                  <span className="ms-auto">
                    <ModeToggle />
                  </span>
                </div>
              </MenuPopup>
            </Menu>
          )}

          {/* Session controls: the panels, the monitoring overlay, and the
              display itself. */}
          <ButtonGroup>
            <Tooltip>
              <TooltipTrigger
                render={
                  <Button
                    variant={activePanel === 'settings' ? "default" : "secondary"}
                    size="icon"
                    className="h-6 w-6"
                    aria-label={t('topMenu.settings')}
                    aria-expanded={activePanel === 'settings'}
                    onClick={() => handlePanelToggle('settings')}
                  />
                }
              >
                <Settings2 className="h-4 w-4" />
              </TooltipTrigger>
              <TooltipContent>{t('topMenu.settings')}</TooltipContent>
            </Tooltip>

            {!isSecondaryDisplay && (((renderableSettings.coreButtons ?? true) && (renderableSettings.gamepadToggle ?? true)) || (renderableSettings.gamepads ?? true)) && (
              <Tooltip>
                <TooltipTrigger
                  render={
                    <Button
                      variant={showGamepads ? "default" : "secondary"}
                      size="icon"
                      className="h-6 w-6"
                      aria-label={t('sections.gamepads.title')}
                      aria-pressed={showGamepads}
                      onClick={() => setShowGamepads((open) => !open)}
                    />
                  }
                >
                  <Joystick className="h-4 w-4" />
                </TooltipTrigger>
                <TooltipContent>{t('sections.gamepads.title')}</TooltipContent>
              </Tooltip>
            )}

            {(renderableSettings.stats ?? true) && !isSecondaryDisplay && (
              <Tooltip>
                <TooltipTrigger
                  render={
                    <Button
                      variant={showSystemMonitoring ? "default" : "secondary"}
                      size="icon"
                      className="h-6 w-6"
                      aria-label={t('topMenu.systemMonitoring')}
                      aria-pressed={showSystemMonitoring}
                      onClick={() => handlePanelToggle('monitoring')}
                    />
                  }
                >
                  <Gauge className="h-4 w-4" />
                </TooltipTrigger>
                <TooltipContent>{t('topMenu.systemMonitoring')}</TooltipContent>
              </Tooltip>
            )}

            {(renderableSettings.fullscreen ?? true) && (
              <Tooltip>
                <TooltipTrigger
                  render={
                    <Button
                      variant="secondary"
                      size="icon"
                      className="h-6 w-6"
                      aria-label={t('topMenu.toggleFullscreen')}
                      onClick={() => toggleFullscreen('requestFullscreen')}
                    />
                  }
                >
                  <Maximize className="h-4 w-4" />
                </TooltipTrigger>
                <TooltipContent>{t('topMenu.toggleFullscreen')}</TooltipContent>
              </Tooltip>
            )}
          </ButtonGroup>

          {/* Gaming mode; the gamepad input toggle and the preview live in
              the overflow menu's Gamepads view. */}
          {(renderableSettings.gamingMode ?? true) && (
            <Tooltip>
              <TooltipTrigger
                render={
                  <Button
                    variant="secondary"
                    size="icon"
                    className="h-6 w-6"
                    aria-label={t('gamingModeTitle')}
                    onClick={() => toggleFullscreen('requestGamingMode')}
                  />
                }
              >
                <Crosshair className="h-4 w-4" />
              </TooltipTrigger>
              <TooltipContent>{`${t('gamingModeTitle')} \u2014 ${t('gamingModeHint')}`}</TooltipContent>
            </Tooltip>
          )}

          {/* Touch controls, from the first touch the session sees. */}
          {(isMobile || hasDetectedTouch) && (
            <ButtonGroup>
              {!isSecondaryDisplay && (
                <Tooltip>
                  <TooltipTrigger
                    render={
                      <Button
                        variant={isTouchGamepadActive ? "default" : "secondary"}
                        size="icon"
                        className="h-6 w-6"
                        aria-label={t('topMenu.touchGamepad')}
                        aria-pressed={isTouchGamepadActive}
                        onClick={onToggleTouchGamepad}
                      />
                    }
                  >
                    <Gamepad2 className="h-4 w-4" />
                  </TooltipTrigger>
                  <TooltipContent>{t('topMenu.touchGamepad')}</TooltipContent>
                </Tooltip>
              )}

              {(renderableSettings.trackpad ?? true) && (
                <Tooltip>
                  <TooltipTrigger
                    render={
                      <Button
                        variant={isTrackpadModeActive ? "default" : "secondary"}
                        size="icon"
                        className="h-6 w-6"
                        aria-label={t('trackpadModeTitle')}
                        aria-pressed={isTrackpadModeActive}
                        onClick={handleToggleTrackpadMode}
                      />
                    }
                  >
                    <Touchpad className="h-4 w-4" />
                  </TooltipTrigger>
                  <TooltipContent>{t('trackpadModeTitle')}</TooltipContent>
                </Tooltip>
              )}

              {(renderableSettings.keyboardButton ?? true) && (
                <Tooltip>
                  <TooltipTrigger
                    render={
                      <Button
                        variant={isKeyboardButtonVisible && !keyboardAttached ? "default" : "secondary"}
                        size="icon"
                        className="h-6 w-6"
                        aria-label={t('keyboardButtonToggleTitle', 'Keyboard Button')}
                        aria-pressed={isKeyboardButtonVisible && !keyboardAttached}
                        onClick={toggleKeyboardButton}
                      />
                    }
                  >
                    <Keyboard className="h-4 w-4" />
                  </TooltipTrigger>
                  <TooltipContent>{t('keyboardButtonToggleTitle', 'Keyboard Button')}</TooltipContent>
                </Tooltip>
              )}
            </ButtonGroup>
          )}

          <Button
            variant="secondary"
            size="icon"
            className="h-6 w-6 cursor-grab active:cursor-grabbing select-none"
            aria-label={t('topMenu.dragHandle')}
            onMouseDown={handleMouseDown}
          >
            <Hand className="h-4 w-4" />
          </Button>

          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  variant="secondary"
                  size="icon"
                  className="h-6 w-6"
                  aria-label={t('topMenu.hideToolbar')}
                  onClick={collapseToolbar}
                />
              }
            >
              <ChevronUp className="h-4 w-4" />
            </TooltipTrigger>
            <TooltipContent>{t('topMenu.hideToolbar')}</TooltipContent>
          </Tooltip>
        </div>

        {/* What stays in view while the bar is up behind the top edge: a tab
            hanging from it that brings it back. */}
        {isCollapsed && (
          <button
            type="button"
            aria-label={t('topMenu.showToolbar')}
            title={t('topMenu.showToolbar')}
            onClick={() => setIsCollapsed(false)}
            className="absolute left-1/2 top-full flex h-4 w-12 -translate-x-1/2 items-center justify-center rounded-b-md border border-t-0 bg-background text-muted-foreground hover:text-foreground"
          >
            <ChevronDown className="h-3 w-3" />
          </button>
        )}
      </motion.div>



      <AnimatePresence>
        {showSystemMonitoring && !isSecondaryDisplay && (renderableSettings.stats ?? true) && (
          <motion.div
            ref={monitoringRef}
            initial={{ opacity: 0, scale: 0.95 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.95 }}
            style={{
              position: 'fixed',
              left: monitoringPanel.position.x,
              top: monitoringPanel.position.y,
              // Held to the window below its top, with a margin under it, and
              // scrolled within, the panel stays whole wherever the drag clamp puts it.
              display: 'flex',
              flexDirection: 'column',
              maxHeight: `calc(100dvh - ${monitoringPanel.position.y}px - 1rem)`,
              zIndex: 30,
              cursor: monitoringPanel.isDragging ? 'grabbing' : undefined
            }}
            onMouseDown={monitoringPanel.onMouseDown}
          >
            <SystemMonitoring />
          </motion.div>
        )}
      </AnimatePresence>

      <AnimatePresence>
        {showGamepads && (
          <motion.div
            ref={gamepadRef}
            initial={{ opacity: 0, scale: 0.95 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.95 }}
            style={{
              position: 'fixed',
              left: gamepadPanel.position.x,
              top: gamepadPanel.position.y,
              display: 'flex',
              flexDirection: 'column',
              maxHeight: `calc(100dvh - ${gamepadPanel.position.y}px - 1rem)`,
              zIndex: 30,
              cursor: gamepadPanel.isDragging ? 'grabbing' : undefined
            }}
            onMouseDown={gamepadPanel.onMouseDown}
          >
            <GamepadPanel
              isGamepadEnabled={isGamepadEnabled}
              onGamepadToggle={onGamepadToggle}
              isTouchGamepadActive={isTouchGamepadActive}
              onToggleTouchGamepad={onToggleTouchGamepad}
              showInputToggle={(renderableSettings.coreButtons ?? true) && (renderableSettings.gamepadToggle ?? true)}
              showPads={renderableSettings.gamepads ?? true}
              onClose={() => setShowGamepads(false)}
            />
          </motion.div>
        )}
      </AnimatePresence>

      <AnimatePresence>
        {activePanel === 'settings' && (
          <motion.div
            ref={panelRef}
            initial={{ opacity: 0, y: -20 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -20 }}
            className="fixed z-20 w-fit"
            style={{
              left: position.x,
              top: position.y + 48,
            }}
          >
            <Settings />
          </motion.div>
        )}
      </AnimatePresence>

      {(isMobile || hasDetectedTouch) &&
        ((renderableSettings.softButtons ?? true) || (renderableSettings.trackpad ?? true)) && (
        <motion.div
          ref={softKeysRef}
          data-soft-keys
          className="fixed bottom-4 left-4 z-40 flex flex-col gap-2 p-2 rounded-lg border bg-card shadow-lg"
          style={{ maxWidth: 'calc(100vw - 2rem)', maxHeight: 'calc(100dvh - 2rem)' }}
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
        >
          {/* The palette opens above the soft keys and scrolls within what the
              viewport leaves, so the keys and its toggle stay at the bottom and
              in reach on a phone in either orientation and either touch mode. */}
          {(renderableSettings.softButtons ?? true) && isKeyPaletteOpen && (
            <div className="key-palette flex min-h-0 max-w-[22rem] flex-col gap-2 overflow-y-auto">
              <div className="grid grid-cols-6 gap-1">
                {PALETTE_KEYS.map(([label, key, code]: string[]) => (
                  <Button
                    key={code}
                    variant="secondary"
                    size="sm"
                    data-code={code}
                    onClick={() => handleOnceKeyClick(key, code)}
                    onMouseDown={(e) => e.preventDefault()}
                  >
                    {label}
                  </Button>
                ))}
              </div>
              <div className="flex flex-wrap gap-1">
                {PALETTE_CHORDS.map((chord: string) => (
                  <Button
                    key={chord}
                    variant="secondary"
                    size="sm"
                    data-chord={chord}
                    onClick={() => playChord(chord)}
                    onMouseDown={(e) => e.preventDefault()}
                  >
                    {chord}
                  </Button>
                ))}
                {userChords.map((chord) => (
                  <span key={chord} className="flex gap-0.5">
                    <Button
                      variant="secondary"
                      size="sm"
                      data-chord={chord}
                      onClick={() => playChord(chord)}
                      onMouseDown={(e) => e.preventDefault()}
                    >
                      {chord}
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      aria-label={t('keyPalette.remove', { chord })}
                      title={t('keyPalette.remove', { chord })}
                      onClick={() => handleRemoveChord(chord)}
                      onMouseDown={(e) => e.preventDefault()}
                    >
                      ×
                    </Button>
                  </span>
                ))}
              </div>
              <form className="flex gap-1" onSubmit={handleAddChord}>
                <input
                  type="text"
                  className="key-palette-input allow-native-input min-w-0 flex-1 rounded border bg-background px-2 py-1 text-xs"
                  value={chordDraft}
                  onChange={(e) => { setChordDraft(e.target.value); setChordRefused(false); }}
                  placeholder={t('keyPalette.addPlaceholder')}
                  aria-label={t('keyPalette.addPlaceholder')}
                  autoCapitalize="off"
                  autoCorrect="off"
                  spellCheck={false}
                />
                <Button type="submit" variant="secondary" size="sm">{t('keyPalette.add')}</Button>
              </form>
              {chordRefused && (
                <p className="text-xs text-muted-foreground">{t('keyPalette.refused')}</p>
              )}
            </div>
          )}
          <div className="flex shrink-0 flex-wrap gap-2">
          {(renderableSettings.softButtons ?? true) && (<>
          <Button
            variant={heldKeys.Control ? "default" : "secondary"}
            size="sm"
            onClick={() => handleHoldKeyClick('Control', 'ControlLeft')}
            onMouseDown={(e) => e.preventDefault()}
          >
            CTRL
          </Button>
          <Button
            variant={heldKeys.Alt ? "default" : "secondary"}
            size="sm"
            onClick={() => handleHoldKeyClick('Alt', 'AltLeft')}
            onMouseDown={(e) => e.preventDefault()}
          >
            ALT
          </Button>
          <Button
            variant={heldKeys.Meta ? "default" : "secondary"}
            size="sm"
            onClick={() => handleHoldKeyClick('Meta', 'MetaLeft')}
            onMouseDown={(e) => e.preventDefault()}
          >
            WIN
          </Button>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => handleOnceKeyClick('Tab', 'Tab')}
            onMouseDown={(e) => e.preventDefault()}
          >
            TAB
          </Button>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => handleOnceKeyClick('Escape', 'Escape')}
            onMouseDown={(e) => e.preventDefault()}
          >
            ESC
          </Button>
          {(renderableSettings.keyboardButton ?? true) && (
            <Button
              variant="secondary"
              size="sm"
              onClick={handleShowVirtualKeyboard}
            >
              <Keyboard className="h-4 w-4" />
            </Button>
          )}
          </>)}
          {(renderableSettings.trackpad ?? true) && (
            <Button
              variant={isTrackpadModeActive ? "default" : "secondary"}
              size="sm"
              onClick={handleToggleTrackpadMode}
              title={t('trackpadModeTitle')}
            >
              <Touchpad className="h-4 w-4" />
            </Button>
          )}
          {(renderableSettings.trackpad ?? true) && isTrackpadModeActive && (
            <label className="flex items-center gap-1 text-xs">
              <span>{t('trackpadSpeedLabel')}</span>
              <select
                className="rounded border bg-background px-1 py-0.5 text-xs"
                value={trackpadSpeed}
                onChange={(e) => handleTrackpadSpeed(Number(e.target.value))}
              >
                {TRACKPAD_SPEEDS.map((v: number) => (
                  <option key={v} value={v}>{`${v}\u00d7`}</option>
                ))}
              </select>
            </label>
          )}
          {(renderableSettings.softButtons ?? true) && (
            <Button
              variant={isKeyPaletteOpen ? "default" : "secondary"}
              size="sm"
              className="key-palette-toggle"
              aria-expanded={isKeyPaletteOpen}
              onClick={() => setIsKeyPaletteOpen((open) => !open)}
              onMouseDown={(e) => e.preventDefault()}
            >
              {isKeyPaletteOpen ? t('keyPalette.less') : t('keyPalette.more')}
            </Button>
          )}
          </div>
        </motion.div>
      )}

      {availablePlacements && (
        <div
          className="screen-placement-overlay fixed inset-0 z-50 pointer-events-auto"
          onClick={() => setAvailablePlacements(null)}
        >
          {availablePlacements.up !== undefined && (
            <Button
              className="absolute top-10 left-1/2 transform -translate-x-1/2 w-24 h-24 text-4xl"
              onClick={(e) => {
                e.stopPropagation();
                launchWindow('up', availablePlacements.up);
              }}
            >
              ▲
            </Button>
          )}
          {availablePlacements.down !== undefined && (
            <Button
              className="absolute bottom-10 left-1/2 transform -translate-x-1/2 w-24 h-24 text-4xl"
              onClick={(e) => {
                e.stopPropagation();
                launchWindow('down', availablePlacements.down);
              }}
            >
              ▼
            </Button>
          )}
          {availablePlacements.left !== undefined && (
            <Button
              className="absolute left-10 top-1/2 transform -translate-y-1/2 w-24 h-24 text-4xl"
              onClick={(e) => {
                e.stopPropagation();
                launchWindow('left', availablePlacements.left);
              }}
            >
              ◄
            </Button>
          )}
          {availablePlacements.right !== undefined && (
            <Button
              className="absolute right-10 top-1/2 transform -translate-y-1/2 w-24 h-24 text-4xl"
              onClick={(e) => {
                e.stopPropagation();
                launchWindow('right', availablePlacements.right);
              }}
            >
              ►
            </Button>
          )}
        </div>
      )}

      {showAppsModal && (
        <Apps isOpen={showAppsModal} onClose={() => setShowAppsModal(false)} />
      )}

      {/* Files dialog, beside the toolbar like the Apps modal: a click in its
          iframe lands outside the panel, which closes on it, so the dialog
          cannot live inside the Files panel. */}
      {showFilesModal && (
        <FilesDialog open={showFilesModal} onOpenChange={setShowFilesModal} />
      )}
    </>
  );
}