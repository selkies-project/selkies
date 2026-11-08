/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a bar layout is made of: its icon buttons, and what they open: the
 * toolbox menu, the settings card, the floating panels, and the soft keys.
 * The toolbar draws a bar with these and the edge layout fans them out around
 * its button; the layout owns where they are, `useBarState`
 * (`layouts/actions.ts`) owns which of them is open, and these draw them. The
 * controls themselves, which exist and what they are called, are the shared
 * list in `layouts/actions.ts`.
 *
 * A button is described as data, a `ToolSpec` (`panels/tools.tsx`), which
 * lists them gated as the bar gates them. `BarLogo`, `MainButtons`,
 * `GamingButton`, and `TouchButtons` draw those as the bar's groups of
 * buttons, each a row or a column and each opening its tooltips on the side
 * the layout names; a layout that places each button itself reads the specs
 * and draws each with `ToolButton`. `SoftKeysPanel` is what the touch group's
 * key button opens.
 *
 * `Tool` is one square icon button with its tooltip, filled while it is on;
 * `ToolButton` is a spec as a `Tool`. `ToolboxMenu` is the overflow menu: the stream switches, then the catalog's
 * panels (`catalog.ts`) each in a submenu or as the item that opens its
 * dialog, a second screen, and a footer with the branding, the appearance
 * toggles, and the citation. `SettingsPopup` hangs the settings card where the
 * layout puts it and closes on a click outside it or Escape. `FloatingToolPanels`
 * draws the panels that float, as the buttons beside the settings open them.
 * @module
 */
import * as React from "react";
import { ScreenShare, ToolCase, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import { Menu, MenuCheckboxItem, MenuItem, MenuPopup, MenuSeparator, MenuSub, MenuSubPopup, MenuSubTrigger, MenuTrigger } from "@/components/ui/menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { SelkiesLogo } from "@/components/logo";
import { cn } from "@/lib/utils";
import { useActions, type BarState } from "@/layouts/actions";
import { LayoutToggle, ThemeToggle, TranslucencyToggle } from "@/panels/appearance";
import { PANELS, shownFloats, usePanelContext } from "@/panels/catalog";
import { Clip } from "@/panels/controls";
import { FloatingPanel } from "@/panels/floating-panel";
import { SettingsCard } from "@/panels/settings-card";
import { CiteLink } from "@/panels/shortcuts";
import { SoftKeys } from "@/panels/soft-keys";
import { useGamingTool, useMainTools, useToolboxHasItems, useTouchTools, type ToolSpec } from "@/panels/tools";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";

/** The side of an element a popup or tooltip opens on. */
export type Side = "top" | "bottom" | "left" | "right";

interface ToolProps {
  /** The button's accessible name, and its tooltip. */
  label: string;
  /** What the tooltip says where that says more than the name. */
  tip?: string;
  /** Fills the button and marks it pressed. */
  active?: boolean;
  className?: string;
  /** The side of the button the tooltip opens on; the kit's default is above. */
  side?: Side;
  /** The tooltip cannot be hovered: the pointer passes through it to what lies beneath, for a button whose tooltip opens over others. */
  passive?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}

/**
 * A bar button with its tooltip, sized by `--bar-button` so a coarse pointer
 * gets a larger one.
 * @param props The button's label, state, and action.
 */
export function Tool({ label, tip, active, onClick, className, side, passive, children }: ToolProps) {
  return (
    <Tooltip disableHoverablePopup={passive}>
      <TooltipTrigger render={
        <Button variant={active ? "default" : "secondary"} size="icon-sm" className={cn("size-(--bar-button)", className)} aria-label={label} aria-pressed={active} onClick={onClick} />
      }>
        {children}
      </TooltipTrigger>
      <TooltipContent side={side}>{tip ?? label}</TooltipContent>
    </Tooltip>
  );
}

/**
 * A spec (`panels/tools.tsx`) as a `Tool`.
 * @param props The button, the side its tooltip opens on, and classes the layout adds.
 */
export function ToolButton({ spec, side, className, passive }: { spec: ToolSpec; side?: Side; className?: string; passive?: boolean }) {
  return (
    <Tool side={side} label={spec.label} tip={spec.tip} active={spec.active} className={cn(spec.className, className)} passive={passive} onClick={spec.run}>
      {spec.icon}
    </Tool>
  );
}


/** One stream switch of the overflow menu: its icon and name before the switch. */
function StreamSwitch({ checked, onToggle, label, children }: { checked: boolean; onToggle: () => void; label: string; children: React.ReactNode }) {
  return (
    <MenuCheckboxItem variant="switch" checked={checked} onCheckedChange={onToggle} title={label}>
      <span className="flex min-w-0 items-center gap-2">{children}<Clip>{label}</Clip></span>
    </MenuCheckboxItem>
  );
}

interface ToolboxMenuProps {
  /** The menu is controlled so its open state survives the submenu swaps; closing it folds the submenus with it. */
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The side of the button the menu opens on; the bar's inner side. */
  side?: Side;
  /** The side of the button its tooltip opens on; the kit's default is above. */
  tooltipSide?: Side;
  /** Classes the layout adds to the button, to shape it as it shapes its others. */
  className?: string;
  /** The button's tooltip cannot be hovered, as `Tool`'s `passive`. */
  passive?: boolean;
}

/**
 * The toolbox button and its overflow menu: the stream switches and the
 * catalog's panels, a second screen, and the footer. Nothing where the menu
 * would list none of the first three (`useToolboxHasItems`).
 * @param props Whether the menu is open, the sides it and its tooltip open on, and classes for the button.
 */
export function ToolboxMenu({ open, onOpenChange, side = "bottom", tooltipSide, className, passive }: ToolboxMenuProps) {
  const d = useDashboard();
  const ctx = usePanelContext();
  const actions = useActions();
  const hasItems = useToolboxHasItems();
  const r = d.renderable;
  const primary = !d.isSecondaryDisplay;
  // The gamepad's input switch is the floating gamepads panel's here.
  const streams = actions.streams.filter((stream) => !stream.input);
  const menuPanels = PANELS.filter((panel) => panel.kind === "panel" && !panel.floats && panel.shown(ctx, "menu"));

  if (!hasItems) return null;
  return (
    <Menu open={open} onOpenChange={onOpenChange}>
      <Tooltip disableHoverablePopup={passive}>
        <TooltipTrigger render={<MenuTrigger render={<Button variant="secondary" size="icon-sm" className={cn("size-(--bar-button)", className)} aria-label={t('topMenu.menu')}><ToolCase className="size-4" /></Button>} />} />
        <TooltipContent side={tooltipSide}>{t('topMenu.menu')}</TooltipContent>
      </Tooltip>
      <MenuPopup align="start" side={side} sideOffset={15}>
        {streams.length > 0 && (
          <>
            {streams.map((stream) => (
              <StreamSwitch key={stream.id} checked={stream.on(d)} onToggle={() => stream.toggle(d)} label={t(stream.label)}><stream.icon /></StreamSwitch>
            ))}
            <MenuSeparator />
          </>
        )}
        {menuPanels.map((panel) => {
          const { icon: Icon, Body, dialog } = panel;
          const title = t(panel.title);
          if (dialog) return <MenuItem key={panel.id} onClick={() => d.openDialog(dialog)} title={title}>{Icon && <Icon />}<Clip>{title}</Clip></MenuItem>;
          return (
            <MenuSub key={panel.id}>
              <MenuSubTrigger title={title}>{Icon && <Icon />}<Clip>{title}</Clip></MenuSubTrigger>
              <MenuSubPopup className={panel.menuClass}>{Body && <Body place="menu" />}</MenuSubPopup>
            </MenuSub>
          );
        })}
        {primary && r.secondScreen && (
          <>
            <MenuSeparator />
            <MenuItem onClick={d.addScreen} title={t('sections.screen.addScreenTitle')}><ScreenShare /><Clip>{t('sections.screen.addScreenTitle')}</Clip></MenuItem>
          </>
        )}
        <MenuSeparator />
        <div className="flex items-center gap-1 px-2 py-1.5">
          <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer" className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground transition-colors hover:text-primary">
            {d.uiShowLogo && <SelkiesLogo width={16} height={16} />}
            <span>{d.uiTitle}</span>
          </a>
          <span className="ms-auto flex items-center gap-1"><LayoutToggle /><TranslucencyToggle /><ThemeToggle /></span>
        </div>
        {actions.cite && <div className="px-2 pb-1.5"><CiteLink /></div>}
      </MenuPopup>
    </Menu>
  );
}

interface SettingsPopupProps {
  open: boolean;
  /** Called on a click outside the card and its bar, and on Escape. Keep it stable. */
  onClose: () => void;
  /** The layout's bar, whose own clicks do not close the card. */
  barRef: React.RefObject<HTMLElement | null>;
  /** Where the card sits; the layout decides. */
  style: React.CSSProperties;
  /** The card's entry, which slides in from the bar's side. */
  className?: string;
}

/**
 * The settings card hung where the layout puts it. An outside click closes
 * it; a Base UI popup is a body child, so a click in one still belongs to the
 * card's controls. Escape closes it as it closes the menus, in the capture
 * phase so the key does not reach the stream as well.
 * @param props Whether it is open, how it closes, and where it sits.
 */
export function SettingsPopup({ open, onClose, barRef, style, className }: SettingsPopupProps) {
  const panelRef = React.useRef<HTMLDivElement>(null);

  React.useEffect(() => {
    if (!open) return undefined;
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      const element = target instanceof Element ? target : null;
      if (barRef.current?.contains(target) || panelRef.current?.contains(target) || element?.closest("[data-base-ui-portal]")) return;
      onClose();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      onClose();
    };
    document.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open, onClose, barRef]);

  if (!open) return null;
  return (
    <div ref={panelRef} className={cn("fixed z-[2020] w-fit animate-in fade-in", className)} style={style}>
      <SettingsCard />
    </div>
  );
}

/**
 * The floating panels the layout has open, each a card dragged by its header.
 * @param props Which are open by catalog id, and how one closes.
 */
export function FloatingToolPanels({ open, onClose }: { open: Record<string, boolean>; onClose: (id: string) => void }) {
  const ctx = usePanelContext();
  return (
    <>
      {shownFloats(ctx).filter(({ id }) => open[id]).map(({ id, title, Body }) => (
        <FloatingPanel key={id} initial={{ x: 1, y: 0.06 }}>
          <div className="flex min-h-0 w-[320px] flex-col rounded-lg border bg-background text-sm shadow-lg chrome-panel">
            <div data-drag-handle className="flex cursor-grab items-center justify-between px-3 py-2 select-none active:cursor-grabbing">
              <h3 className="pointer-events-none text-sm font-semibold">{t(title)}</h3>
              <Button variant="ghost" size="icon-sm" onClick={() => onClose(id)} aria-label={t('common.close')} title={t('common.close')}><X className="size-4" /></Button>
            </div>
            <div className="min-h-0 cursor-default overflow-y-auto border-t px-3 py-3">{Body && <Body place="float" />}</div>
          </div>
        </FloatingPanel>
      ))}
    </>
  );
}

/**
 * The wordmark at the bar's start, a link to the project; nothing where the
 * server turned the logo off. It is left to the menu's footer on a phone.
 * @param props Classes the layout adds.
 */
export function BarLogo({ className }: { className?: string }) {
  const d = useDashboard();
  if (!d.uiShowLogo) return null;
  return (
    <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer" aria-label={d.uiTitle} title={d.uiTitle}
      className={cn("flex items-center transition-colors hover:text-primary max-sm:hidden", className)}>
      <SelkiesLogo width={20} height={20} />
    </a>
  );
}

/** What a group of bar buttons takes. */
interface BarGroupProps {
  /** The bar's open state (`useBarState`). */
  bar: BarState;
  /** The side of a button its tooltip opens on; the kit's default is above. */
  side?: Side;
  /** `vertical` stacks the buttons for a bar that is a column. */
  orientation?: "horizontal" | "vertical";
}

/**
 * The bar's main group, drawn as one joined control (see `useMainTools`).
 * @param props The bar's state, and the side and direction it is laid out in.
 */
export function MainButtons({ bar, side, orientation }: BarGroupProps) {
  const tools = useMainTools(bar);
  return (
    <ButtonGroup orientation={orientation}>
      {tools.map((spec) => <ToolButton key={spec.key} spec={spec} side={side} />)}
    </ButtonGroup>
  );
}

/**
 * The gaming-mode button; nothing where the server turned gaming mode off.
 * @param props The side its tooltip opens on.
 */
export function GamingButton({ side }: { side?: Side }) {
  const spec = useGamingTool();
  return spec ? <ToolButton spec={spec} side={side} /> : null;
}

/**
 * A touch client's buttons as one joined control (see `useTouchTools`).
 * Nothing on a client without touch.
 * @param props The bar's state, and the side and direction it is laid out in.
 */
export function TouchButtons({ bar, side, orientation }: BarGroupProps) {
  const tools = useTouchTools(bar);
  if (tools.length === 0) return null;
  return (
    <ButtonGroup orientation={orientation}>
      {tools.map((spec) => <ToolButton key={spec.key} spec={spec} side={side} />)}
    </ButtonGroup>
  );
}

/**
 * The soft keys' panel, at the bottom left unless the layout moves it. It is
 * a flex column, so the palette inside shrinks and scrolls rather than
 * pushing the keys off the screen, and its height leaves the bar in view.
 * While it is open it publishes how far it reaches up from the bottom as
 * `--soft-keys-top`, for the poor-connection mark (`notices.tsx`) to sit
 * above it, and removes it as the panel goes.
 * @param props Whether it is open, and the offsets that stand it clear of a bar on its corner.
 */
export function SoftKeysPanel({ open, style }: { open: boolean; style?: React.CSSProperties }) {
  const observer = React.useRef<ResizeObserver | null>(null);
  const panelRef = React.useCallback((panel: HTMLDivElement | null) => {
    const root = document.documentElement;
    observer.current?.disconnect();
    observer.current = null;
    if (!panel) {
      root.style.removeProperty('--soft-keys-top');
      return;
    }
    const publish = () => root.style.setProperty(
      '--soft-keys-top', `${parseFloat(getComputedStyle(panel).bottom) + panel.offsetHeight}px`);
    observer.current = new ResizeObserver(publish);
    observer.current.observe(panel);
    publish();
  }, []);
  if (!open) return null;
  return (
    <div ref={panelRef} className="fixed bottom-4 left-4 z-[2040] flex flex-col rounded-lg border bg-card p-2 shadow-lg animate-in fade-in slide-in-from-bottom-2 chrome-panel"
      style={{ maxWidth: "calc(100vw - 2rem)", maxHeight: "calc(100dvh - 5rem)", ...style }}>
      <SoftKeys showTrackpad={false} />
    </div>
  );
}
