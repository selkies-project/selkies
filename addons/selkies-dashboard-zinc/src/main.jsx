// src/main.jsx
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.tsx';
import './index.css';

const currentHash = window.location.hash;
const noDashboardModes = ['#shared', '#player2', '#player3', '#player4'];

// Check if the current mode is one that should prevent the dashboard from loading
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
}
