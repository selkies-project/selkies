/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The wish layout: a floating toolbar along the top edge, dragged by its
 * handle and slid up behind the edge by its last button (Ctrl+Shift+M and
 * gaming mode slide it up too), leaving a tab to bring it back. The toolbox
 * button opens the overflow menu: the stream switches, then the catalog's
 * panels (`catalog.tsx`) each in a submenu, or as the item that opens its
 * dialog, a second screen, and a footer with the branding and the appearance
 * toggles. The settings hang under the bar as a card of tabs; a panel that
 * floats has a button of its own and a panel beside the bar; on a touch
 * client the soft keys open at the bottom left from a button of the bar,
 * hidden until it is pressed and while the bar is slid up.
 *
 * Reads `toggleDashboard` from the core and posts `sidebarVisibilityChanged`
 * as its panels open and close.
 * @module
 */
import * as React from "react";
import { ChevronDown, ChevronUp, Command, Hand, ScreenShare, Settings2, ToolCase, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "cn";
import { ButtonGroup } from "@/components/ui/button-group";
import { Menu, MenuCheckboxItem, MenuItem, MenuPopup, MenuSeparator, MenuSub, MenuSubPopup, MenuSubTrigger, MenuTrigger } from "@/components/ui/menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { SelkiesLogo } from "@/components/logo";
import { LayoutToggle, ThemeToggle } from "@/panels/appearance";
import { PANELS, usePanelContext } from "@/panels/catalog";
import { Clip } from "@/panels/controls";
import { FloatingPanel } from "@/panels/floating-panel";
import { place, useAnchorDrag, useElementSize, useViewport } from "@/panels/anchor";
import { SettingsCard } from "@/panels/settings-card";
import { SoftKeys } from "@/panels/soft-keys";
import { CiteLink } from "@/panels/shortcuts";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";
import { useActions, useChromeEvents, type Action } from "./actions";

/** The toolbar's height in pixels before it is measured; sliding it up by its measured height hides it exactly. */
const TOOLBAR_HEIGHT = 50;

/** A toolbar button with its tooltip, which says `tip` where that says more than the name. */
function Tool({ label, tip, active, onClick, className, children }: { label: string; tip?: string; active?: boolean; onClick: () => void; className?: string; children: React.ReactNode }) {
  return (
    <Tooltip>
      <TooltipTrigger render={
        <Button variant={active ? "default" : "secondary"} size="icon-sm" className={cn("size-(--bar-button)", className)} aria-label={label} aria-pressed={active} onClick={onClick} />
      }>
        {children}
      </TooltipTrigger>
      <TooltipContent>{tip ?? label}</TooltipContent>
    </Tooltip>
  );
}

/** One of the shared controls (`actions.ts`) as a toolbar button. */
function ActionTool({ action }: { action: Action }) {
  return (
    <Tool label={action.label} tip={action.title} active={action.active} className={action.className} onClick={action.run}>
      <action.icon className="size-4" />
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

/** The bar, its menu, and the panels hanging from it. */
export default function ToolbarLayout() {
  const d = useDashboard();
  const ctx = usePanelContext();
  const [settingsOpen, setSettingsOpen] = React.useState(false);
  // The floating panels open, by catalog id.
  const [floating, setFloating] = React.useState<Record<string, boolean>>({});
  const toggleFloating = (id: string) => setFloating((open) => ({ ...open, [id]: !open[id] }));
  const floatingOpen = Object.values(floating).some(Boolean);
  // The overflow menu is controlled so its open state survives the submenu
  // swaps; closing it folds the submenus with it.
  const [menuOpen, setMenuOpen] = React.useState(false);
  // Slid up behind the top edge of the window, leaving a tab to bring it back.
  const [collapsed, setCollapsed] = React.useState(false);
  // The soft keys' panel, shut until its button opens it.
  const [softKeysOpen, setSoftKeysOpen] = React.useState(false);
  // The bar's place as a fraction of the room the window leaves it, centered
  // at the top until dragged: a resize, or a group joining the bar, keeps it
  // in proportion rather than letting it run off the edge.
  const [anchor, setAnchor] = React.useState({ x: 0.5, y: 0 });
  const barRef = React.useRef<HTMLDivElement>(null);
  const viewport = useViewport();
  const barSize = useElementSize(barRef, { width: 400, height: TOOLBAR_HEIGHT });
  const position = place(anchor, viewport, barSize);
  const drag = useAnchorDrag(setAnchor, viewport, barSize);
  const panelRef = React.useRef<HTMLDivElement>(null);

  const fold = React.useCallback(() => {
    setSettingsOpen(false);
    setMenuOpen(false);
  }, []);

  // The bar slides up on Ctrl+Shift+M and in gaming mode, leaving its tab; fullscreen closes the settings.
  useChromeEvents({
    open: settingsOpen || floatingOpen || d.statsOpen,
    onToggle: () => { fold(); setCollapsed((value) => !value); },
    onGamingMode: () => { fold(); setCollapsed(true); },
    onFullscreen: () => setSettingsOpen(false),
  });

  // An outside click closes the settings; a Base UI popup is a body child,
  // so a click in one still belongs to the panel's controls.
  React.useEffect(() => {
    if (!settingsOpen) return undefined;
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      const element = target instanceof Element ? target : null;
      if (barRef.current?.contains(target) || panelRef.current?.contains(target) || element?.closest("[data-base-ui-portal]")) return;
      setSettingsOpen(false);
    };
    // Escape closes the settings as it closes the menus; the capture phase
    // keeps the key from reaching the stream as well.
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      setSettingsOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [settingsOpen]);

  const r = d.renderable;
  const primary = !d.isSecondaryDisplay;
  const actions = useActions();
  // The gamepad's input switch is the floating gamepads panel's here.
  const streams = actions.streams.filter((stream) => !stream.input);
  const touch = [actions.touchGamepad, actions.trackpad, actions.keyboardButton].filter((action): action is Action => action !== null);
  const menuPanels = PANELS.filter((panel) => panel.kind === "panel" && !panel.floats && panel.shown(ctx, "menu"));
  const floats = PANELS.filter((panel) => panel.floats && panel.shown(ctx, "float"));
  const menuAvailable = streams.length > 0 || menuPanels.length > 0 || (primary && r.secondScreen);
  const softKeys = actions.softKeys;

  return (
    <>
      <div ref={barRef} className="toolbar fixed left-0 z-[2000] w-fit max-w-[100vw] rounded-lg border bg-background shadow-lg"
        style={{ transform: `translate(${position.x}px, ${position.y}px)`, top: collapsed ? -(barSize.height + position.y) : 0, transition: "top 300ms" }}>
        {/* Tighter on a phone, where the touch buttons join the bar: 320 pixels hold it, with smaller buttons
            and the logo left to the menu's footer there. */}
        <div className="flex items-center gap-1 px-1 py-2 max-sm:[--bar-button:1.5rem] sm:gap-1.5 sm:px-2">
          {d.uiShowLogo && (
            <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer" aria-label={d.uiTitle} title={d.uiTitle} className="flex items-center pr-1 transition-colors hover:text-primary max-sm:hidden">
              <SelkiesLogo width={20} height={20} />
            </a>
          )}

          {menuAvailable && (
            <Menu open={menuOpen} onOpenChange={(open) => { setMenuOpen(open); if (open) setSettingsOpen(false); }}>
              <Tooltip>
                <TooltipTrigger render={<MenuTrigger render={<Button variant="secondary" size="icon-sm" className="size-(--bar-button)" aria-label={t('topMenu.menu')}><ToolCase className="size-4" /></Button>} />} />
                <TooltipContent>{t('topMenu.menu')}</TooltipContent>
              </Tooltip>
              <MenuPopup align="start" sideOffset={15}>
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
                  <span className="ms-auto flex items-center gap-1"><LayoutToggle /><ThemeToggle /></span>
                </div>
                {actions.cite && <div className="px-2 pb-1.5"><CiteLink /></div>}
              </MenuPopup>
            </Menu>
          )}

          <ButtonGroup>
            <Tool label={t('topMenu.settings')} active={settingsOpen} className="settings-button" onClick={() => { setMenuOpen(false); setSettingsOpen((open) => !open); }}><Settings2 className="size-4" /></Tool>
            {floats.map(({ id, title, icon: Icon }) => (
              <Tool key={id} label={t(title)} active={!!floating[id]} onClick={() => toggleFloating(id)}>{Icon && <Icon className="size-4" />}</Tool>
            ))}
            {actions.stats && <ActionTool action={actions.stats} />}
            {actions.fullscreen && <ActionTool action={actions.fullscreen} />}
          </ButtonGroup>

          {actions.gamingMode && <ActionTool action={actions.gamingMode} />}

          {(touch.length > 0 || softKeys) && (
            <ButtonGroup>
              {touch.map((action) => <ActionTool key={action.className} action={action} />)}
              {softKeys && <Tool label={t('keyPalette.title')} active={softKeysOpen} className="soft-keys-button" onClick={() => setSoftKeysOpen((open) => !open)}><Command className="size-4" /></Tool>}
            </ButtonGroup>
          )}

          <Button variant="secondary" size="icon-sm" className="size-(--bar-button) cursor-grab touch-none select-none active:cursor-grabbing" aria-label={t('topMenu.dragHandle')} title={t('topMenu.dragHandle')}
            onPointerDown={(event) => drag.begin(event, position)}>
            <Hand className="size-4" />
          </Button>
          <Tool label={t('topMenu.hideToolbar')} onClick={() => { fold(); setCollapsed(true); }}><ChevronUp className="size-4" /></Tool>
        </div>

        {/* What stays in view while the bar is up behind the top edge: a tab hanging from it that brings it back. */}
        {collapsed && (
          <button type="button" aria-label={t('topMenu.showToolbar')} title={t('topMenu.showToolbar')} onClick={() => setCollapsed(false)}
            className="absolute top-full left-1/2 flex h-4 w-12 -translate-x-1/2 items-center justify-center rounded-b-md border border-t-0 bg-background text-muted-foreground hover:text-foreground">
            <ChevronDown className="size-3" />
          </button>
        )}
      </div>

      {settingsOpen && (
        <div ref={panelRef} className="fixed z-[2020] w-fit animate-in fade-in slide-in-from-top-2" style={{ left: position.x, top: position.y + barSize.height + 6 }}>
          <SettingsCard />
        </div>
      )}

      {floats.filter(({ id }) => floating[id]).map(({ id, title, Body }) => (
        <FloatingPanel key={id} initial={{ x: 1, y: 0.06 }}>
          <div className="flex min-h-0 w-[300px] flex-col rounded-lg border bg-background text-xs shadow-lg">
            <div data-drag-handle className="flex cursor-grab items-center justify-between px-3 py-2 select-none active:cursor-grabbing">
              <h3 className="pointer-events-none text-sm font-semibold">{t(title)}</h3>
              <Button variant="ghost" size="icon-xs" onClick={() => toggleFloating(id)} aria-label={t('common.close')} title={t('common.close')}><X className="size-3" /></Button>
            </div>
            <div className="min-h-0 cursor-default overflow-y-auto border-t px-3 py-3">{Body && <Body place="float" />}</div>
          </div>
        </FloatingPanel>
      ))}

      {softKeys && softKeysOpen && !collapsed && (
        // A flex column, so the palette inside shrinks and scrolls rather than pushing the keys off the screen;
        // its height leaves the bar above it in view.
        <div className="fixed bottom-4 left-4 z-[2040] flex flex-col rounded-lg border bg-card p-2 shadow-lg animate-in fade-in slide-in-from-bottom-2" style={{ maxWidth: "calc(100vw - 2rem)", maxHeight: "calc(100dvh - 5rem)" }}>
          <SoftKeys showTrackpad={false} />
        </div>
      )}
    </>
  );
}
