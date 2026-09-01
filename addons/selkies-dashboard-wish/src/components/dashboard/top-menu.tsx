/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The wish dashboard's menu bar and the overlays it owns: the draggable top
 * menu (apps, files, clipboard, sharing, settings, monitoring, second-screen
 * placement, keyboard assistance), the gaming control bar, the ellipsis menu,
 * the mobile soft keys, and the draggable System Monitoring panel.
 *
 * Reads the `serverSettings` and `trackpadModeUpdate` messages the core posts
 * on `window` and posts `sidebarVisibilityChanged`, `touchinput:trackpad`,
 * `touchinput:touch` and `setSynth` back; held modifier keys are delivered as
 * synthetic KeyboardEvents on `window`, which the core's input handler consumes
 * like real ones. A secondary display opens as a new window on the
 * `#display2-<direction>` fragment, placed with the Window Management API
 * where the browser offers it.
 * @module
 */

import * as React from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Button } from "@/components/ui/button";
import { ModeToggle } from "@/components/ui/ModeToggle";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Menubar,
  MenubarContent,
  MenubarItem,
  MenubarMenu,
  MenubarTrigger,
  MenubarSeparator,
  MenubarLabel,
  MenubarSub,
  MenubarSubContent,
  MenubarSubTrigger,
} from "@/components/ui/menubar";
import {
  Volume2,
  Gamepad2,
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
  LayoutPanelLeft,
  Keyboard,
  Touchpad,
  ScreenShare,
  Crosshair
} from "lucide-react";

import { Clipboard } from "@/components/dashboard/clipboard";
import { Files, FilesDialog } from "@/components/dashboard/files";
import { Apps } from "@/components/dashboard/apps";
import { Settings } from "@/components/dashboard/settings";
import { SystemMonitoring } from "@/components/dashboard/system-monitoring";
import { Sharing } from "@/components/dashboard/sharing";
import { ShortcutsMenu } from "@/components/dashboard/shortcuts-menu";
import { SelkiesLogo } from "@/components/logo";
import { computeRenderableSettings, getLastServerSettings, isMobileClient, isSecondaryDisplay } from "@/utils";
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
  /** Toggles the stats overlay. */
  toggleStats: () => void;
}

/**
 * The menu bar, its panels and the overlays around it. Server settings are
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
  onToggleTouchGamepad }: TopMenuProps) {

  const [activePanel, setActivePanel] = React.useState<string | null>(null);
  const [showAppsModal, setShowAppsModal] = React.useState(false);
  const [showFilesModal, setShowFilesModal] = React.useState(false);
  const [showDropdown, setShowDropdown] = React.useState(false);
  const [showSystemMonitoring, setShowSystemMonitoring] = React.useState(false);
  const [isDragging, setIsDragging] = React.useState(false);
  const [isSystemMonitoringDragging, setIsSystemMonitoringDragging] = React.useState(false);
  const [position, setPosition] = React.useState(() => {
    // Rough centering off an assumed 400px menu; the measured width recentres
    // it after mount.
    const x = window.innerWidth / 2 - 200;
    return { x, y: 0 };
  });
  const [systemMonitoringPosition, setSystemMonitoringPosition] = React.useState(() => {
    return { x: 16, y: 64 };
  });

  const [serverSettings, setServerSettings] = React.useState<any>(() => getLastServerSettings());
  const [renderableSettings, setRenderableSettings] = React.useState<any>(() => computeRenderableSettings(getLastServerSettings()));
  const uiTitle: string = serverSettings?.ui_title?.value ?? 'Selkies';
  const uiShowLogo: boolean = serverSettings?.ui_show_logo?.value ?? true;

  const isMobile = isMobileClient;
  const [hasDetectedTouch, setHasDetectedTouch] = React.useState(false);
  const [isTrackpadModeActive, setIsTrackpadModeActive] = React.useState(false);

  const [availablePlacements, setAvailablePlacements] = React.useState<any>(null);

  const [heldKeys, setHeldKeys] = React.useState({
    Control: false,
    Alt: false,
    Meta: false,
  });

  const dragRef = React.useRef<HTMLDivElement>(null);
  const ellipsisRef = React.useRef<HTMLDivElement>(null);
  const dropdownRef = React.useRef<HTMLDivElement>(null);
  const panelRef = React.useRef<HTMLDivElement>(null);
  const systemMonitoringRef = React.useRef<HTMLDivElement>(null);

  const startPosRef = React.useRef({ x: 0, y: 0 });
  const systemMonitoringStartPosRef = React.useRef({ x: 0, y: 0 });

  React.useEffect(() => {
    const handleMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      if (event.data?.type === "serverSettings") {
        console.log("Dashboard received server settings:", event.data.payload);
        setServerSettings(event.data.payload);
        setRenderableSettings(computeRenderableSettings(event.data.payload));
      }
      if (event.data?.type === 'trackpadModeUpdate') {
        if (typeof event.data.enabled === 'boolean') {
          setIsTrackpadModeActive(event.data.enabled);
        }
      }
    };
    window.addEventListener("message", handleMessage);
    return () => {
      window.removeEventListener("message", handleMessage);
    };
  }, []);

  // The core reacts to panels opening and closing (input focus). Monitoring
  // counts as open: the websockets core only recomputes window.fps while it
  // believes the sidebar is visible, and the overlay is not an activePanel.
  React.useEffect(() => {
    window.postMessage(
      { type: 'sidebarVisibilityChanged', isOpen: !!activePanel || showSystemMonitoring },
      window.location.origin
    );
  }, [activePanel, showSystemMonitoring]);

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
      window.removeEventListener('touchstart', detectTouch);
    };
    window.addEventListener('touchstart', detectTouch, { passive: true } as AddEventListenerOptions);
    return () => {
      window.removeEventListener('touchstart', detectTouch);
    };
  }, []);

  // Recentres the menu on its measured width.
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

  /** Starts dragging the System Monitoring panel. */
  const handleSystemMonitoringMouseDown = (e: React.MouseEvent) => {
    setIsSystemMonitoringDragging(true);
    systemMonitoringStartPosRef.current = {
      x: e.clientX - systemMonitoringPosition.x,
      y: e.clientY - systemMonitoringPosition.y,
    };
  };

  React.useEffect(() => {
    const handleSystemMonitoringMouseMove = (e: MouseEvent) => {
      if (!isSystemMonitoringDragging) return;

      const newX = e.clientX - systemMonitoringStartPosRef.current.x;
      const newY = e.clientY - systemMonitoringStartPosRef.current.y;

      const systemMonitoringElement = systemMonitoringRef.current;
      const panelWidth = systemMonitoringElement ? systemMonitoringElement.offsetWidth : 300;
      const panelHeight = systemMonitoringElement ? systemMonitoringElement.offsetHeight : 200;

      const maxX = window.innerWidth - panelWidth;
      const maxY = window.innerHeight - panelHeight;

      setSystemMonitoringPosition({
        x: Math.max(0, Math.min(newX, maxX)),
        y: Math.max(0, Math.min(newY, maxY)),
      });
    };

    const handleSystemMonitoringMouseUp = () => {
      setIsSystemMonitoringDragging(false);
    };

    if (isSystemMonitoringDragging) {
      document.addEventListener('mousemove', handleSystemMonitoringMouseMove);
      document.addEventListener('mouseup', handleSystemMonitoringMouseUp);
    }

    return () => {
      document.removeEventListener('mousemove', handleSystemMonitoringMouseMove);
      document.removeEventListener('mouseup', handleSystemMonitoringMouseUp);
    };
  }, [isSystemMonitoringDragging]);

  // An outside click closes the dropdown and the active panel; the System
  // Monitoring overlay is not a panel and stays.
  React.useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      const target = event.target as Node;

      if (showDropdown) {
        const isOutsideEllipsisMenu = ellipsisRef.current && !ellipsisRef.current.contains(target);
        const isOutsideDropdown = dropdownRef.current && !dropdownRef.current.contains(target);

        if (isOutsideEllipsisMenu && isOutsideDropdown) {
          setShowDropdown(false);
        }
      }

      if (activePanel) {
        const isOutsideMainMenu = dragRef.current && !dragRef.current.contains(target);
        const isOutsidePanel = panelRef.current && !panelRef.current.contains(target);

        // Radix portals render outside the panel element.
        const isOnDropdownPortal = (target as Element).closest('[data-radix-popper-content-wrapper]') !== null;
        const isOnDropdownTrigger = (target as Element).closest('[data-radix-dropdown-menu-trigger]') !== null;
        const isOnSelectTrigger = (target as Element).closest('[data-radix-select-trigger]') !== null;
        const isOnSelectContent = (target as Element).closest('[data-radix-select-content]') !== null;

        if (isOutsideMainMenu && isOutsidePanel && !isOnDropdownPortal && !isOnDropdownTrigger && !isOnSelectTrigger && !isOnSelectContent) {
          setActivePanel(null);
        }
      }
    };

    if (activePanel || showDropdown) {
      document.addEventListener('mousedown', handleClickOutside);
    }

    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
    };
  }, [activePanel, showDropdown]);

  /**
   * Opens or closes a panel. Apps is a modal, monitoring is an overlay that
   * closes the active panel, and the remaining panels are mutually exclusive.
   */
  const handlePanelToggle = (panelName: string) => {
    setShowDropdown(false);

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
    const url = `${window.location.href.split('#')[0]}#display2-${direction}`;
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

  /** The panel body for the active panel. */
  const renderPanel = () => {
    switch (activePanel) {
      case 'settings':
        return <Settings />;
      case 'monitoring':
        return <SystemMonitoring />;
      default:
        return null;
    }
  };



  return (
    <>
      {((renderableSettings.gamingMode ?? true) || (!isSecondaryDisplay && (renderableSettings.gamepadToggle ?? true))) && (
        <motion.div
          className="fixed top-0 left-0 z-50 w-fit rounded-lg border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300"
          style={{
            transform: `translate(${position.x - 84}px, ${position.y}px)`,
          }}
        >
          <div className="flex items-center px-2 py-2">
            <Menubar className="h-6 border-0 bg-transparent p-0">
              <MenubarMenu>
                <MenubarTrigger asChild>
                  <Button
                    variant="secondary"
                    size="icon"
                    className="h-6 w-6"
                  >
                    <Gamepad2 className="h-4 w-4" />
                  </Button>
                </MenubarTrigger>
                <MenubarContent align="start" className="min-w-[260px] max-w-[300px]">
                  <MenubarLabel>{t('topMenu.gaming')}</MenubarLabel>
                  {(renderableSettings.gamingMode ?? true) && (
                    <MenubarItem
                      className="items-start"
                      onClick={() => {
                        if (document.fullscreenElement) {
                          document.exitFullscreen().catch(err => console.error("Error exiting fullscreen:", err));
                        } else {
                          window.postMessage({ type: 'requestGamingMode' }, window.location.origin);
                        }
                      }}
                    >
                      <Crosshair className="h-4 w-4 mr-2 mt-0.5" />
                      <span className="flex-1 min-w-0">
                        <span className="block">{t('gamingModeTitle')}</span>
                        <span className="block text-xs text-muted-foreground whitespace-normal">
                          {t('gamingModeHint')}
                        </span>
                      </span>
                    </MenubarItem>
                  )}
                  {!isSecondaryDisplay && (renderableSettings.gamepadToggle ?? true) && (
                    <MenubarItem
                      onClick={(e) => {
                        e.preventDefault();
                        e.stopPropagation();
                        onGamepadToggle();
                      }}
                    >
                      <Gamepad2 className="h-4 w-4 mr-2" />
                      <span className="flex-1">{t('topMenu.gamepadInput')}</span>
                      <span className="text-xs text-muted-foreground ml-auto">
                        {isGamepadEnabled ? t('common.on') : t('common.off')}
                      </span>
                    </MenubarItem>
                  )}
                </MenubarContent>
              </MenubarMenu>
            </Menubar>
          </div>
        </motion.div>
      )}

      <motion.div
        ref={ellipsisRef}
        className="fixed top-0 left-0 z-50 w-fit rounded-lg border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300"
        style={{
          transform: `translate(${position.x - 42}px, ${position.y}px)`,
        }}
      >
        <div className="flex items-center px-2 py-2">
          <Menubar className="h-6 border-0 bg-transparent p-0">
            <MenubarMenu>
              <MenubarTrigger asChild>
                <Button
                  variant="secondary"
                  size="icon"
                  className="h-6 w-6"
                >
                  <LayoutPanelLeft className="h-4 w-4" />
                </Button>
              </MenubarTrigger>
              <MenubarContent align="start" className="min-w-[200px]">

                {!isSecondaryDisplay && (renderableSettings.coreButtons ?? true) && (
                  <>
                    <MenubarLabel>{t('topMenu.streamControls')}</MenubarLabel>

                    {(renderableSettings.videoToggle ?? true) && (
                      <MenubarItem
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          onVideoToggle();
                        }}
                      >
                        <Monitor className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.videoStream')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isVideoActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.audioToggle ?? true) && (
                      <MenubarItem
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          onAudioToggle();
                        }}
                      >
                        <Volume2 className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.audioStream')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isAudioActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.microphoneToggle ?? true) && (
                      <MenubarItem
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          onMicrophoneToggle();
                        }}
                      >
                        <Mic className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.microphone')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isMicrophoneActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.webcamToggle ?? true) && (
                      <MenubarItem
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          onWebcamToggle();
                        }}
                      >
                        <Webcam className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.webcam')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isWebcamActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.gamepadToggle ?? true) && (
                      <MenubarItem
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          onGamepadToggle();
                        }}
                      >
                        <Gamepad2 className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.gamepadInput')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isGamepadEnabled ? t('common.enabled') : t('common.disabled')}
                        </span>
                      </MenubarItem>
                    )}

                    <MenubarSeparator />
                  </>
                )}

                <MenubarLabel>{t('topMenu.toolsPanels')}</MenubarLabel>

                {(renderableSettings.clipboard ?? true) && !isSecondaryDisplay && (
                  <MenubarSub>
                    <MenubarSubTrigger>
                      <ClipboardIcon className="h-4 w-4 mr-2" />
                      {t('sections.clipboard.title')}
                    </MenubarSubTrigger>
                    <MenubarSubContent>
                      <Clipboard />
                    </MenubarSubContent>
                  </MenubarSub>
                )}

                {(renderableSettings.files ?? true) && !isSecondaryDisplay && (
                  <MenubarSub>
                    <MenubarSubTrigger>
                      <FileText className="h-4 w-4 mr-2" />
                      {t('sections.files.title')}
                    </MenubarSubTrigger>
                    <MenubarSubContent>
                      <Files onOpenDownloads={() => setShowFilesModal(true)} />
                    </MenubarSubContent>
                  </MenubarSub>
                )}

                {(renderableSettings.sharing ?? true) && !isSecondaryDisplay && (
                  <MenubarSub>
                    <MenubarSubTrigger>
                      <Share2 className="h-4 w-4 mr-2" />
                      {t('sections.sharing.title')}
                    </MenubarSubTrigger>
                    <MenubarSubContent>
                      <Sharing show={true} />
                    </MenubarSubContent>
                  </MenubarSub>
                )}

                {(renderableSettings.shortcuts ?? true) && (
                <MenubarSub>
                  <MenubarSubTrigger>
                    <Keyboard className="h-4 w-4 mr-2" />
                    {t('sections.shortcuts.title')}
                  </MenubarSubTrigger>
                  <MenubarSubContent>
                    <ShortcutsMenu />
                  </MenubarSubContent>
                </MenubarSub>
                )}

                <MenubarSeparator />

                {(isMobile || hasDetectedTouch) && (
                  <>
                    <MenubarLabel>{t('topMenu.touchControls')}</MenubarLabel>

                    {!isSecondaryDisplay && (
                      <MenubarItem onClick={onToggleTouchGamepad}>
                        <Gamepad2 className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.touchGamepad')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isTouchGamepadActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.trackpad ?? true) && (
                      <MenubarItem onClick={handleToggleTrackpadMode}>
                        <Touchpad className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('trackpadModeTitle')}</span>
                        <span className="text-xs text-muted-foreground ml-auto">
                          {isTrackpadModeActive ? t('common.on') : t('common.off')}
                        </span>
                      </MenubarItem>
                    )}

                    {(renderableSettings.keyboardButton ?? true) && (
                      <MenubarItem onClick={handleShowVirtualKeyboard}>
                        <Keyboard className="h-4 w-4 mr-2" />
                        <span className="flex-1">{t('topMenu.virtualKeyboard')}</span>
                      </MenubarItem>
                    )}

                    <MenubarSeparator />
                  </>
                )}

                {/* second_screen is effective availability (admin flag AND backend
                    capacity) and the server rejects secondaries it cannot back, so the
                    entry follows it rather than offering a window that would be killed. */}
                {!isSecondaryDisplay && serverSettings?.second_screen?.value && (
                  <>
                    <MenubarItem
                      onClick={handleAddScreenClick}
                      title={t('sections.screen.addScreenTitle')}
                    >
                      <ScreenShare className="h-4 w-4 mr-2" />
                      <span className="flex-1">{t('sections.screen.addScreenTitle')}</span>
                    </MenubarItem>
                    <MenubarSeparator />
                  </>
                )}

                <div className="flex items-center justify-between w-full px-2 py-1">
                  <a
                    href="https://github.com/selkies-project/selkies"
                    target="_blank"
                    rel="noopener noreferrer"
                    className="flex items-center gap-2 hover:text-primary transition-colors"
                  >
                    {uiShowLogo && <SelkiesLogo width={20} height={20} />}
                    <span className="text-sm font-medium">
                      {uiTitle}
                    </span>
                  </a>
                  <ModeToggle />
                </div>
              </MenubarContent>
            </MenubarMenu>
          </Menubar>
        </div>
      </motion.div>

      <motion.div
        ref={dragRef}
        className="fixed top-0 left-0 z-50 w-fit rounded-lg border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300"
        style={{
          transform: `translate(${position.x}px, ${position.y}px)`,
        }}
      >
        <div className="flex items-center space-x-4 px-2 py-2">
          <div className="flex items-center space-x-1">
            {(renderableSettings.apps ?? true) && !isSecondaryDisplay && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="secondary"
                    size="icon"
                    className="h-6 w-6"
                    onClick={() => handlePanelToggle('apps')}
                  >
                    <LayoutGrid className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{t('sections.apps.title')}</TooltipContent>
              </Tooltip>
            )}

            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant={activePanel === 'settings' ? "default" : "secondary"}
                  size="icon"
                  className="h-6 w-6"
                  onClick={() => handlePanelToggle('settings')}
                >
                  <Settings2 className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>{t('topMenu.settings')}</TooltipContent>
            </Tooltip>

            {(renderableSettings.stats ?? true) && !isSecondaryDisplay && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant={showSystemMonitoring ? "default" : "secondary"}
                    size="icon"
                    className="h-6 w-6"
                    onClick={() => handlePanelToggle('monitoring')}
                  >
                    <Gauge className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{t('topMenu.systemMonitoring')}</TooltipContent>
              </Tooltip>
            )}

            {(renderableSettings.fullscreen ?? true) && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="secondary"
                    size="icon"
                    className="h-6 w-6"
                    onClick={() => {
                      if (document.fullscreenElement) {
                        document.exitFullscreen().catch(err => console.error("Error exiting fullscreen:", err));
                      } else {
                        // Plain fullscreen: the core locks neither the pointer nor
                        // the keyboard, so the bar stays usable.
                        window.postMessage({ type: 'requestFullscreen' }, window.location.origin);
                      }
                    }}
                  >
                    <Maximize className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{t('topMenu.toggleFullscreen')}</TooltipContent>
              </Tooltip>
            )}

            {(renderableSettings.gamingMode ?? true) && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="secondary"
                    size="icon"
                    className="h-6 w-6"
                    onClick={() => {
                      if (document.fullscreenElement) {
                        document.exitFullscreen().catch(err => console.error("Error exiting fullscreen:", err));
                      } else {
                        window.postMessage({ type: 'requestGamingMode' }, window.location.origin);
                      }
                    }}
                  >
                    <Crosshair className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{t('gamingModeTitle')}</TooltipContent>
              </Tooltip>
            )}

            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="secondary"
                  size="icon"
                  className="h-6 w-6 cursor-grab active:cursor-grabbing select-none"
                  onMouseDown={handleMouseDown}
                >
                  <Hand className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>{t('topMenu.dragHandle')}</TooltipContent>
            </Tooltip>
          </div>
        </div>
      </motion.div>



      <AnimatePresence>
        {showSystemMonitoring && (
          <motion.div
            ref={systemMonitoringRef}
            initial={{ opacity: 0, scale: 0.95 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.95 }}
            style={{
              position: 'fixed',
              left: systemMonitoringPosition.x,
              top: systemMonitoringPosition.y,
              zIndex: 30,
              cursor: isSystemMonitoringDragging ? 'grabbing' : 'grab'
            }}
            onMouseDown={handleSystemMonitoringMouseDown}
          >
            <SystemMonitoring />
          </motion.div>
        )}
      </AnimatePresence>

      <AnimatePresence>
        {activePanel && (
          <motion.div
            ref={panelRef}
            initial={{ opacity: 0, y: -20 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -20 }}
            className="absolute z-20 w-fit"
            style={{
              left: position.x,
              top: position.y + 48,
            }}
          >
            {renderPanel()}
          </motion.div>
        )}
      </AnimatePresence>

      {(isMobile || hasDetectedTouch) &&
        ((renderableSettings.softButtons ?? true) || (renderableSettings.trackpad ?? true)) && (
        <motion.div
          className="fixed bottom-4 left-4 z-40 flex flex-wrap gap-2 p-2 rounded-lg border bg-card/95 backdrop-blur-sm shadow-lg"
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
        >
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

      {/* Files dialog, beside the menubar like the Apps modal: a click in its
          iframe blurs the window, which closes every menu, so it cannot live
          inside the Files submenu. */}
      {showFilesModal && (
        <FilesDialog open={showFilesModal} onOpenChange={setShowFilesModal} />
      )}
    </>
  );
}