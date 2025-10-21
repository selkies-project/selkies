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
  ChevronLeft,
  ChevronDown,
  ChevronUp
} from "lucide-react";

import { Clipboard } from "@/components/dashboard/clipboard";
import { Files } from "@/components/dashboard/files";
import { Apps } from "@/components/dashboard/apps";
import { Settings } from "@/components/dashboard/settings";
import { SystemMonitoring } from "@/components/dashboard/system-monitoring";
import { Sharing } from "@/components/dashboard/sharing";
import { ShortcutsMenu } from "@/components/dashboard/shortcuts-menu";
import { SelkiesLogo } from "@/components/logo";

// --- Constants ---
const urlHash = window.location.hash;
const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';

const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";

interface TopMenuProps {
  isVideoActive: boolean;
  isAudioActive: boolean;
  isMicrophoneActive: boolean;
  isGamepadEnabled: boolean;
  onVideoToggle: () => void;
  onAudioToggle: () => void;
  onMicrophoneToggle: () => void;
  onGamepadToggle: () => void;
  toggleStats: () => void;
}

export function TopMenu({
  isVideoActive,
  isAudioActive,
  isMicrophoneActive,
  isGamepadEnabled,
  onVideoToggle,
  onAudioToggle,
  onMicrophoneToggle,
  onGamepadToggle }: TopMenuProps) {

  const [activePanel, setActivePanel] = React.useState<string | null>(null);
  const [showAppsModal, setShowAppsModal] = React.useState(false);
  const [showDropdown, setShowDropdown] = React.useState(false);
  const [showSystemMonitoring, setShowSystemMonitoring] = React.useState(false);
  const [isDragging, setIsDragging] = React.useState(false);
  const [isSystemMonitoringDragging, setIsSystemMonitoringDragging] = React.useState(false);
  const [position, setPosition] = React.useState(() => {
    // Start with a rough center estimate, will be adjusted after mount
    const x = window.innerWidth / 2 - 200; // Reduced from 300 to better estimate actual menu width
    return { x, y: 0 };
  });
  const [systemMonitoringPosition, setSystemMonitoringPosition] = React.useState(() => {
    // Start with top-left position
    return { x: 16, y: 64 }; // 16px from left, 64px from top (below menu bar)
  });

  // --- Server Settings & UI Customization ---
  const [serverSettings, setServerSettings] = React.useState<any>(null);
  const [uiTitle, setUiTitle] = React.useState('Selkies');
  const [uiShowLogo, setUiShowLogo] = React.useState(true);

  // --- Mobile/Touch Detection ---
  const [isMobile, setIsMobile] = React.useState(false);
  const [hasDetectedTouch, setHasDetectedTouch] = React.useState(false);
  const [isTrackpadModeActive, setIsTrackpadModeActive] = React.useState(false);

  // --- Touch Gamepad ---
  const [isTouchGamepadActive, setIsTouchGamepadActive] = React.useState(false);
  const [isTouchGamepadSetup, setIsTouchGamepadSetup] = React.useState(false);

  // --- Second Screen Support ---
  const [availablePlacements, setAvailablePlacements] = React.useState<any>(null);

  // --- Keyboard Assistance ---
  const [isKeyboardButtonVisible, setIsKeyboardButtonVisible] = React.useState(true);
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

  // --- Server Settings Message Listener ---
  React.useEffect(() => {
    const handleMessage = (event: MessageEvent) => {
      if (
        event.origin === window.location.origin &&
        event.data?.type === "serverSettings"
      ) {
        console.log("Dashboard received server settings:", event.data.payload);
        setServerSettings(event.data.payload);
      }
    };
    window.addEventListener("message", handleMessage);
    return () => {
      window.removeEventListener("message", handleMessage);
    };
  }, []);

  // --- Update UI Title and Logo from Server Settings ---
  React.useEffect(() => {
    if (!serverSettings) return;

    const s_ui_title = serverSettings.ui_title;
    if (s_ui_title) {
      setUiTitle(s_ui_title.value);
    }

    const s_ui_show_logo = serverSettings.ui_show_logo;
    if (s_ui_show_logo) {
      setUiShowLogo(s_ui_show_logo.value);
    }
  }, [serverSettings]);

  // --- Mobile Detection ---
  React.useEffect(() => {
    const mobileCheck =
      typeof window !== "undefined" &&
      (((navigator as any).userAgentData && (navigator as any).userAgentData.mobile) ||
        /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(
          navigator.userAgent
        ));
    setIsMobile(!!mobileCheck);
  }, []);

  // --- Touch Detection ---
  React.useEffect(() => {
    const detectTouch = () => {
      console.log("Dashboard: First touch detected. Enabling touch-specific features.");
      setHasDetectedTouch(true);
      // Remove the listener after first touch
      window.removeEventListener('touchstart', detectTouch);
    };
    window.addEventListener('touchstart', detectTouch, { passive: true } as AddEventListenerOptions);
    return () => {
      window.removeEventListener('touchstart', detectTouch);
    };
  }, []);

  // Center the menu properly after mount
  React.useEffect(() => {
    if (dragRef.current) {
      const menuWidth = dragRef.current.offsetWidth;
      const centerX = (window.innerWidth - menuWidth) / 2;
      setPosition(prev => ({ ...prev, x: centerX }));
    }
  }, []);

  // Dragging functionality
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

      // Get the actual dimensions of the menu element
      const menuElement = dragRef.current;
      const menuWidth = menuElement ? menuElement.offsetWidth : 600; // fallback to 600
      const menuHeight = menuElement ? menuElement.offsetHeight : 100; // fallback to 100

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

  // System monitoring dragging functionality
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

      // Get the actual dimensions of the system monitoring element
      const systemMonitoringElement = systemMonitoringRef.current;
      const panelWidth = systemMonitoringElement ? systemMonitoringElement.offsetWidth : 300; // fallback to 300
      const panelHeight = systemMonitoringElement ? systemMonitoringElement.offsetHeight : 200; // fallback to 200

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

  // Click outside to close panels and dropdown
  React.useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      const target = event.target as Node;

      // Close dropdown if clicking outside dropdown and ellipsis button
      if (showDropdown) {
        const isOutsideEllipsisMenu = ellipsisRef.current && !ellipsisRef.current.contains(target);
        const isOutsideDropdown = dropdownRef.current && !dropdownRef.current.contains(target);

        if (isOutsideEllipsisMenu && isOutsideDropdown) {
          setShowDropdown(false);
        }
      }

      // Close panels if clicking outside panel and main menu (excluding system monitoring)
      if (activePanel) {
        const isOutsideMainMenu = dragRef.current && !dragRef.current.contains(target);
        const isOutsidePanel = panelRef.current && !panelRef.current.contains(target);

        // Also check if the click is on a dropdown portal (which might be rendered outside the panel)
        const isOnDropdownPortal = (target as Element).closest('[data-radix-popper-content-wrapper]') !== null;
        const isOnDropdownTrigger = (target as Element).closest('[data-radix-dropdown-menu-trigger]') !== null;
        const isOnSelectTrigger = (target as Element).closest('[data-radix-select-trigger]') !== null;
        const isOnSelectContent = (target as Element).closest('[data-radix-select-content]') !== null;

        if (isOutsideMainMenu && isOutsidePanel && !isOnDropdownPortal && !isOnDropdownTrigger && !isOnSelectTrigger && !isOnSelectContent) {
          setActivePanel(null);
        }
      }
    };

    // Only add listener if there's something to close
    if (activePanel || showDropdown) {
      document.addEventListener('mousedown', handleClickOutside);
    }

    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
    };
  }, [activePanel, showDropdown]);

  // --- Handler Functions ---
  const handlePanelToggle = (panelName: string) => {
    // Close dropdown when opening any panel or modal
    setShowDropdown(false);

    if (panelName === 'apps') {
      setShowAppsModal(true);
      return;
    }

    if (panelName === 'monitoring') {
      setShowSystemMonitoring(prev => !prev);
      return;
    }

    // Implement mutual exclusion - close other panels when opening a new one
    const newPanel = activePanel === panelName ? null : panelName;
    setActivePanel(newPanel);
  };

  const handleToggleTouchGamepad = React.useCallback(() => {
    const newActiveState = !isTouchGamepadActive;
    setIsTouchGamepadActive(newActiveState);

    if (newActiveState && !isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_SETUP",
          payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true },
        },
        window.location.origin
      );
      setIsTouchGamepadSetup(true);
      console.log(
        "Dashboard: Touch Gamepad SETUP sent, targetDivId:",
        TOUCH_GAMEPAD_HOST_DIV_ID,
        "visible: true"
      );
    } else if (isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_VISIBILITY",
          payload: {
            visible: newActiveState,
            targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID,
          },
        },
        window.location.origin
      );
      console.log(
        `Dashboard: Touch Gamepad VISIBILITY sent, targetDivId:`,
        TOUCH_GAMEPAD_HOST_DIV_ID,
        `visible: ${newActiveState}`
      );
    }
  }, [isTouchGamepadActive, isTouchGamepadSetup]);

  const handleToggleTrackpadMode = React.useCallback(() => {
    const newActiveState = !isTrackpadModeActive;
    setIsTrackpadModeActive(newActiveState);
    const message = newActiveState ? "touchinput:trackpad" : "touchinput:touch";
    console.log(`Dashboard: Toggling trackpad mode. Sending: ${message}`);
    window.postMessage({ type: message }, window.location.origin);
  }, [isTrackpadModeActive]);

  const launchWindow = (direction: string, screen: any = null) => {
    const url = `${window.location.href.split('#')[0]}#display2-${direction}`;
    let features = 'resizable=yes,scrollbars=yes,noopener,noreferrer';
    if (screen) {
      features += `,left=${screen.availLeft},top=${screen.availTop},width=${screen.availWidth},height=${screen.availHeight}`;
    }
    window.open(url, '_blank', features);
    setAvailablePlacements(null);
  };

  const handleAddScreenClick = async () => {
    if (!('getScreenDetails' in window)) {
      console.warn("Window Management API not supported. Opening default second screen.");
      launchWindow('right');
      return;
    }

    try {
      const screenDetails = await (window as any).getScreenDetails();
      const currentScreen = screenDetails.currentScreen;
      const otherScreens = screenDetails.screens.filter((s: any) => s !== currentScreen);

      if (otherScreens.length === 0) {
        console.log("No other screens detected. Opening default second screen.");
        launchWindow('right');
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
        launchWindow(direction, screen);
      } else if (availableDirections.length > 1) {
        console.log("Multiple placement options found. Showing arrows.");
        setAvailablePlacements(placements);
      } else {
        console.log("No adjacent screens found in cardinal directions. Opening default.");
        launchWindow('right');
      }
    } catch (err) {
      console.error("Error with Window Management API or permission denied:", err);
      launchWindow('right');
    }
  };

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

  const handleOnceKeyClick = (key: string, code: string) => {
    console.log(`Dashboard: Dispatching key press for ${key} with modifiers:`, heldKeys);
    sendKeyEvent('keydown', key, code, heldKeys);
    setTimeout(() => {
      sendKeyEvent('keyup', key, code, heldKeys);
    }, 50);
  };

  const toggleKeyboardButtonVisibility = () => {
    setIsKeyboardButtonVisible(prev => !prev);
  };

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
      {/* Ellipsis Control Bar */}
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

                <MenubarLabel>Stream Controls</MenubarLabel>

                <MenubarItem
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onVideoToggle();
                  }}
                >
                  <Monitor className="h-4 w-4 mr-2" />
                  <span className="flex-1">Video Stream</span>
                  <span className="text-xs text-muted-foreground ml-auto">
                    {isVideoActive ? 'On' : 'Off'}
                  </span>
                </MenubarItem>

                <MenubarItem
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onAudioToggle();
                  }}
                >
                  <Volume2 className="h-4 w-4 mr-2" />
                  <span className="flex-1">Audio Stream</span>
                  <span className="text-xs text-muted-foreground ml-auto">
                    {isAudioActive ? 'On' : 'Off'}
                  </span>
                </MenubarItem>

                <MenubarItem
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onMicrophoneToggle();
                  }}
                >
                  <Mic className="h-4 w-4 mr-2" />
                  <span className="flex-1">Microphone</span>
                  <span className="text-xs text-muted-foreground ml-auto">
                    {isMicrophoneActive ? 'On' : 'Off'}
                  </span>
                </MenubarItem>

                <MenubarItem
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onGamepadToggle();
                  }}
                >
                  <Gamepad2 className="h-4 w-4 mr-2" />
                  <span className="flex-1">Gamepad Input</span>
                  <span className="text-xs text-muted-foreground ml-auto">
                    {isGamepadEnabled ? 'Enabled' : 'Disabled'}
                  </span>
                </MenubarItem>

                <MenubarSeparator />
                <MenubarLabel>Tools & Panels</MenubarLabel>

                <MenubarSub>
                  <MenubarSubTrigger>
                    <ClipboardIcon className="h-4 w-4 mr-2" />
                    Clipboard
                  </MenubarSubTrigger>
                  <MenubarSubContent>
                    <Clipboard />
                  </MenubarSubContent>
                </MenubarSub>

                <MenubarSub>
                  <MenubarSubTrigger>
                    <FileText className="h-4 w-4 mr-2" />
                    Files
                  </MenubarSubTrigger>
                  <MenubarSubContent>
                    <Files />
                  </MenubarSubContent>
                </MenubarSub>

                <MenubarSub>
                  <MenubarSubTrigger>
                    <Share2 className="h-4 w-4 mr-2" />
                    Sharing
                  </MenubarSubTrigger>
                  <MenubarSubContent>
                    <Sharing show={true} onClose={() => { }} />
                  </MenubarSubContent>
                </MenubarSub>

                <MenubarSub>
                  <MenubarSubTrigger>
                    <Keyboard className="h-4 w-4 mr-2" />
                    Shortcuts
                  </MenubarSubTrigger>
                  <MenubarSubContent>
                    <ShortcutsMenu />
                  </MenubarSubContent>
                </MenubarSub>

                <MenubarSeparator />

                {/* Mobile/Touch Controls */}
                {(isMobile || hasDetectedTouch) && (
                  <>
                    <MenubarLabel>Touch Controls</MenubarLabel>

                    <MenubarItem onClick={handleToggleTouchGamepad}>
                      <Gamepad2 className="h-4 w-4 mr-2" />
                      <span className="flex-1">Touch Gamepad</span>
                      <span className="text-xs text-muted-foreground ml-auto">
                        {isTouchGamepadActive ? 'On' : 'Off'}
                      </span>
                    </MenubarItem>

                    <MenubarItem onClick={handleToggleTrackpadMode}>
                      <Touchpad className="h-4 w-4 mr-2" />
                      <span className="flex-1">Trackpad Mode</span>
                      <span className="text-xs text-muted-foreground ml-auto">
                        {isTrackpadModeActive ? 'On' : 'Off'}
                      </span>
                    </MenubarItem>

                    <MenubarItem onClick={handleShowVirtualKeyboard}>
                      <Keyboard className="h-4 w-4 mr-2" />
                      <span className="flex-1">Virtual Keyboard</span>
                    </MenubarItem>

                    <MenubarSeparator />
                  </>
                )}

                {/* Second Screen Support */}
                {displayId === 'primary' && (
                  <>
                    <MenubarItem onClick={handleAddScreenClick}>
                      <ScreenShare className="h-4 w-4 mr-2" />
                      <span className="flex-1">Add Second Screen</span>
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

      {/* Main Top Menu Bar */}
      <motion.div
        ref={dragRef}
        className="fixed top-0 left-0 z-50 w-fit rounded-lg border bg-background/95 backdrop-blur-sm shadow-lg opacity-30 hover:opacity-100 transition-opacity duration-300"
        style={{
          transform: `translate(${position.x}px, ${position.y}px)`,
        }}
      >
        <div className="flex items-center space-x-4 px-2 py-2">
          {/* Control Buttons */}
          <div className="flex items-center space-x-1">
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
              <TooltipContent>Apps</TooltipContent>
            </Tooltip>

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
              <TooltipContent>Settings</TooltipContent>
            </Tooltip>

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
              <TooltipContent>System Monitoring</TooltipContent>
            </Tooltip>

            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="secondary"
                  size="icon"
                  className="h-6 w-6"
                  onClick={() => {
                    if (document.fullscreenElement) {
                      document.exitFullscreen();
                    } else {
                      document.documentElement.requestFullscreen();
                    }
                  }}
                >
                  <Maximize className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Toggle Fullscreen</TooltipContent>
            </Tooltip>

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
              <TooltipContent>Drag Handle</TooltipContent>
            </Tooltip>
          </div>
        </div>
      </motion.div>



      {/* System Monitoring Panel - Draggable */}
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

      {/* Active Panel */}
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

      {/* Mobile Key Buttons */}
      {(isMobile || hasDetectedTouch) && (
        <motion.div
          className="fixed bottom-4 left-4 z-40 flex flex-wrap gap-2 p-2 rounded-lg border bg-card/95 backdrop-blur-sm shadow-lg"
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
        >
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
          {isKeyboardButtonVisible && (
            <Button
              variant="secondary"
              size="sm"
              onClick={handleShowVirtualKeyboard}
            >
              <Keyboard className="h-4 w-4" />
            </Button>
          )}
        </motion.div>
      )}

      {/* Second Screen Placement Arrows */}
      {availablePlacements && (
        <div
          className="fixed inset-0 z-50 pointer-events-auto"
          onClick={() => setAvailablePlacements(null)}
        >
          {availablePlacements.up && (
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
          {availablePlacements.down && (
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
          {availablePlacements.left && (
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
          {availablePlacements.right && (
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

      {/* Apps Modal - Separate from panels */}
      {showAppsModal && (
        <Apps isOpen={showAppsModal} onClose={() => setShowAppsModal(false)} />
      )}
    </>
  );
}