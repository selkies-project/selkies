/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import React from 'react';
import DashboardOverlay from './components/DashboardOverlay';
import { ThemeProvider } from './components/ui/theme-provider';
import { UploadNotifications } from './components/dashboard/upload-notifications';
import { Toaster } from 'sonner';

/**
 * Root of the primary-display dashboard: the theme provider, the overlay
 * portaled into the dashboard root, upload notifications and the toaster.
 * @module
 */

interface AppProps {
  /** Element the dashboard chrome is portaled into. */
  dashboardRoot: Element;
}

/** Wraps the dashboard overlay in the providers every card relies on. */
function App({ dashboardRoot }: AppProps): React.ReactElement {
  return (
    <ThemeProvider defaultTheme="dark" storageKey="vite-ui-theme">
      <DashboardOverlay container={dashboardRoot} />
      <UploadNotifications />
      <Toaster
        position="bottom-right"
        richColors
        closeButton
      />
    </ThemeProvider>
  );
}

export default App; 