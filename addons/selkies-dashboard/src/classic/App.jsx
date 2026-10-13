/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Root of the classic shell: the sidebar over the stream.
 * @module
 */
import './index.css';
import DashboardOverlay from './DashboardOverlay';

/**
 * Renders the dashboard overlay into the element main.jsx created for it.
 * @param {object} props
 * @param {HTMLElement} props.dashboardRoot Portal target main.jsx appended to `document.body`.
 */
function App({ dashboardRoot }) {
  return (
    <>
      <DashboardOverlay container={dashboardRoot} />
    </>
  );
}

export default App;
