/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// src/components/DashboardOverlay.jsx
import ReactDOM from 'react-dom';
import Sidebar from './Sidebar';
import '../styles/Overlay.css';

function DashboardOverlay({ container }) {

  if (!container) {
    return null;
  }

  return ReactDOM.createPortal(
    <div className="dashboard-overlay-container">
      <Sidebar />
    </div>,
    container
  );
}

export default DashboardOverlay;
