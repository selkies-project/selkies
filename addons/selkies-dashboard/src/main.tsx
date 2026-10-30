/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Entry point of the dashboard.
 *
 * Probes `/api/status` for the server's streaming mode, dual-mode flag, and
 * layout, publishes them as `window.__SELKIES_STREAMING_MODE__`,
 * `window.__SELKIES_DUAL_MODE__`, and `window.__SELKIES_UI_LAYOUT__`, waits
 * for the runtime-served core script to expose `window.selkiesCoreInitialize`
 * and starts it, then mounts the dashboard: the full `App` under `#root` for
 * the primary display, only the floating gamepad button for the `#player2`
 * to `#player4` hashes, and the same button once touch is detected for
 * `#shared`. The touch gamepad is bundled straight from the
 * universal-touch-gamepad addon, so a fresh checkout builds without a
 * vendored copy.
 * @module
 */
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import PlayerGamepadButton from './panels/player-gamepad-button';
import './index.css';
import { getRoutePrefix } from './utils';
import { urlFragmentKeyword } from '../../selkies-web-core/lib/page-url.js';
import "../../universal-touch-gamepad/universalTouchGamepad.js";

declare global {
  interface Window {
    __SELKIES_STREAMING_MODE__?: string;
    __SELKIES_DUAL_MODE__?: boolean;
    __SELKIES_UI_LAYOUT__?: string;
    selkiesCoreInitialize?: () => void;
  }
}

/**
 * Asks the server for the active streaming mode so the core initializes
 * into the right transport even when localStorage disagrees with the server,
 * whether it permits switching transports, so the dashboard can show the
 * WebSocket/WebRTC toggle before `serverSettings` arrive over the stream,
 * and which layout it names, so the first paint is the right one.
 */
async function detectInitialMode(): Promise<void> {
  try {
    const resp = await fetch(`${getRoutePrefix()}/api/status`, { credentials: 'same-origin', signal: AbortSignal.timeout(2000) });
    if (!resp.ok) throw new Error(`Failed to fetch initial mode, status: ${resp.status}`);
    const data = await resp.json();
    if (data?.current_mode) window.__SELKIES_STREAMING_MODE__ = data.current_mode;
    if (data && typeof data.enable_dual_mode !== 'undefined') window.__SELKIES_DUAL_MODE__ = !!data.enable_dual_mode;
    if (typeof data?.ui_layout === 'string') window.__SELKIES_UI_LAYOUT__ = data.ui_layout;
  } catch (err) {
    console.warn(`Error detecting initial mode: ${err}`);
  }
}

/**
 * Waits for the core, which arrives as a separate runtime script and is never
 * bundled here, to expose its deferred entry point.
 * @param timeoutMs Give-up deadline in milliseconds.
 * @throws When the core has not loaded by the deadline.
 */
async function waitForCore(timeoutMs = 10000): Promise<void> {
  const deadline = performance.now() + timeoutMs;
  while (typeof window.selkiesCoreInitialize !== 'function') {
    if (performance.now() > deadline) throw new Error('selkies-core.js did not load; cannot initialize streaming');
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const currentHash = urlFragmentKeyword();
const noDashboardModes = ['#shared', '#player2', '#player3', '#player4'];
const playerClientModes = ['#player2', '#player3', '#player4'];

(async () => {
  await detectInitialMode();
  try {
    await waitForCore();
    window.selkiesCoreInitialize!();
  } catch (err) {
    console.error(err);
  }

  if (!noDashboardModes.includes(currentHash)) {
    const dashboardRoot = document.createElement('div');
    dashboardRoot.id = 'dashboard-root';
    // Keystrokes on dashboard controls (slider arrows, menu navigation) drive
    // the UI, not the game: the input core skips events whose target sits
    // under an allow-native-input ancestor.
    dashboardRoot.classList.add('allow-native-input');
    document.body.appendChild(dashboardRoot);
    const mount = document.getElementById('root');
    if (!mount) {
      console.error("CRITICAL: Dashboard mount point #root not found. Primary dashboard will not render.");
      return;
    }
    ReactDOM.createRoot(mount).render(<React.StrictMode><App dashboardRoot={dashboardRoot} /></React.StrictMode>);
  } else {
    const playerRoot = document.createElement('div');
    playerRoot.id = 'player-ui-root';
    document.body.appendChild(playerRoot);
    ReactDOM.createRoot(playerRoot).render(
      <React.StrictMode><PlayerGamepadButton touchOnly={!playerClientModes.includes(currentHash)} /></React.StrictMode>,
    );
  }
})();
