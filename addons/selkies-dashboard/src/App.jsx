/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// src/App.jsx
import DashboardOverlay from './components/DashboardOverlay';

// App receives the dashboardRoot element created in main.jsx
function App({ dashboardRoot }) {
  return (
    <>
      <DashboardOverlay container={dashboardRoot} />
    </>
  );
}

export default App;
