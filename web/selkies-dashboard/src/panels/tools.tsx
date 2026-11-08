/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The buttons a bar layout offers, described as data. A button is a
 * `ToolSpec`: its name, mark, state, class, and what a press does, which a
 * layout draws wherever it places it. `useMainTools` lists the bar's main
 * group (the settings button, a button for each panel that floats, the stats,
 * fullscreen, and the chrome knobs every bar layout shares), `useGamingTool`
 * gaming mode, `useLayoutTool` the layout toggle, and `useTouchTools` a touch
 * client's buttons and the soft keys' (`useToolboxHasItems` says whether the
 * toolbox menu, which is not a spec, has an entry); each lists only what the session allows and this page can use, in the
 * order the bar draws them. The toolbar draws them as groups
 * (`panels/toolbox.tsx`) and the edge layout fans them out one by one. The
 * controls they wrap, which exist and what they are called, are the shared
 * list in `layouts/actions.ts`.
 * @module
 */
import * as React from "react";
import { Activity, Blend, Command, Settings2 } from "lucide-react";
import { cn } from "@/lib/utils";
import { useActions, useBarPrefs, type Action, type BarState, type Translucency } from "@/layouts/actions";
import { PANELS, shownFloats, usePanelContext } from "@/panels/catalog";
import { useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";

/** One button as data: what a layout needs to draw it wherever it places it. */
export interface ToolSpec {
  /** Identifies the button among its layout's. */
  key: string;
  /** Its accessible name, and its tooltip. */
  label: string;
  /** What the tooltip says where that says more than the name. */
  tip?: string;
  /** Fills the button and marks it pressed. */
  active?: boolean;
  /** The class the suites and the core find the button by. */
  className?: string;
  icon: React.ReactNode;
  /** What a press does. */
  run: () => void;
}

/**
 * One of the shared controls (`layouts/actions.ts`) as a spec.
 * @param action The control.
 */
function actionSpec(action: Action): ToolSpec {
  return { key: action.className, label: action.label, tip: action.title, active: action.active, className: action.className, icon: <action.icon className="size-4" />, run: action.run };
}

/**
 * The buttons of the bar's main group.
 * @param bar The bar's state.
 * @returns The buttons the session allows, in the order the bar draws them.
 */
export function useMainTools(bar: BarState): ToolSpec[] {
  const ctx = usePanelContext();
  const d = useDashboard();
  const actions = useActions();
  const { barStrip, toggleBarStrip, translucency, setTranslucency } = useBarPrefs();
  // The level reads on the button's own fill: plain at solid, graded washes for light and medium, full at strong.
  const translucentGrade = translucency === 1 ? "bg-primary/35 hover:bg-primary/55" : translucency === 2 ? "bg-primary/60 hover:bg-primary/85" : translucency === 3 ? "bg-primary hover:bg-primary" : "";
  const specs: ToolSpec[] = [
    { key: "settings", label: t('topMenu.settings'), active: bar.settingsOpen, className: "settings-button", icon: <Settings2 className="size-4" />, run: bar.toggleSettings },
    ...shownFloats(ctx).map(({ id, title, icon: Icon }): ToolSpec => (
      { key: id, label: t(title), active: !!bar.floating[id], icon: Icon && <Icon className="size-4" />, run: () => bar.toggleFloating(id) })),
  ];
  if (actions.stats) specs.push(actionSpec(actions.stats));
  if (actions.fullscreen) specs.push(actionSpec(actions.fullscreen));
  if (d.layout.id === "edge") {
    specs.push({ key: "figures-strip", label: t('topMenu.figuresStrip'), active: barStrip, className: "figures-strip-button", icon: <Activity className="size-4" />, run: toggleBarStrip });
  }
  specs.push({
    key: "translucency", label: t('topMenu.translucentPanels'), tip: `${t('topMenu.translucentPanels')}: ${translucency + 1}/4`, active: translucency > 0,
    className: cn("translucency-button", translucentGrade), icon: <Blend className="size-4" />, run: () => setTranslucency(((translucency + 1) % 4) as Translucency),
  });
  return specs;
}

/**
 * The gaming-mode button, as a spec.
 * @returns The button, or null where the server turned gaming mode off.
 */
export function useGamingTool(): ToolSpec | null {
  const { gamingMode } = useActions();
  return gamingMode ? actionSpec(gamingMode) : null;
}

/**
 * Whether the toolbox menu has anything to list: a stream switch, a panel of
 * the catalog, or adding a second screen. A layout draws the menu's button
 * only then, as the toolbar always has; the footer's toggles and links are
 * not reason enough to show it.
 * @returns True where the menu has an entry.
 */
export function useToolboxHasItems(): boolean {
  const d = useDashboard();
  const ctx = usePanelContext();
  const { streams } = useActions();
  const panels = PANELS.some((panel) => panel.kind === "panel" && !panel.floats && panel.shown(ctx, "menu"));
  // The gamepad's input switch is the floating gamepads panel's, not the menu's.
  return streams.some((stream) => !stream.input) || panels || (!d.isSecondaryDisplay && d.renderable.secondScreen);
}

/**
 * The layout toggle, as a spec: it steps to the next layout the server
 * allows, as the menu's footer toggle does, and names the layout it is on and
 * the one it goes to.
 * @returns The button, or null while the server allows only one layout.
 */
export function useLayoutTool(): ToolSpec | null {
  const { layout, layoutChoices, setLayout } = useDashboard();
  if (layoutChoices.length < 2) return null;
  const index = layoutChoices.findIndex((choice) => choice.id === layout.id);
  const next = layoutChoices[(index + 1) % layoutChoices.length];
  const label = `${t('layout.title')}: ${t(layout.labelKey)}`;
  const Icon = layout.icon;
  return { key: "layout", label, tip: `${label} → ${t(next.labelKey)}`, className: "layout-fan-button", icon: <Icon className="size-4" />, run: () => setLayout(next.id) };
}

/**
 * A touch client's buttons: the touch gamepad, the trackpad, the system
 * keyboard, and the soft keys' panel.
 * @param bar The bar's state.
 * @returns The buttons, none on a client without touch.
 */
export function useTouchTools(bar: BarState): ToolSpec[] {
  const actions = useActions();
  const specs = [actions.touchGamepad, actions.trackpad, actions.keyboardButton]
    .filter((action): action is Action => action !== null)
    .map(actionSpec);
  if (actions.softKeys) {
    specs.push({ key: "soft-keys", label: t('keyPalette.title'), active: bar.softKeysOpen, className: "soft-keys-button", icon: <Command className="size-4" />, run: bar.toggleSoftKeys });
  }
  return specs;
}
