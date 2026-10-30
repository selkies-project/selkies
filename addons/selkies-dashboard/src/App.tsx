/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Root of the primary-display dashboard: the theme, the shared state, the
 * stream settings, and under them the layout the page shows beside what
 * every layout shares: the dialogs, the stats over the stream (the sidebar's
 * strip, the toolbar's floating panel), the placement
 * arrows, the on-screen keyboard button, the notices, and the toaster. The
 * chrome is portaled into the dashboard root main.tsx creates over the
 * stream; a viewer, or `ui_show_sidebar` off, gets no layout at all, only
 * the floating touch-gamepad toggle on a touch client.
 * @module
 */
import * as React from "react";
import ReactDOM from "react-dom";
import { ThemeProvider } from "@/components/ui/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Apps } from "@/panels/apps";
import { FilesDialog } from "@/panels/files";
import { FloatingPanel } from "@/panels/floating-panel";
import { VirtualKeyboardButton } from "@/panels/gamepads";
import { ConnectionIndicator, Notices, Toaster } from "@/panels/notices";
import PlayerGamepadButton from "@/panels/player-gamepad-button";
import { ScreenPlacement } from "@/panels/screen-placement";
import { SettingsProvider } from "@/state/settings-context";
import { StreamStats } from "@/panels/stats";
import { StreamStrip } from "@/panels/stats-section";
import { DashboardProvider, useDashboard } from "@/state/dashboard";

/** The layout and what sits beside it, read from the shared state. */
function Chrome() {
  const d = useDashboard();
  const Layout = d.layout.component;
  if (d.isViewer) return <PlayerGamepadButton touchOnly isActive={d.touchGamepad} onToggle={d.toggleTouchGamepad} />;
  if (!d.renderable.chrome) return null;
  return (
    <>
      <React.Suspense fallback={null}><Layout /></React.Suspense>
      <ScreenPlacement />
      <VirtualKeyboardButton />
      {d.statsOpen && !d.isSecondaryDisplay && d.renderable.stats && (d.layout.id === "default"
        ? <StreamStrip />
        : (
          <FloatingPanel className="stats-overlay" initial={{ x: 0, y: 0.06 }} storageKey="stats_panel_position">
            <StreamStats variant="floating" />
          </FloatingPanel>
        ))}
      {d.dialog === "apps" && <Apps isOpen onClose={d.closeDialog} />}
      {d.dialog === "files" && <FilesDialog open onOpenChange={(open) => { if (!open) d.closeDialog(); }} />}
    </>
  );
}

interface AppProps {
  /** Element the dashboard chrome is portaled into. */
  dashboardRoot: Element;
}

/** Wraps the chrome in the providers every panel relies on. */
export default function App({ dashboardRoot }: AppProps) {
  return (
    <ThemeProvider defaultTheme="dark" storageKey="theme">
      <DashboardProvider>
        <SettingsProvider>
          <TooltipProvider>
            {ReactDOM.createPortal(<Chrome />, dashboardRoot)}
            <ConnectionIndicator />
            <Notices />
            <Toaster />
          </TooltipProvider>
        </SettingsProvider>
      </DashboardProvider>
    </ThemeProvider>
  );
}
