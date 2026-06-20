/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import React from 'react';
import DashboardOverlay from './components/DashboardOverlay';
import { ThemeProvider } from './components/ui/theme-provider';
import { Toaster } from 'sonner';

interface AppProps {
  dashboardRoot: Element;
}

function App({ dashboardRoot }: AppProps): React.ReactElement {
  return (
    <ThemeProvider defaultTheme="dark" storageKey="vite-ui-theme">
      <DashboardOverlay container={dashboardRoot} />
      <Toaster 
        position="bottom-right"
        richColors
        closeButton
      />
    </ThemeProvider>
  );
}

export default App; 