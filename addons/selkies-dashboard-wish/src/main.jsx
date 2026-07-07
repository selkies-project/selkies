/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// src/main.jsx
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.tsx';
import PlayerGamepadButton from './components/dashboard/PlayerGamepadButton.tsx';
import './index.css';
import { getRoutePrefix } from './utils.ts';

// Probe the server for the active streaming mode so the runtime-served core
// (deferred via the inline flag in index.html) initializes into the right
// transport even when localStorage disagrees with the server.
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
      window.__SELKIES_STREAMING_MODE__ = data.current_mode;
    }
  } catch (err) {
    console.warn(`Error detecting initial mode: ${err}`);
  }
}

// The core arrives as a separate runtime script (never bundled here); wait
// for its loader to expose the deferred entry point.
async function waitForCore(timeoutMs = 10000) {
  const deadline = performance.now() + timeoutMs;
  while (typeof window.selkiesCoreInitialize !== 'function') {
    if (performance.now() > deadline) {
      throw new Error('selkies-core.js did not load; cannot initialize streaming');
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const currentHash = window.location.hash;
const noDashboardModes = ['#shared', '#player2', '#player3', '#player4'];
const playerClientModes = ['#player2', '#player3', '#player4'];

(async () => {
  await detectInitialMode();
  try {
    await waitForCore();
    window.selkiesCoreInitialize();
  } catch (err) {
    console.error(err);
  }

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
