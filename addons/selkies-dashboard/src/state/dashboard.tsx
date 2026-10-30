/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The state every layout reads and every panel shares, held once above the
 * layout so a layout switch loses nothing and the Ctrl+Shift chords keep
 * working while a layout has its chrome folded.
 *
 * State follows the core's echoes rather than local toggles: a
 * `pipelineControl` or `gamepadControl` request is posted to `window`, and
 * `pipelineStatusUpdate` / `sidebarButtonStatusUpdate` flip the state once
 * whichever core is active has applied the change. The provider also listens
 * for `clientRoleUpdate` (viewers get no control UI, only the floating
 * touch-gamepad toggle on a touch client), `serverSettings` (the payload
 * every visibility flag derives from), `trackpadModeUpdate`,
 * `gamingModeUpdate`, and the core-owned hotkey message `toggleTouchGamepad`;
 * `toggleDashboard` (Ctrl+Shift+M) is each layout's own, since what folds
 * differs. The touch overlay is driven with `TOUCH_GAMEPAD_SETUP` and
 * `TOUCH_GAMEPAD_VISIBILITY`; the soft keys dispatch synthetic
 * `KeyboardEvent`s on `window`, which the core's input handler consumes like
 * real ones, under `setSynth`; a second display opens as a new window on the
 * `#display2-<direction>` fragment, placed with the Window Management API
 * where the browser offers it.
 * @module
 */
import * as React from "react";
import { fragmentWithSessionToken } from "../../../selkies-web-core/lib/page-url.js";
import { PALETTE_CHORDS, PALETTE_KEYS, TRACKPAD_SPEEDS, TRACKPAD_SPEED_KEY, USER_CHORDS_KEY, chordEvents,
  formatChord, parseChord, readUserChords, writeUserChords } from "../../../selkies-web-core/lib/touch-controls.js";
import { computeRenderableSettings, getLastServerSettings, getPrefixedKey, getPrintJobs, hardwareKeyboard,
  isMobileClient, isSecondaryDisplay, isViewerUrlMode, type Renderable, type ServerSettings } from "@/utils";
import { LAYOUTS, layoutById, resolveLayout, storeLayout, type LayoutSpec } from "@/layouts";

export { PALETTE_CHORDS, PALETTE_KEYS, TRACKPAD_SPEEDS };

/** The element universalTouchGamepad draws its overlay into. */
export const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";
/** Where the logo and the branding link to. */
export const PROJECT_URL = "https://github.com/selkies-project/selkies";

/** The page's attached-keyboard verdict, listening from load. */
export const keyboardWatch = hardwareKeyboard();

/** Whether a hardware keyboard is attached, which keeps the system's on-screen one down. */
export function useKeyboardAttached(): boolean {
  return React.useSyncExternalStore(keyboardWatch.subscribe, keyboardWatch.attached);
}

/** The soft modifier keys and whether each is held. */
export interface HeldKeys { Control: boolean; Alt: boolean; Meta: boolean }

/** A screen per side a second display can be placed on; `null` for a side with no screen measured. */
export type Placements = Record<"up" | "down" | "left" | "right", unknown> | null;

/** What `useDashboard` hands every layout and panel. */
export interface DashboardState {
  /** The last `serverSettings` payload, null before the stream connects. */
  serverSettings: ServerSettings | null;
  /** Every visibility flag derived from it. */
  renderable: Renderable;
  /** `ui_title`. */
  uiTitle: string;
  /** `ui_show_logo`. */
  uiShowLogo: boolean;
  /** The server demoted this page to a viewer, or its URL names one. */
  isViewer: boolean;
  /** This page is the `#display2` window. */
  isSecondaryDisplay: boolean;
  /** A mobile user agent, or a touch seen this session. */
  isTouch: boolean;
  /** Gaming mode holds the pointer and keyboard. */
  isGamingMode: boolean;
  /** The layout on screen. */
  layout: LayoutSpec;
  /** The layouts the server lets this page pick from; one hides the toggle. */
  layoutChoices: LayoutSpec[];
  /** Switches the layout and stores the pick. */
  setLayout: (id: string) => void;
  video: boolean;
  audio: boolean;
  microphone: boolean;
  webcam: boolean;
  gamepad: boolean;
  toggleVideo: () => void;
  toggleAudio: () => void;
  toggleMicrophone: () => void;
  toggleWebcam: () => void;
  toggleGamepad: () => void;
  /** The on-screen touch gamepad is shown. */
  touchGamepad: boolean;
  toggleTouchGamepad: () => void;
  trackpad: boolean;
  toggleTrackpad: () => void;
  trackpadSpeed: number;
  setTrackpadSpeed: (value: number) => void;
  /** The floating on-screen keyboard button is shown. */
  keyboardButton: boolean;
  toggleKeyboardButton: () => void;
  /** Pops the system's on-screen keyboard through the core's assist input. */
  showVirtualKeyboard: () => void;
  /** The stats overlay is on screen, kept across reloads. */
  statsOpen: boolean;
  setStatsOpen: (open: boolean) => void;
  /** Documents printed so far, which the printing panel and its entry follow. */
  printJobCount: number;
  heldKeys: HeldKeys;
  holdKey: (key: keyof HeldKeys, code: string) => void;
  pressKey: (key: string, code: string) => void;
  playChord: (chord: string) => void;
  userChords: string[];
  /** Adds the chord, false where it does not parse. */
  addChord: (text: string) => boolean;
  removeChord: (name: string) => void;
  /** The sides a second display can go, while the arrows ask; null otherwise. */
  placements: Placements;
  /** Adds a second display, placing it unasked where one adjacent screen is measured. */
  addScreen: () => Promise<void>;
  /** Opens the second display's window on `direction`, on `screen` when measured. */
  launchWindow: (direction: string, screen?: unknown) => boolean;
  dismissPlacements: () => void;
  /** Leaves fullscreen when in it, else asks the core for it. */
  toggleFullscreen: () => void;
  /** Leaves fullscreen when in it, else asks the core for gaming mode. */
  toggleGamingMode: () => void;
  /** Tells the core whether the chrome holds input focus. */
  reportChromeOpen: (open: boolean) => void;
  /** The dialog open over the stream, which every layout hosts the same way. */
  dialog: "apps" | "files" | null;
  openDialog: (name: "apps" | "files") => void;
  closeDialog: () => void;
}

const DashboardContext = React.createContext<DashboardState | null>(null);

/** The shared state; throws outside the provider. */
export function useDashboard(): DashboardState {
  const state = React.useContext(DashboardContext);
  if (!state) throw new Error("useDashboard needs a DashboardProvider");
  return state;
}

const post = (message: object) => window.postMessage(message, window.location.origin);

/**
 * Leaves fullscreen when in it, otherwise asks the core to enter it. Entering
 * is the core's, which owns what each mode locks (plain fullscreen locks
 * neither the pointer nor the keyboard, gaming mode locks both); exiting is
 * the browser's own call through whichever prefixed API exists.
 */
function leaveOrRequest(request: "requestFullscreen" | "requestGamingMode"): void {
  if (!document.fullscreenElement) {
    post({ type: request });
    return;
  }
  const doc = document as any;
  const exit = doc.exitFullscreen || doc.mozCancelFullScreen || doc.webkitExitFullscreen || doc.msExitFullscreen;
  if (exit) Promise.resolve(exit.call(document)).catch((err: unknown) => console.error("Error exiting fullscreen:", err));
}

/** Dispatches a synthetic KeyboardEvent on `window` for the core's input handler. */
function sendKeyEvent(type: string, key: string, code: string, state: Partial<HeldKeys> & { Shift?: boolean }): void {
  window.dispatchEvent(new KeyboardEvent(type, {
    key, code,
    ctrlKey: !!state.Control, altKey: !!state.Alt, metaKey: !!state.Meta, shiftKey: !!state.Shift,
    bubbles: true, cancelable: true,
  }));
}

/** Every side, with no screen to place the window on: what the arrows offer when nothing can be measured. */
const ANY_SIDE: Placements = { up: null, down: null, left: null, right: null };

/** Holds the shared state and feeds it to the layout and the panels. */
export function DashboardProvider({ children }: { children: React.ReactNode }) {
  const [serverSettings, setServerSettings] = React.useState<ServerSettings | null>(getLastServerSettings);
  const renderable = React.useMemo(() => computeRenderableSettings(serverSettings), [serverSettings]);
  const [isViewer, setIsViewer] = React.useState<boolean>(isViewerUrlMode);
  const [hasDetectedTouch, setHasDetectedTouch] = React.useState(false);
  const [isGamingMode, setIsGamingMode] = React.useState(false);
  const [video, setVideo] = React.useState(true);
  const [audio, setAudio] = React.useState(true);
  const [microphone, setMicrophone] = React.useState(false);
  const [webcam, setWebcam] = React.useState(false);
  const [gamepad, setGamepad] = React.useState(true);
  const [touchGamepad, setTouchGamepad] = React.useState(false);
  const touchGamepadSetup = React.useRef(false);
  const [trackpad, setTrackpad] = React.useState(false);
  const [trackpadSpeed, setTrackpadSpeedState] = React.useState<number>(() => {
    const stored = parseFloat(localStorage.getItem(getPrefixedKey(TRACKPAD_SPEED_KEY)) ?? "");
    return TRACKPAD_SPEEDS.includes(stored) ? stored : 1;
  });
  const [keyboardButton, setKeyboardButton] = React.useState(true);
  const keyboardAttached = useKeyboardAttached();
  const [statsOpen, setStatsOpenState] = React.useState(() => localStorage.getItem(getPrefixedKey("stats_strip")) === "true");
  const [printJobCount, setPrintJobCount] = React.useState(() => getPrintJobs().length);
  const [heldKeys, setHeldKeys] = React.useState<HeldKeys>({ Control: false, Alt: false, Meta: false });
  const [userChords, setUserChords] = React.useState<string[]>(() => readUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY)));
  const [placements, setPlacements] = React.useState<Placements>(null);
  const [layoutPick, setLayoutPick] = React.useState<string | null>(null);
  const [dialog, setDialog] = React.useState<"apps" | "files" | null>(null);

  // The layout the server allows and names: the status probe before the
  // stream connects, the settings payload after.
  const layoutSetting = serverSettings?.ui_layout;
  const layoutChoices = React.useMemo(() => {
    const allowed: string[] | undefined = layoutSetting?.allowed;
    return allowed ? LAYOUTS.filter((layout) => allowed.includes(layout.id)) : LAYOUTS;
  }, [layoutSetting]);
  const layout = React.useMemo(() => {
    const serverValue = layoutSetting?.value ?? (window as any).__SELKIES_UI_LAYOUT__ ?? null;
    const allowed: string[] | null = layoutSetting?.allowed ?? null;
    if (layoutPick && layoutChoices.some((choice) => choice.id === layoutPick)) return layoutById(layoutPick);
    return resolveLayout(serverValue, allowed);
  }, [layoutSetting, layoutPick, layoutChoices]);
  const setLayout = React.useCallback((id: string) => {
    storeLayout(id);
    setLayoutPick(id);
  }, []);
  // Named on the document for what sits outside the layout: the dialogs
  // portaled to the body take the sidebar's palette while it is the layout.
  React.useEffect(() => {
    document.documentElement.dataset.layout = layout.id;
  }, [layout]);

  /** Hides the touch overlay (once set up); a no-op while it is not showing. */
  const hideTouchGamepad = React.useCallback(() => {
    setTouchGamepad((active) => {
      if (active && touchGamepadSetup.current) {
        post({ type: "TOUCH_GAMEPAD_VISIBILITY", payload: { visible: false, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID } });
      }
      return false;
    });
  }, []);

  const toggleTouchGamepad = React.useCallback(() => {
    setTouchGamepad((active) => {
      const next = !active;
      if (next && !touchGamepadSetup.current) {
        post({ type: "TOUCH_GAMEPAD_SETUP", payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true } });
        touchGamepadSetup.current = true;
      } else if (touchGamepadSetup.current) {
        post({ type: "TOUCH_GAMEPAD_VISIBILITY", payload: { visible: next, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID } });
      }
      return next;
    });
  }, []);

  React.useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message !== "object" || message === null) return;
      switch (message.type) {
        case "serverSettings":
          setServerSettings(message.payload);
          break;
        case "clientRoleUpdate":
          setIsViewer(message.role === "viewer");
          break;
        case "pipelineStatusUpdate":
        case "sidebarButtonStatusUpdate":
          if (message.video !== undefined) setVideo(message.video);
          if (message.audio !== undefined) setAudio(message.audio);
          if (message.microphone !== undefined) setMicrophone(message.microphone);
          if (message.webcam !== undefined) setWebcam(message.webcam);
          if (message.gamepad !== undefined) {
            setGamepad(message.gamepad);
            // Gamepad input off takes the touch overlay down with it: its presses would go nowhere.
            if (message.gamepad === false) hideTouchGamepad();
          }
          break;
        case "gamepadControl":
          if (typeof message.enabled === "boolean") setGamepad(message.enabled);
          break;
        case "toggleTouchGamepad":
          toggleTouchGamepad();
          break;
        case "trackpadModeUpdate":
          if (typeof message.enabled === "boolean") setTrackpad(message.enabled);
          break;
        case "gamingModeUpdate":
          setIsGamingMode(!!message.active);
          break;
      }
    };
    const countPrintJobs = () => setPrintJobCount(getPrintJobs().length);
    window.addEventListener("message", onMessage);
    window.addEventListener("printJobsChanged", countPrintJobs);
    return () => {
      window.removeEventListener("message", onMessage);
      window.removeEventListener("printJobsChanged", countPrintJobs);
    };
  }, [hideTouchGamepad, toggleTouchGamepad]);

  // The first touch enables the touch-specific controls for the session. In
  // the capture phase: in trackpad mode the stream's own handler stops the
  // touch from bubbling, and the first touch is usually on the stream.
  React.useEffect(() => {
    if (isMobileClient) return undefined;
    const detectTouch = () => setHasDetectedTouch(true);
    window.addEventListener("touchstart", detectTouch, { passive: true, capture: true, once: true });
    return () => window.removeEventListener("touchstart", detectTouch, { capture: true });
  }, []);

  React.useEffect(() => {
    localStorage.setItem(getPrefixedKey("stats_strip"), String(statsOpen));
  }, [statsOpen]);

  const toggleGamepad = React.useCallback(() => {
    setGamepad((enabled) => {
      post({ type: "gamepadControl", enabled: !enabled });
      if (enabled) hideTouchGamepad();
      return !enabled;
    });
  }, [hideTouchGamepad]);

  const toggleTrackpad = React.useCallback(() => {
    setTrackpad((active) => {
      post({ type: active ? "touchinput:touch" : "touchinput:trackpad" });
      return !active;
    });
  }, []);

  /** The trackpad's speed is client-only; the core persists trackpad_speed itself. */
  const setTrackpadSpeed = React.useCallback((value: number) => {
    setTrackpadSpeedState(value);
    post({ type: "setTrackpadSpeed", value });
  }, []);

  const toggleKeyboardButton = React.useCallback(() => {
    // Asked for back while a keyboard was assumed: the user knows better.
    if (keyboardAttached) {
      keyboardWatch.reset();
      setKeyboardButton(true);
      return;
    }
    setKeyboardButton((visible) => !visible);
  }, [keyboardAttached]);

  /**
   * Pops the on-screen keyboard by focusing the core's keyboard-assist input,
   * releasing it again on the next touch of the interaction overlay. An
   * attached keyboard is no longer assumed, so the floating button comes back.
   */
  const showVirtualKeyboard = React.useCallback(() => {
    keyboardWatch.reset();
    const assist = document.getElementById("keyboard-input-assist") as HTMLInputElement | null;
    const overlay = document.getElementById("overlayInput");
    if (!assist) {
      console.error("Could not find #keyboard-input-assist element to focus.");
      return;
    }
    assist.removeAttribute("aria-hidden");
    assist.value = "";
    assist.focus();
    overlay?.addEventListener("touchstart", () => {
      if (document.activeElement === assist) {
        assist.blur();
        assist.setAttribute("aria-hidden", "true");
      }
    }, { once: true, passive: true });
  }, []);

  /** Toggles a held modifier soft key; the core's synthetic-key mode is raised while any is held. */
  const holdKey = React.useCallback((key: keyof HeldKeys, code: string) => {
    setHeldKeys((held) => {
      const wasHeld = held[key];
      const heldCount = Object.values(held).filter(Boolean).length;
      if (!wasHeld && heldCount === 0) post({ type: "setSynth", value: true });
      else if (wasHeld && heldCount === 1) post({ type: "setSynth", value: false });
      const next = { ...held, [key]: !wasHeld };
      sendKeyEvent(wasHeld ? "keyup" : "keydown", key, code, next);
      return next;
    });
  }, []);

  /** Presses a soft key once, with the held modifiers applied. */
  const pressKey = React.useCallback((key: string, code: string) => {
    sendKeyEvent("keydown", key, code, heldKeys);
    setTimeout(() => sendKeyEvent("keyup", key, code, heldKeys), 50);
  }, [heldKeys]);

  /**
   * Plays a palette chord (`lib/touch-controls.js`) as the soft keys do, with
   * synthetic mode on for its length unless a soft modifier already has it:
   * its modifiers and key pressed, then after 50 ms released in reverse. A
   * modifier held on a soft key stays held.
   */
  const playChord = React.useCallback((text: string) => {
    const chord = parseChord(text);
    if (!chord) return;
    const holding = Object.values(heldKeys).some(Boolean);
    if (!holding) post({ type: "setSynth", value: true });
    const events = chordEvents(chord, heldKeys);
    const firstUp = events.findIndex((e: any) => e.type === "keyup");
    events.slice(0, firstUp).forEach((e: any) => sendKeyEvent(e.type, e.key, e.code, e.state));
    setTimeout(() => {
      events.slice(firstUp).forEach((e: any) => sendKeyEvent(e.type, e.key, e.code, e.state));
      if (!holding) post({ type: "setSynth", value: false });
    }, 50);
  }, [heldKeys]);

  /** Adds the chord typed into the palette to the user's own, kept per origin. */
  const addChord = React.useCallback((text: string) => {
    const chord = parseChord(text);
    if (!chord) return false;
    const name = formatChord(chord);
    setUserChords((chords) => {
      const next = chords.includes(name) ? chords : [...chords, name];
      writeUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY), next);
      return next;
    });
    return true;
  }, []);

  const removeChord = React.useCallback((name: string) => {
    setUserChords((chords) => {
      const next = chords.filter((c) => c !== name);
      writeUserChords(localStorage, getPrefixedKey(USER_CHORDS_KEY), next);
      return next;
    });
  }, []);

  /**
   * Opens the secondary display in a new window, sized to `screen` when the
   * Window Management API supplied one. Not `noopener` in the features: that
   * makes window.open return null even when it opened, leaving a refusal
   * indistinguishable from success, so the opener is severed on the handle.
   * A refusal from an async continuation, whose click activation is spent,
   * leaves the arrows up: an arrow click is a fresh one.
   */
  const launchWindow = React.useCallback((direction: string, screen: any = null) => {
    const url = `${window.location.href.split("#")[0]}${fragmentWithSessionToken(`display2-${direction}`)}`;
    let features = "resizable=yes,scrollbars=yes";
    if (screen) {
      features += `,left=${screen.availLeft},top=${screen.availTop},width=${screen.availWidth},height=${screen.availHeight}`;
    }
    const opened = window.open(url, "_blank", features);
    if (!opened) {
      console.warn("Second display window was blocked; leaving the placement arrows up.");
      return false;
    }
    try { opened.opener = null; } catch { /* already navigated away */ }
    setPlacements(null);
    return true;
  }, []);

  /**
   * Adds a secondary display. With the Window Management API a single adjacent
   * screen is used directly; anything else offers the placement arrows. Asking
   * beats guessing: the API answers nothing without the window-management
   * permission, and a display silently opened to the right of a monitor that
   * sits above or left of this one is what the arrows exist to avoid.
   */
  const addScreen = React.useCallback(async () => {
    if (!("getScreenDetails" in window)) {
      setPlacements(ANY_SIDE);
      return;
    }
    try {
      const details = await (window as any).getScreenDetails();
      const current = details.currentScreen;
      const others = details.screens.filter((s: any) => s !== current);
      if (others.length === 0) {
        setPlacements(ANY_SIDE);
        return;
      }
      const found: Record<string, any> = {};
      for (const s of others) {
        if (!found.right && s.left >= current.left + current.width) found.right = s;
        if (!found.left && s.left + s.width <= current.left) found.left = s;
        if (!found.down && s.top >= current.top + current.height) found.down = s;
        if (!found.up && s.top + s.height <= current.top) found.up = s;
      }
      const sides = Object.keys(found);
      if (sides.length === 1) {
        if (!launchWindow(sides[0], found[sides[0]])) setPlacements(found as Placements);
      } else {
        setPlacements(sides.length > 1 ? (found as Placements) : ANY_SIDE);
      }
    } catch (err: any) {
      // A refused permission is an ordinary outcome: the arrows still ask.
      if (!(err && err.name === "NotAllowedError")) console.error("Error with Window Management API:", err);
      setPlacements(ANY_SIDE);
    }
  }, [launchWindow]);

  const state: DashboardState = {
    serverSettings,
    renderable,
    uiTitle: serverSettings?.ui_title?.value ?? "Selkies",
    uiShowLogo: serverSettings?.ui_show_logo?.value !== false,
    isViewer,
    isSecondaryDisplay,
    isTouch: isMobileClient || hasDetectedTouch,
    isGamingMode,
    layout,
    layoutChoices,
    setLayout,
    video, audio, microphone, webcam, gamepad,
    toggleVideo: () => post({ type: "pipelineControl", pipeline: "video", enabled: !video }),
    toggleAudio: () => post({ type: "pipelineControl", pipeline: "audio", enabled: !audio }),
    toggleMicrophone: () => post({ type: "pipelineControl", pipeline: "microphone", enabled: !microphone }),
    toggleWebcam: () => post({ type: "pipelineControl", pipeline: "webcam", enabled: !webcam }),
    toggleGamepad,
    touchGamepad,
    toggleTouchGamepad,
    trackpad,
    toggleTrackpad,
    trackpadSpeed,
    setTrackpadSpeed,
    keyboardButton: keyboardButton && !keyboardAttached,
    toggleKeyboardButton,
    showVirtualKeyboard,
    statsOpen,
    setStatsOpen: setStatsOpenState,
    printJobCount,
    heldKeys, holdKey, pressKey, playChord, userChords, addChord, removeChord,
    placements, addScreen, launchWindow,
    dismissPlacements: () => setPlacements(null),
    toggleFullscreen: () => leaveOrRequest("requestFullscreen"),
    toggleGamingMode: () => leaveOrRequest("requestGamingMode"),
    reportChromeOpen: (open: boolean) => post({ type: "sidebarVisibilityChanged", isOpen: open }),
    dialog,
    openDialog: setDialog,
    closeDialog: () => setDialog(null),
  };

  return <DashboardContext.Provider value={state}>{children}</DashboardContext.Provider>;
}
