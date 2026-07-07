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

function DashboardOverlay({ container }: DashboardOverlayProps): React.ReactElement | null {
  const [isGamepadEnabled, setIsGamepadEnabled] = useState<boolean>(false);
  const [showStats, setShowStats] = useState<boolean>(true);
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

  // Add handlers for button clicks
  const handleVideoToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'video', enabled: !isVideoActive }, window.location.origin);
    setIsVideoActive(!isVideoActive);
  };

  const handleAudioToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'audio', enabled: !isAudioActive }, window.location.origin);
    setIsAudioActive(!isAudioActive);
  };

  const handleMicrophoneToggle = () => {
    window.postMessage({ type: 'pipelineControl', pipeline: 'microphone', enabled: !isMicrophoneActive }, window.location.origin);
    setIsMicrophoneActive(!isMicrophoneActive);
  };

  const handleGamepadToggle = () => {
    window.postMessage({ type: 'gamepadControl', enabled: !isGamepadEnabled }, window.location.origin);
    setIsGamepadEnabled(!isGamepadEnabled);
  };

  React.useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.ctrlKey && event.shiftKey && event.key === "G") {
        event.preventDefault();
        handleGamepadToggle();
      }

      if (event.ctrlKey && event.shiftKey && event.key === "F") {
        event.preventDefault();
        if (!document.fullscreenElement) {
          // The core fullscreens the stream container (pointer-lock aware).
          window.postMessage({ type: 'requestFullscreen' }, window.location.origin);
        }
      }

      if (event.ctrlKey && event.shiftKey && event.key === "X") {
        event.preventDefault();
        setShowStats((prev) => !prev);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handleGamepadToggle]);

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
            toggleStats={() => setShowStats(false)}
          />
        )}
        
        {/* Gamepad component (input is owned by the primary display) */}
        {isGamepadEnabled && !isSecondaryDisplay && (
          <Gamepad isGamepadEnabled={isGamepadEnabled} onGamepadToggle={setIsGamepadEnabled} />
        )}
      </div>
    </TooltipProvider>,
    container
  );
}

export default DashboardOverlay;

