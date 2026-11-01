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
 * floats has a button of its own and a panel beside the bar; the soft keys
 * sit at the bottom left on a touch client.
 *
 * Reads `toggleDashboard` from the core and posts `sidebarVisibilityChanged`
 * as its panels open and close.
 * @module
 */
import * as React from "react";
import { ChevronDown, ChevronUp, Crosshair, Gamepad2, Gauge, Hand, Keyboard, Maximize, ScreenShare, Settings2, Touchpad, ToolCase, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import { Menu, MenuCheckboxItem, MenuItem, MenuPopup, MenuSeparator, MenuSub, MenuSubPopup, MenuSubTrigger, MenuTrigger } from "@/components/ui/menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { SelkiesLogo } from "@/components/logo";
import { LayoutToggle, ThemeToggle } from "@/panels/appearance";
import { PANELS, STREAMS, usePanelContext } from "@/panels/catalog";
import { Clip } from "@/panels/controls";
import { FloatingPanel } from "@/panels/floating-panel";
import { place, useAnchorDrag, useElementSize, useViewport } from "@/panels/anchor";
import { SettingsCard } from "@/panels/settings-card";
import { SoftKeys } from "@/panels/soft-keys";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { onCoreMessage } from "@/utils";
import { t } from "@/i18n";

/** Height of the toolbar in pixels, which sliding it up by hides it exactly. */
const TOOLBAR_HEIGHT = 42;

/** A toolbar button with its tooltip. */
function Tool({ label, active, onClick, className, children }: { label: string; active?: boolean; onClick: () => void; className?: string; children: React.ReactNode }) {
  return (
    <Tooltip>
      <TooltipTrigger render={
        <Button variant={active ? "default" : "secondary"} size="icon-xs" className={className} aria-label={label} aria-pressed={active} onClick={onClick} />
      }>
        {children}
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
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
  const { reportChromeOpen } = d;

  const fold = React.useCallback(() => {
    setSettingsOpen(false);
    setMenuOpen(false);
  }, []);

  React.useEffect(() => {
    const offMessages = onCoreMessage((message) => {
      // Ctrl+Shift+M, owned by the core: the bar slides up or back down.
      if (message.type === "toggleDashboard") {
        fold();
        setCollapsed((value) => !value);
      }
      // Gaming mode holds the pointer and keyboard: the bar slides up and leaves its tab.
      if (message.type === "gamingModeUpdate" && message.active) {
        fold();
        setCollapsed(true);
      }
    });
    const onFullscreen = () => { if (document.fullscreenElement) setSettingsOpen(false); };
    document.addEventListener("fullscreenchange", onFullscreen);
    return () => {
      offMessages();
      document.removeEventListener("fullscreenchange", onFullscreen);
    };
  }, [fold]);

  React.useEffect(() => {
    reportChromeOpen(settingsOpen || floatingOpen || d.statsOpen);
  }, [settingsOpen, floatingOpen, d.statsOpen, reportChromeOpen]);

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
  const streams = primary && r.coreButtons ? STREAMS.filter((stream) => !stream.input && stream.allowed(r)) : [];
  const menuPanels = PANELS.filter((panel) => panel.kind === "panel" && !panel.floats && panel.shown(ctx, "menu"));
  const floats = PANELS.filter((panel) => panel.floats && panel.shown(ctx, "float"));
  const menuAvailable = streams.length > 0 || menuPanels.length > 0 || (primary && r.secondScreen);

  return (
    <>
      <div ref={barRef} className="toolbar fixed left-0 z-[2000] w-fit max-w-[100vw] rounded-lg border bg-background shadow-lg"
        style={{ transform: `translate(${position.x}px, ${position.y}px)`, top: collapsed ? -(TOOLBAR_HEIGHT + position.y) : 0, transition: "top 300ms" }}>
        {/* Tighter on a phone, where the touch buttons join the bar: 320 pixels hold it. */}
        <div className="flex items-center gap-1 px-1 py-2 sm:gap-1.5 sm:px-2">
          {d.uiShowLogo && (
            <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer" aria-label={d.uiTitle} title={d.uiTitle} className="flex items-center pr-1 transition-colors hover:text-primary">
              <SelkiesLogo width={20} height={20} />
            </a>
          )}

          {menuAvailable && (
            <Menu open={menuOpen} onOpenChange={(open) => { setMenuOpen(open); if (open) setSettingsOpen(false); }}>
              <Tooltip>
                <TooltipTrigger render={<MenuTrigger render={<Button variant="secondary" size="icon-xs" aria-label={t('topMenu.menu')}><ToolCase className="size-4" /></Button>} />} />
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
              </MenuPopup>
            </Menu>
          )}

          <ButtonGroup>
            <Tool label={t('topMenu.settings')} active={settingsOpen} className="settings-button" onClick={() => { setMenuOpen(false); setSettingsOpen((open) => !open); }}><Settings2 className="size-4" /></Tool>
            {floats.map(({ id, title, icon: Icon }) => (
              <Tool key={id} label={t(title)} active={!!floating[id]} onClick={() => toggleFloating(id)}>{Icon && <Icon className="size-4" />}</Tool>
            ))}
            {primary && r.stats && <Tool label={t('topMenu.systemMonitoring')} active={d.statsOpen} onClick={() => d.setStatsOpen(!d.statsOpen)}><Gauge className="size-4" /></Tool>}
            {r.fullscreen && <Tool label={t('topMenu.toggleFullscreen')} className="fullscreen-button" onClick={d.toggleFullscreen}><Maximize className="size-4" /></Tool>}
          </ButtonGroup>

          {r.gamingMode && (
            <Tool label={`${t('gamingModeTitle')} — ${t('gamingModeHint')}`} className="gaming-mode-button" onClick={d.toggleGamingMode}><Crosshair className="size-4" /></Tool>
          )}

          {d.isTouch && (
            <ButtonGroup>
              {primary && <Tool label={t('topMenu.touchGamepad')} active={d.touchGamepad} onClick={d.toggleTouchGamepad}><Gamepad2 className="size-4" /></Tool>}
              {r.trackpad && <Tool label={t('trackpadModeTitle')} active={d.trackpad} className="trackpad-mode-button" onClick={d.toggleTrackpad}><Touchpad className="size-4" /></Tool>}
              {r.keyboardButton && <Tool label={t('keyboardButtonToggleTitle')} active={d.keyboardButton} className="keyboard-toggle-button" onClick={d.toggleKeyboardButton}><Keyboard className="size-4" /></Tool>}
            </ButtonGroup>
          )}

          <Button variant="secondary" size="icon-xs" className="cursor-grab touch-none select-none active:cursor-grabbing" aria-label={t('topMenu.dragHandle')} title={t('topMenu.dragHandle')}
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
        <div ref={panelRef} className="fixed z-[2020] w-fit animate-in fade-in slide-in-from-top-2" style={{ left: position.x, top: position.y + 48 }}>
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

      {d.isTouch && (r.softButtons || r.trackpad) && (
        <div className="fixed bottom-4 left-4 z-[2040] rounded-lg border bg-card p-2 shadow-lg animate-in fade-in slide-in-from-bottom-2" style={{ maxWidth: "calc(100vw - 2rem)", maxHeight: "calc(100dvh - 2rem)" }}>
          <SoftKeys />
        </div>
      )}
    </>
  );
}
