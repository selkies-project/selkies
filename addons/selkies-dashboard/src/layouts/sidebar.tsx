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
import { ChevronDown, ChevronUp, Crosshair, FileUp, FolderOpen, Gamepad2, Keyboard, LayoutGrid, Maximize, Mic, Monitor, Touchpad, Volume2, Webcam } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SelkiesLogo } from "@/components/logo";
import { LayoutToggle, ThemePill } from "@/panels/appearance";
import { fractionOf, place, useViewport } from "@/panels/anchor";
import { Clipboard } from "@/panels/clipboard";
import { SwitchRow, Clip } from "@/panels/controls";
import { GamepadControls } from "@/panels/gamepads";
import { Printing } from "@/panels/printing";
import { AudioSettings, ScreenSettings, VideoSettings } from "@/panels/settings";
import { Sharing } from "@/panels/sharing";
import { ShortcutsMenu } from "@/panels/shortcuts";
import { SoftKeys } from "@/panels/soft-keys";
import { StatsSection } from "@/panels/stats-section";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { useSettings, useSettingsPanels } from "@/state/settings-context";
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
  const panels = useSettingsPanels();
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
    const onMessage = (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      if (event.data?.type === "toggleDashboard") setOpen((shown) => !shown);
    };
    // Entering fullscreen (button, gaming mode, Ctrl+Shift+F, or browser UI)
    // folds the drawer so the user lands in the session.
    const onFullscreen = () => { if (document.fullscreenElement) setOpen(false); };
    window.addEventListener("message", onMessage);
    document.addEventListener("fullscreenchange", onFullscreen);
    return () => {
      window.removeEventListener("message", onMessage);
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
  const section = (name: string, id: string, title: string, body: React.ReactNode) => (
    <Section id={id} title={title} open={!!sections[name]} onToggle={() => toggleSection(name)}>{body}</Section>
  );

  const showCoreButtons = !d.isSecondaryDisplay && d.renderable.coreButtons;
  const showKeyboardTile = d.isTouch && d.renderable.softButtons;
  const showTrackpadTile = d.isTouch && d.renderable.trackpad;
  const primary = !d.isSecondaryDisplay;
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
                <Maximize />
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
            {showCoreButtons && d.renderable.videoToggle && (
              <Tile active={d.video} label={t(d.video ? "buttons.videoStreamDisableTitle" : "buttons.videoStreamEnableTitle")} onClick={d.toggleVideo}><Monitor /></Tile>
            )}
            {showCoreButtons && d.renderable.audioToggle && (
              <Tile active={d.audio} label={t(d.audio ? "buttons.audioStreamDisableTitle" : "buttons.audioStreamEnableTitle")} onClick={d.toggleAudio}><Volume2 /></Tile>
            )}
            {showCoreButtons && d.renderable.microphoneToggle && (
              <Tile active={d.microphone} label={t(d.microphone ? "buttons.microphoneDisableTitle" : "buttons.microphoneEnableTitle")} onClick={d.toggleMicrophone}><Mic /></Tile>
            )}
            {showCoreButtons && d.renderable.webcamToggle && (
              <Tile active={d.webcam} label={t(d.webcam ? "buttons.webcamDisableTitle" : "buttons.webcamEnableTitle")} onClick={d.toggleWebcam}><Webcam /></Tile>
            )}
            {showCoreButtons && d.renderable.gamepadToggle && (
              <Tile active={d.gamepad} label={t(d.gamepad ? "buttons.gamepadDisableTitle" : "buttons.gamepadEnableTitle")} onClick={d.toggleGamepad}><Gamepad2 /></Tile>
            )}
            {showKeyboardTile && (
              <Tile active={d.keyboardButton} className="keyboard-toggle-button" label={t("keyboardButtonToggleTitle")} onClick={d.toggleKeyboardButton}><Keyboard /></Tile>
            )}
            {showTrackpadTile && (
              <Tile active={d.trackpad} className="trackpad-mode-button" label={t("trackpadModeTitle")} onClick={d.toggleTrackpad}><Touchpad /></Tile>
            )}
          </div>
        )}

        <div className="sidebar-mobile-key-actions">
          <SoftKeys showTrackpad={false} showKeyboardButton={false} />
        </div>

        {panels.video && section("video", "settings-content", t("sections.video.title"), <VideoSettings />)}
        {panels.screen && section("screen", "screen-settings-content", t("sections.screen.title"), <ScreenSettings />)}
        {panels.audio && section("audio", "audio-settings-content", t("sections.audio.title"), <AudioSettings />)}
        {primary && d.renderable.stats && section("stats", "stats-content", t("sections.stats.title"), (
          <>
            <SwitchRow id="statsStripToggle" label={t("sections.stats.stripLabel")} checked={d.statsOpen} onToggle={() => d.setStatsOpen(!d.statsOpen)} />
            <StatsSection active={shown && !!sections.stats} framerate={settings.framerate} />
          </>
        ))}
        {primary && d.renderable.clipboard && section("clipboard", "clipboard-content", t("sections.clipboard.title"), <Clipboard />)}
        {primary && d.renderable.files && (d.renderable.fileUpload || d.renderable.fileDownload) && section("files", "files-content", t("sections.files.title"), (
          <div className="files-actions">
            {d.renderable.fileUpload && (
              <Button variant="outline" className="min-w-0" title={t("sections.files.uploadButton")} onClick={() => window.dispatchEvent(new CustomEvent("requestFileUpload"))}>
                <FileUp /><Clip>{t("sections.files.uploadButton")}</Clip>
              </Button>
            )}
            {d.renderable.fileDownload && (
              <Button variant="outline" className="min-w-0" title={t("sections.files.downloadButtonTitle")} onClick={() => d.openDialog("files")}>
                <FolderOpen /><Clip>{t("sections.files.downloadButtonTitle")}</Clip>
              </Button>
            )}
          </div>
        ))}
        {primary && d.printJobCount > 0 && section("printing", "printing-content", t("sections.printing.title"), <Printing />)}
        {primary && d.renderable.apps && section("apps", "apps-content", t("sections.apps.title"), (
          <Button variant="outline" className="w-full min-w-0" title={t("sections.apps.openButton")} onClick={() => d.openDialog("apps")}><LayoutGrid /><Clip>{t("sections.apps.openButton")}</Clip></Button>
        ))}
        {primary && d.renderable.sharing && section("sharing", "sharing-content", t("sections.sharing.title"), <Sharing show showUrl />)}
        {primary && d.renderable.gamepads && section("gamepads", "gamepads-content", t("sections.gamepads.title"), <GamepadControls rumbleButton />)}
        {d.renderable.shortcuts && section("shortcuts", "shortcuts-content", t("sections.shortcuts.title"), <ShortcutsMenu />)}
      </aside>
    </>
  );
}
