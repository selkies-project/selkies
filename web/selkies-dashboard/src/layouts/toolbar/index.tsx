/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The wish layout: a floating toolbar along the top edge, dragged by its
 * handle and slid up behind the edge by its last button (Ctrl+Shift+M and
 * gaming mode slide it up too), leaving a tab to bring it back. What the bar
 * holds and what its buttons open (the toolbox menu, the settings card, the
 * floating panels, the touch client's buttons and the soft keys) are
 * `panels/toolbox.tsx`, shared with the edge layout, and which of them is
 * open is `useBarState` (`layouts/actions.ts`). The settings hang under the
 * bar; on a touch client the soft keys open at the bottom left, hidden until
 * their button is pressed and while the bar is slid up.
 *
 * What this layout adds is where the bar is: a fraction of the room the
 * window leaves it, dragged by the handle, and how it slides up and where the
 * settings hang, all in `model.ts`. Reads `toggleDashboard` from the core and
 * posts `sidebarVisibilityChanged` as its panels open and close.
 * @module
 */
import * as React from "react";
import { ChevronDown, ChevronUp, Hand } from "lucide-react";
import { Button } from "@/components/ui/button";
import { BarLogo, FloatingToolPanels, GamingButton, MainButtons, SettingsPopup, SoftKeysPanel, Tool, ToolboxMenu, TouchButtons } from "@/panels/toolbox";
import { t } from "@/i18n";
import { useActions, useBarState } from "../actions";
import { barStyle, popupStyle, usePlacement } from "./model";

/** The bar, its menu, and the panels hanging from it. */
export default function ToolbarLayout() {
  const bar = useBarState();
  const { softKeys } = useActions();
  const barRef = React.useRef<HTMLDivElement>(null);
  const placement = usePlacement(barRef);

  return (
    <>
      <div ref={barRef} className="toolbar fixed left-0 z-[2000] w-fit max-w-[100vw] rounded-lg border bg-background shadow-lg"
        style={barStyle(placement, bar.collapsed)}>
        {/* Tighter on a phone, where the touch buttons join the bar: 320 pixels hold it, with smaller buttons
            and the logo left to the menu's footer there. */}
        <div className="flex items-center gap-1 px-1 py-2 max-sm:[--bar-button:1.5rem] sm:gap-1.5 sm:px-2">
          <BarLogo className="pr-1" />
          <ToolboxMenu open={bar.menuOpen} onOpenChange={bar.onMenuOpenChange} />
          <MainButtons bar={bar} />
          <GamingButton />
          <TouchButtons bar={bar} />
          <Button variant="secondary" size="icon-sm" className="size-(--bar-button) cursor-grab touch-none select-none active:cursor-grabbing" aria-label={t('topMenu.dragHandle')} title={t('topMenu.dragHandle')}
            onPointerDown={placement.begin}>
            <Hand className="size-4" />
          </Button>
          <Tool label={t('topMenu.hideToolbar')} onClick={bar.hide}><ChevronUp className="size-4" /></Tool>
        </div>

        {/* What stays in view while the bar is up behind the top edge: a tab hanging from it that brings it back. */}
        {bar.collapsed && (
          <button type="button" aria-label={t('topMenu.showToolbar')} title={t('topMenu.showToolbar')} onClick={() => bar.setCollapsed(false)}
            className="absolute top-full left-1/2 flex h-6 w-16 -translate-x-1/2 items-center justify-center rounded-b-md border border-t-0 bg-background text-muted-foreground hover:text-foreground">
            <ChevronDown className="size-4" />
          </button>
        )}
      </div>

      <SettingsPopup open={bar.settingsOpen} onClose={bar.closeSettings} barRef={barRef}
        className="slide-in-from-top-2" style={popupStyle(placement)} />

      <FloatingToolPanels open={bar.floating} onClose={bar.toggleFloating} />

      <SoftKeysPanel open={softKeys && bar.softKeysOpen && !bar.collapsed} />
    </>
  );
}
