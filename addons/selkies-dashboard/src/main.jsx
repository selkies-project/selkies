/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// src/main.jsx
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.jsx';
import PlayerGamepadButton from './components/PlayerGamepadButton.jsx';
import './index.css';
import { getRoutePrefix } from './utils.js';
// Bundled straight from the addon it lives in, so a fresh checkout builds
// without a vendored copy in src/.
import "../../universal-touch-gamepad/universalTouchGamepad.js";

// Probe the server for the currently active streaming mode
// before importing selkies-core.
async function detectInitialMode() {
  try {
    const resp = await fetch(`${getRoutePrefix()}/api/status`, {
      credentials: 'same-origin',
      signal: AbortSignal.timeout(2000),
    });
    if (!resp.ok) 
      throw new Error(`Failed to fetch initial mode, status: ${resp.status}`);
    const data = await resp.json();
    if (data && data.current_mode) {
      console.log(`Received initial streaming mode: ${data.current_mode}`);
      window.__SELKIES_STREAMING_MODE__ = data.current_mode;
    }
    // Expose whether the server permits switching transports, so the dashboard
    // can show the WebSocket/WebRTC toggle even before serverSettings arrive
    // over the stream — otherwise a WebRTC session that never connects would
    // leave the user with no visible way back to WebSockets.
    if (data && typeof data.enable_dual_mode !== 'undefined') {
      window.__SELKIES_DUAL_MODE__ = !!data.enable_dual_mode;
    }
  } catch (err) {
    console.warn(`Error detecting initial mode: ${err}`);
  }
}

const currentHash = window.location.hash;
const noDashboardModes = ['#shared', '#player2', '#player3', '#player4'];
const playerClientModes = ['#player2', '#player3', '#player4'];

(async () => {
  await detectInitialMode();
  // Prevent selkies-core from auto-initializing
  window.__SELKIES_DEFER_INITIALIZATION = true;
  await import('./selkies-core.js');
  // Initialize with the mode detected from server
  window.selkiesCoreInitialize();
  if (!noDashboardModes.includes(currentHash)) {
    const dashboardRootElement = document.createElement('div');
    dashboardRootElement.id = 'dashboard-root';
    // Keystrokes on dashboard controls (slider arrows, dropdown nav) drive the
    // UI, not the game: the input core skips events whose target sits under an
    // allow-native-input ancestor.
    dashboardRootElement.classList.add('allow-native-input');
    document.body.appendChild(dashboardRootElement);
    const appMountPoint = document.getElementById('root');
    if (appMountPoint) {
      ReactDOM.createRoot(appMountPoint).render(
        <React.StrictMode>
          <App dashboardRoot={dashboardRootElement} />
        </React.StrictMode>,
      );
    } else {
      console.error("CRITICAL: Dashboard mount point #root not found. Primary dashboard will not render.");
    }
  } else {
    console.log(`Dashboard UI rendering skipped for mode: ${currentHash}`);
    if (playerClientModes.includes(currentHash)) {
      console.log(`Player client mode detected. Initializing gamepad button UI for ${currentHash}.`);
      const playerUIRootElement = document.createElement('div');
      playerUIRootElement.id = 'player-ui-root';
      document.body.appendChild(playerUIRootElement);
      ReactDOM.createRoot(playerUIRootElement).render(
        <React.StrictMode>
          <PlayerGamepadButton />
        </React.StrictMode>,
      );
    }
  }
})();
