/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Root of the primary-display page: the shell the appearance settings pick,
 * loaded only when picked. Each shell is a whole dashboard with its own
 * stylesheet, portaled into the element main.jsx created for it.
 * @module
 */

import { lazy, Suspense, type ComponentType, type LazyExoticComponent } from "react";
import { useAppearance } from "./appearance";

/** Props every shell takes. */
interface ShellProps {
  /** Element the dashboard chrome is portaled into. */
  dashboardRoot: HTMLElement;
}

/** The shells by the id `ui_shell` names them by (`lib/appearance.js` SHELLS). */
const SHELLS: Record<string, LazyExoticComponent<ComponentType<ShellProps>>> = {
  classic: lazy(() => import("./classic/App.jsx")),
  wish: lazy(() => import("./wish-app")),
};

/** Renders the shell in force. */
function App({ dashboardRoot }: ShellProps) {
  const { shell } = useAppearance();
  const Shell = SHELLS[shell] || SHELLS.classic;
  return (
    <Suspense fallback={null}>
      <Shell dashboardRoot={dashboardRoot} />
    </Suspense>
  );
}

export default App;
