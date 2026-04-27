// src/main.jsx
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.jsx';
import PlayerGamepadButton from './components/PlayerGamepadButton.jsx';
import './index.css';

// Probe the dual-mode supervisor for the currently active streaming mode
// before importing selkies-core. selkies-core reads
// window.__SELKIES_STREAMING_MODE__ at module-evaluation time and starts the
// transport immediately, so the probe must finish first. Falls back silently
// (single-mode setups have no /status endpoint).
async function detectInitialMode() {
  try {
    const r = await fetch('./status', {
      credentials: 'same-origin',
      signal: AbortSignal.timeout(3000),
    });
    if (!r.ok) return;
    const data = await r.json();
    if (data && data.current_mode) {
      window.__SELKIES_STREAMING_MODE__ = data.current_mode;
    }
  } catch {
    // /status missing, slow, or returned bad JSON - keep default
  }
}

const currentHash = window.location.hash;
const noDashboardModes = ['#shared', '#player2', '#player3', '#player4'];
const playerClientModes = ['#player2', '#player3', '#player4'];

(async () => {
  await detectInitialMode();
  await import('./selkies-core.js');
  if (!noDashboardModes.includes(currentHash)) {
    const dashboardRootElement = document.createElement('div');
    dashboardRootElement.id = 'dashboard-root';
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
