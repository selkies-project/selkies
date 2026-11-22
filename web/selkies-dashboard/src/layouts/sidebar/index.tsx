/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The default layout: the classic drawer on the left edge, opened by the
 * handle there or by Ctrl+Shift+M. Its header carries the wordmark and one
 * menu button, so a long title keeps the row: the menu holds what changes how
 * the session is shown and taken (fullscreen, gaming mode, and a touch
 * client's keyboard button and trackpad switches), and under them the
 * appearance controls (the layout toggle, the translucency knob, the theme
 * pill) in the row the toolbar's menu ends with; a row of tiles under the
 * header toggles the streams; a
 * touch client gets the soft keys under the tiles; every panel is a section
 * below, each folding on its own, but one that is only a dialog (the apps),
 * whose row opens it, and the footer carries the citation. The sections hold the
 * same panels the toolbar carries, drawn in this layout's own skin
 * (`sidebar.css`), and the stats section's switch lays the strip over the
 * stream. Entering fullscreen or gaming mode folds the drawer, and the
 * handle goes with gaming mode, so the session has the pointer.
 *
 * Reads `toggleDashboard` (Ctrl+Shift+M) from the core and posts
 * `sidebarVisibilityChanged` as the drawer opens and closes; the panels
 * speak to the core themselves.
 * @module
 */
import * as React from "react";
import { ArrowUpRight, ChevronDown, ChevronUp, Menu as MenuIcon } from "lucide-react";
import { SelkiesLogo } from "@/components/logo";
import { Menu, MenuCheckboxItem, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from "@/components/ui/menu";
import { LayoutToggle, ThemePill, TranslucencyToggle } from "@/panels/appearance";
import { fractionOf, place, useViewport } from "@/panels/anchor";
import { PANELS, shownCards, usePanelContext, type PanelSpec } from "@/panels/catalog";
import { CardList } from "@/panels/catalog-bodies";
import { SwitchRow } from "@/panels/controls";
import { SoftKeys } from "@/panels/soft-keys";
import { CiteLink } from "@/panels/shortcuts";
import { StatsSection } from "@/panels/stats";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";
import { useActions, useChromeEvents, type Action } from "../actions";
import "./sidebar.css";

/** Height of the handle in pixels, which its drag keeps inside the window. */
const HANDLE_HEIGHT = 60;

/** One tile of the stream row: a stream's toggle. */
function Tile({ active, label, onClick, children }: { active: boolean; label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button type="button" className="action-button" aria-pressed={active} aria-label={label} title={label} onClick={onClick}>
      {children}
    </button>
  );
}

/**
 * The header's menu: the actions that take the session full screen, a touch
 * client's input switches, and the appearance controls. Each item keeps its
 * action's class, which the suites find it by; the popup is portaled, so it
 * carries the drawer's skin itself.
 */
function HeaderMenu({ actions, toggles }: { actions: Action[]; toggles: Action[] }) {
  const label = t("topMenu.menu");
  return (
    <Menu>
      <MenuTrigger className="header-action-button sidebar-menu-button" aria-label={label} title={label}>
        <MenuIcon />
      </MenuTrigger>
      <MenuPopup align="end" className="sidebar-skin sidebar-menu-popup">
        {actions.map((action) => (
          <MenuItem key={action.className} className={action.className} title={action.title} onClick={action.run}>
            <action.icon />{action.label}
          </MenuItem>
        ))}
        {toggles.length > 0 && <MenuSeparator />}
        {toggles.map((action) => (
          <MenuCheckboxItem key={action.className} variant="switch" className={action.className} title={action.title}
            checked={!!action.active} onCheckedChange={() => action.run()} closeOnClick>
            <span className="flex min-w-0 items-center gap-2"><action.icon className="size-4" />{action.label}</span>
          </MenuCheckboxItem>
        ))}
        {(actions.length > 0 || toggles.length > 0) && <MenuSeparator />}
        <div className="sidebar-menu-appearance">
          <LayoutToggle />
          <TranslucencyToggle />
          <ThemePill />
        </div>
      </MenuPopup>
    </Menu>
  );
}

interface SectionProps {
  /** The content element's id, which the suites address. */
  id: string;
  title: string;
  open: boolean;
  onToggle: () => void;
  children: React.ReactNode;
}

/** One folding section of the drawer: a header button and, while open, its body. */
function Section({ id, title, open, onToggle, children }: SectionProps) {
  return (
    <div className="sidebar-section">
      <button type="button" className="sidebar-section-header" aria-expanded={open} aria-controls={id} title={title} onClick={onToggle}>
        <h3>{title}</h3>
        {open ? <ChevronUp /> : <ChevronDown />}
      </button>
      {open && <div className="sidebar-section-content" id={id}>{children}</div>}
    </div>
  );
}

/** A panel that is only a dialog: a row like a section's header that opens it, its arrow saying it leads away. */
function DialogRow({ title, hint, onOpen }: { title: string; hint: string; onOpen: () => void }) {
  return (
    <div className="sidebar-section">
      <button type="button" className="sidebar-section-link" aria-haspopup="dialog" title={hint} onClick={onOpen}>
        <h3>{title}</h3>
        <ArrowUpRight />
      </button>
    </div>
  );
}

/** The drawer, its handle, and the sections. */
export default function SidebarLayout() {
  const d = useDashboard();
  const ctx = usePanelContext();
  const [open, setOpen] = React.useState(false);
  const [sections, setSections] = React.useState<Record<string, boolean>>({});
  // The handle's place down the left edge, kept as a fraction of the window's
  // height so a resize keeps it in proportion and on screen.
  const [handleAt, setHandleAt] = React.useState(0.5);
  const viewport = useViewport();
  const handleSize = React.useMemo(() => ({ width: 0, height: HANDLE_HEIGHT }), []);
  const handleTop = place({ x: 0, y: handleAt }, viewport, handleSize).y;
  const drag = React.useRef<{ startY: number; startTop: number; moved: boolean } | null>(null);
  // Hidden for a viewer and in gaming mode; entering fullscreen folds the drawer.
  const shown = open && !d.isViewer && !d.isGamingMode;
  useChromeEvents({ open: shown, onToggle: () => setOpen((value) => !value), onFullscreen: () => setOpen(false) });

  const onHandleDown = (event: React.PointerEvent<HTMLDivElement>) => {
    drag.current = { startY: event.clientY, startTop: handleTop, moved: false };
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const onHandleMove = (event: React.PointerEvent) => {
    const state = drag.current;
    if (!state) return;
    const delta = event.clientY - state.startY;
    if (Math.abs(delta) > 6) state.moved = true;
    if (state.moved) setHandleAt(fractionOf({ x: 0, y: state.startTop + delta }, viewport, handleSize).y);
  };
  const onHandleUp = () => {
    const state = drag.current;
    drag.current = null;
    if (state && !state.moved) setOpen((value) => !value);
  };

  const toggleSection = (name: string) => setSections((state) => ({ ...state, [name]: !state[name] }));
  // What each panel of the catalog draws in its section: a settings panel its
  // cards one after the other, the stats the strip's switch over the section's
  // own view, any other its body.
  const sectionBody = (panel: PanelSpec) => {
    if (panel.kind === "settings") return <CardList cards={shownCards(panel, ctx)} />;
    if (panel.kind === "stats") {
      return (
        <>
          <SwitchRow id="statsStripToggle" label={t("sections.stats.stripLabel")} checked={d.statsOpen} onToggle={() => d.setStatsOpen(!d.statsOpen)} />
          <StatsSection active={shown && !!sections.stats} />
        </>
      );
    }
    const Body = panel.Body;
    return Body ? <Body place="section" /> : null;
  };

  const actions = useActions();
  // The touch gamepad's switch is in the gamepads section.
  const present = (list: (Action | null)[]) => list.filter((action): action is Action => action !== null);
  const modes = present([actions.fullscreen, actions.gamingMode]);
  const touchModes = present([actions.keyboardButton, actions.trackpad]);
  const handleLabel = t(shown ? "topMenu.hideToolbar" : "topMenu.showToolbar");

  return (
    <>
      {!d.isViewer && !d.isGamingMode && (
        <div className="toggle-handle sidebar-skin fixed z-[2010]" style={{ top: handleTop }} role="button" aria-label={handleLabel} title={handleLabel}
          onPointerDown={onHandleDown} onPointerMove={onHandleMove} onPointerUp={onHandleUp} onPointerCancel={onHandleUp}>
          <div className="toggle-indicator" />
        </div>
      )}
      <aside className={`sidebar z-[2000] ${shown ? "is-open" : ""}`} aria-hidden={!shown}>
        <header className="sidebar-header">
          <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer">
            {d.uiShowLogo && <SelkiesLogo width={30} height={30} />}
            <h2>{d.uiTitle}</h2>
          </a>
          <div className="header-controls">
            <HeaderMenu actions={modes} toggles={touchModes} />
          </div>
        </header>

        {actions.streams.length > 0 && (
          <div className="sidebar-action-buttons">
            {actions.streams.map((stream) => (
              <Tile key={stream.id} active={stream.on(d)} label={t(stream.on(d) ? stream.disable : stream.enable)} onClick={() => stream.toggle(d)}>
                <stream.icon />
              </Tile>
            ))}
          </div>
        )}

        <div className="sidebar-mobile-key-actions">
          <SoftKeys showTrackpad={false} showKeyboardButton={false} palette="below" />
        </div>

        {PANELS.filter((panel) => panel.shown(ctx, "section")).map((panel) => {
          const { dialog } = panel;
          if (dialog && !panel.Body) {
            return <DialogRow key={panel.id} title={t(panel.title)} hint={t(panel.dialogHint ?? panel.title)} onOpen={() => d.openDialog(dialog)} />;
          }
          return (
            <Section key={panel.id} id={panel.contentId} title={t(panel.title)} open={!!sections[panel.id]} onToggle={() => toggleSection(panel.id)}>
              {sectionBody(panel)}
            </Section>
          );
        })}

        {actions.cite && (
          <footer className="sidebar-footer">
            <CiteLink />
          </footer>
        )}
      </aside>
    </>
  );
}
