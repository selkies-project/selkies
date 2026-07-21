/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import React, { useState } from 'react';
import ReactDOM from 'react-dom';
import { TopMenu } from './dashboard/top-menu';
import { Gamepad } from './dashboard/gamepad';
import { TooltipProvider } from './ui/tooltip';
import { isSecondaryDisplay, getLastServerSettings } from '../utils';
import '../styles/Overlay.css';

interface DashboardOverlayProps {
  container: Element | null;
}

const TOUCH_GAMEPAD_HOST_DIV_ID = 'touch-gamepad-host';

function DashboardOverlay({ container }: DashboardOverlayProps): React.ReactElement | null {
  const [isGamepadEnabled, setIsGamepadEnabled] = useState<boolean>(true);
  const [showStats, setShowStats] = useState<boolean>(true);
  // Touch-gamepad state lives here (not in TopMenu) so the Ctrl+Shift+G hotkey
  // works even while the menu is unmounted (hidden UI, viewers).
  const [isTouchGamepadActive, setIsTouchGamepadActive] = useState<boolean>(false);
  const [isTouchGamepadSetup, setIsTouchGamepadSetup] = useState<boolean>(false);
  const [isVideoActive, setIsVideoActive] = useState<boolean>(true);
  const [isAudioActive, setIsAudioActive] = useState<boolean>(true);
  const [isMicrophoneActive, setIsMicrophoneActive] = useState<boolean>(false);
  const [isViewer, setIsViewer] = useState<boolean>(false);
  // ui_show_sidebar hides the whole dashboard chrome — wish's analog of the
  // classic sidebar is the top menu (and everything it opens).
  const [showSidebar, setShowSidebar] = useState<boolean>(
    () => (getLastServerSettings() as any)?.ui_show_sidebar?.value !== false
  );

  // Add message event listener for status updates
  React.useEffect(() => {
    const handleWindowMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message === 'object' && message !== null) {
        if (message.type === 'pipelineStatusUpdate') {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined) setIsMicrophoneActive(message.microphone);
        } else if (message.type === 'clientRoleUpdate') {
          // Read-only viewers get no control UI.
          setIsViewer(message.role === 'viewer');
        } else if (message.type === 'sidebarButtonStatusUpdate') {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined) setIsMicrophoneActive(message.microphone);
          if (message.gamepad !== undefined) setIsGamepadEnabled(message.gamepad);
        } else if (message.type === 'serverSettings') {
          setShowSidebar(message.payload?.ui_show_sidebar?.value !== false);
        }
      }
    };

    window.addEventListener('message', handleWindowMessage);
    return () => window.removeEventListener('message', handleWindowMessage);
  }, []);

  // Pipeline toggles post the request and let the core's status echoes
  // (pipelineStatusUpdate / sidebarButtonStatusUpdate) flip the state, so the
  // menu never claims a change the active transport didn't perform (WebRTC
  // ignores video/audio pipelineControl).
  const handleVideoToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'video', enabled: !isVideoActive }, window.location.origin);
  };

  const handleAudioToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'audio', enabled: !isAudioActive }, window.location.origin);
  };

  const handleMicrophoneToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'microphone', enabled: !isMicrophoneActive }, window.location.origin);
  };

  const handleGamepadToggle = () => {
    window.postMessage({ type: 'gamepadControl', enabled: !isGamepadEnabled }, window.location.origin);
    setIsGamepadEnabled(!isGamepadEnabled);
  };

  const handleToggleTouchGamepad = React.useCallback(() => {
    const newActiveState = !isTouchGamepadActive;
    setIsTouchGamepadActive(newActiveState);
    if (newActiveState && !isTouchGamepadSetup) {
      window.postMessage(
        {
          type: 'TOUCH_GAMEPAD_SETUP',
          payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true },
        },
        window.location.origin
      );
      setIsTouchGamepadSetup(true);
    } else if (isTouchGamepadSetup) {
      window.postMessage(
        {
          type: 'TOUCH_GAMEPAD_VISIBILITY',
          payload: { visible: newActiveState, targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID },
        },
        window.location.origin
      );
    }
  }, [isTouchGamepadActive, isTouchGamepadSetup]);

  React.useEffect(() => {
    // Core-owned chords (Ctrl+Shift+M / Ctrl+Shift+G; fullscreen is handled by
    // the core itself) arrive as messages — identical wiring in both dashboards.
    const handleHotkeyMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (!message || typeof message !== "object") return;
      if (message.type === "toggleDashboard") {
        setShowStats((prev) => !prev);
      } else if (message.type === "toggleTouchGamepad") {
        handleToggleTouchGamepad();
      }
    };

    window.addEventListener("message", handleHotkeyMessage);
    return () => window.removeEventListener("message", handleHotkeyMessage);
  }, [handleToggleTouchGamepad]);

  if (!container) {
    return null;
  }

  return ReactDOM.createPortal(
    <TooltipProvider>
      <div className="h-screen w-screen">
        {/* Top Menu as primary navigation */}
        {showStats && !isViewer && showSidebar && (
          <TopMenu
            isVideoActive={isVideoActive}
            isAudioActive={isAudioActive}
            isMicrophoneActive={isMicrophoneActive}
            isGamepadEnabled={isGamepadEnabled}
            onVideoToggle={handleVideoToggle}
            onAudioToggle={handleAudioToggle}
            onMicrophoneToggle={handleMicrophoneToggle}
            onGamepadToggle={handleGamepadToggle}
            isTouchGamepadActive={isTouchGamepadActive}
            onToggleTouchGamepad={handleToggleTouchGamepad}
            toggleStats={() => setShowStats(false)}
          />
        )}

        {/* Gamepad component (input is owned by the primary display); follows
            the same chrome gates as the menu so hidden-UI and viewer sessions
            don't get a floating card over the stream. */}
        {isGamepadEnabled && !isSecondaryDisplay && showStats && !isViewer && showSidebar && (
          <Gamepad isGamepadEnabled={isGamepadEnabled} onGamepadToggle={setIsGamepadEnabled} />
        )}
      </div>
    </TooltipProvider>,
    container
  );
}

export default DashboardOverlay;

