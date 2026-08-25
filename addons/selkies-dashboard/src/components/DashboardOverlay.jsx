/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Portal that places the sidebar over the stream.
 * @module
 */
import ReactDOM from 'react-dom';
import Sidebar from './Sidebar';
import '../styles/Overlay.css';

/**
 * Renders the sidebar into `container` through a portal, or nothing until a
 * container exists.
 * @param {object} props
 * @param {HTMLElement|null} props.container Element the overlay is portaled into.
 */
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
