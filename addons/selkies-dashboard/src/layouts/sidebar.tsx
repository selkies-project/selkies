/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The default layout: the classic drawer on the left edge, opened by the
 * handle there or by Ctrl+Shift+M. Its header carries the wordmark, the
 * theme pill, the layout toggle, fullscreen, and gaming mode; a row of tiles
 * toggles the streams; a touch client gets the soft keys under them; every
 * panel is a section below, each folding on its own. The sections hold the
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
import { ChevronDown, ChevronUp, Crosshair, Keyboard, Maximize, Touchpad } from "lucide-react";
import { SelkiesLogo } from "@/components/logo";
import { LayoutToggle, ThemePill } from "@/panels/appearance";
import { fractionOf, place, useViewport } from "@/panels/anchor";
import { PANELS, STREAMS, shownCards, usePanelContext, type PanelSpec } from "@/panels/catalog";
import { CardList } from "@/panels/catalog-bodies";
import { SwitchRow } from "@/panels/controls";
import { SoftKeys } from "@/panels/soft-keys";
import { CiteLink } from "@/panels/shortcuts";
import { StatsSection } from "@/panels/stats-section";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { useSettings } from "@/state/settings-context";
import { onCoreMessage } from "@/utils";
import { t } from "@/i18n";
import "./sidebar.css";

/** Height of the handle in pixels, which its drag keeps inside the window. */
const HANDLE_HEIGHT = 60;

/** One stream toggle tile. */
function Tile({ active, label, className, onClick, children }: { active: boolean; label: string; className?: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button type="button" className={`action-button ${className ?? ""}`} aria-pressed={active} aria-label={label} title={label} onClick={onClick}>
      {children}
    </button>
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

/** The drawer, its handle, and the sections. */
export default function SidebarLayout() {
  const d = useDashboard();
  const ctx = usePanelContext();
  const settings = useSettings();
  const [open, setOpen] = React.useState(false);
  const [sections, setSections] = React.useState<Record<string, boolean>>({});
  // The handle's place down the left edge, kept as a fraction of the window's
  // height so a resize keeps it in proportion and on screen.
  const [handleAt, setHandleAt] = React.useState(0.5);
  const viewport = useViewport();
  const handleSize = React.useMemo(() => ({ width: 0, height: HANDLE_HEIGHT }), []);
  const handleTop = place({ x: 0, y: handleAt }, viewport, handleSize).y;
  const drag = React.useRef<{ startY: number; startTop: number; moved: boolean } | null>(null);
  const { reportChromeOpen } = d;

  React.useEffect(() => {
    const offMessages = onCoreMessage((message) => {
      if (message.type === "toggleDashboard") setOpen((shown) => !shown);
    });
    // Entering fullscreen (button, gaming mode, Ctrl+Shift+F, or browser UI)
    // folds the drawer so the user lands in the session.
    const onFullscreen = () => { if (document.fullscreenElement) setOpen(false); };
    document.addEventListener("fullscreenchange", onFullscreen);
    return () => {
      offMessages();
      document.removeEventListener("fullscreenchange", onFullscreen);
    };
  }, []);

  const shown = open && !d.isViewer && !d.isGamingMode;
  React.useEffect(() => { reportChromeOpen(shown); }, [shown, reportChromeOpen]);

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
          <StatsSection active={shown && !!sections.stats} framerate={settings.framerate} />
        </>
      );
    }
    const Body = panel.Body;
    return Body ? <Body place="section" /> : null;
  };

  const showCoreButtons = !d.isSecondaryDisplay && d.renderable.coreButtons;
  const showKeyboardTile = d.isTouch && d.renderable.softButtons;
  const showTrackpadTile = d.isTouch && d.renderable.trackpad;
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
            <ThemePill />
            <LayoutToggle className="header-action-button" />
            {d.renderable.fullscreen && (
              <button type="button" className="header-action-button fullscreen-button" aria-label={t("fullscreenTitle")} title={t("fullscreenTitle")} onClick={d.toggleFullscreen}>
                {/* Its corners stop a unit inside the crosshair's circle: drawn to that circle's extent, the two read as one size. */}
                <Maximize viewBox="1.09 1.09 21.82 21.82" />
              </button>
            )}
            {d.renderable.gamingMode && (
              <button type="button" className="header-action-button gaming-mode-button" aria-label={t("gamingModeTitle")}
                title={`${t("gamingModeTitle")} — ${t("gamingModeHint")}`} onClick={d.toggleGamingMode}>
                <Crosshair />
              </button>
            )}
          </div>
        </header>

        {(showCoreButtons || showKeyboardTile || showTrackpadTile) && (
          <div className="sidebar-action-buttons">
            {showCoreButtons && STREAMS.filter((stream) => stream.allowed(d.renderable)).map((stream) => (
              <Tile key={stream.id} active={stream.on(d)} label={t(stream.on(d) ? stream.disable : stream.enable)} onClick={() => stream.toggle(d)}>
                <stream.icon />
              </Tile>
            ))}
            {showKeyboardTile && (
              <Tile active={d.keyboardButton} className="keyboard-toggle-button" label={t("keyboardButtonToggleTitle")} onClick={d.toggleKeyboardButton}><Keyboard /></Tile>
            )}
            {showTrackpadTile && (
              <Tile active={d.trackpad} className="trackpad-mode-button" label={t("trackpadModeTitle")} onClick={d.toggleTrackpad}><Touchpad /></Tile>
            )}
          </div>
        )}

        <div className="sidebar-mobile-key-actions">
          <SoftKeys showTrackpad={false} showKeyboardButton={false} palette="below" />
        </div>

        {PANELS.filter((panel) => panel.shown(ctx, "section")).map((panel) => (
          <Section key={panel.id} id={panel.contentId} title={t(panel.title)} open={!!sections[panel.id]} onToggle={() => toggleSection(panel.id)}>
            {sectionBody(panel)}
          </Section>
        ))}

        {d.renderable.shortcuts && <footer className="sidebar-footer"><CiteLink /></footer>}
      </aside>
    </>
  );
}
