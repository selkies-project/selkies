/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The edge layout: one round button, whose arrow points into the window, away
 * from the edge or corner it sinks into, docked in the top right corner to
 * begin with, in place of a bar. It sinks half into its edge at rest, so it
 * covers little of the stream, glowing from within in the logo's pink and purple while it rests there, and
 * comes fully out, the glow fading, for a hover, a keyboard focus, a drag, or
 * while what it opened is open. A page folded away leaves a plain sliver. A tap opens a fan of round
 * buttons around it; a drag carries it, and on release it docks on the
 * nearest edge, or in a corner, at the position it was left (`model.ts`). The
 * dock is kept per browser.
 *
 * The fan is in at most two rows of arcs facing into the window (`Fan`), on a
 * phone as on a desktop: a single arc for a handful of buttons, two rows for
 * the rest, the first buttons in the inner row. Its radius comes from the room
 * the window leaves around the button: a half circle on an edge, a quarter in
 * a corner, less in a narrow window. What two rows hold there is the page; a
 * client with more buttons than that, a touch client with its own, gets the
 * dots button as the last of each page, which turns to the next and from the
 * last back to the first. Each button's name is its tooltip, which opens
 * outward from its arc and cannot be hovered, since it lies over the next row.
 *
 * The fan holds what the toolbar's bar holds, gated the same way and
 * described by the same parts (`panels/tools.tsx`), in the order a person
 * reaches for them: the settings button, the toolbox menu, the stats,
 * fullscreen, adding a second screen, gaming mode and the touch client's
 * buttons, the buttons of the panels that float, the file and app dialogs, and
 * the figures strip's switch and the panels' translucency, and last the layout
 * toggle. The round buttons, the floating one and the project link follow the
 * translucency knob like the panels (`edge-glass` in `index.css`): the fill is
 * the page's tinted background and what lies behind is blurred with the level,
 * while a switch that is on stays solid. The stream
 * switches are not in it: the toolbox menu has them. The dialogs read the
 * catalog (`PANELS`) so their gates never fork from the menu's. The settings
 * button closes the fan and
 * hangs the settings card where the fan was; the stats button lays the
 * floating stats over the stream. The fan closes on a click outside it, on
 * Escape, when the button is dragged, and when the page is folded away.
 *
 * The strip the sidebar's "Show over the stream" lays over the stream
 * (the four status marks, then the frame rate, the video rate, and the time a
 * frame takes to the wire) rides top-center over the stream, wherever the
 * button is, on the same dark ground the sidebar's own strip takes. It is the
 * floating stats drawn for a bar (`StreamStats`' `bar` prop in
 * `panels/stats.tsx`), and mounting it is what turns the stats on, so it is
 * mounted only while the page may show stats at all. Which of the fan's
 * panels is open, and the folding on Ctrl+Shift+M and gaming mode, is
 * `useBarState` (`layouts/actions.ts`), shared with the toolbar. A page folded
 * away leaves a sliver of the button, which a tap brings back.
 *
 * Where the server shows the logo, a smaller button carrying it slides out
 * from under the floating one, along the edge, while a mouse hovers the
 * floating one or the keyboard focuses it, and opens the project's page
 * (`ProjectLink`). It tucks away while the fan is open, so the two never
 * overlap, and has no touch counterpart, since a touch has no hover; the
 * menu's footer carries the same link.
 * @module
 */
import * as React from "react";
import { CircleArrowOutUpRight, Ellipsis, ScreenShare } from "lucide-react";
import { SelkiesLogo } from "@/components/logo";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useElementSize, useViewport } from "@/panels/anchor";
import { PANELS, usePanelContext } from "@/panels/catalog";
import { StreamStats } from "@/panels/stats";
import { FloatingToolPanels, SettingsPopup, SoftKeysPanel, ToolButton, ToolboxMenu } from "@/panels/toolbox";
import { useGamingTool, useLayoutTool, useMainTools, useToolboxHasItems, useTouchTools, useTranslucencyTool, type ToolSpec } from "@/panels/tools";
import { PROJECT_URL, useDashboard } from "@/state/dashboard";
import { t } from "@/i18n";
import { useActions, useBarPrefs, useBarState, type BarState } from "../actions";
import { FAB_FALLBACK, PLACEMENT_SPECS, arrowAngle, cardStyle, dockPoint, fabShift, fanCapacity, fanItemSize, fanLayout, linkPoint, outwardSide, useDock, useDockDrag, useReveal, type Dock, type Reveal } from "./model";

/**
 * The classes that make a button of the fan round, lift it off the stream, and
 * let it follow the translucency knob (`edge-glass` blurs what lies behind it
 * with the level). A button that is off takes the page's tinted background, so
 * it reads the stream through it, and keeps it on hover, turning its mark to
 * the accent instead; a switch that is on keeps the solid fill of its variant.
 */
const FAN_BUTTON = "edge-glass size-full rounded-full border border-border shadow-lg data-[variant=secondary]:bg-background data-[variant=secondary]:hover:bg-background data-[variant=secondary]:hover:text-primary";

/** The main group's buttons the fan puts first, before and after the toolbox menu, and those it puts last, by key; the rest of the group, the panels that float, sit between. */
const LEAD = ["settings"];
const NEXT = ["stats-button", "fullscreen-button"];
const CHROME = ["figures-strip"];

/** What the floating button wears while it rests half hidden: the logo's pink and purple as a glow from within (`edge-halo` in `index.css`), which finds the button without lifting it. */
const GLOW = "edge-halo";

/** Milliseconds between one fan button's entrance and the next's. */
const FAN_STAGGER = 20;

/** What the fan takes. */
interface FanProps {
  dock: Dock;
  /** The floating button's size, which the fan is drawn around. */
  fabSize: { width: number; height: number };
  /** The floating button, whose own clicks do not close the fan. */
  anchorRef: React.RefObject<HTMLElement | null>;
  /** The bar state the fan's buttons open things by. */
  bar: BarState;
  /** The page of buttons shown, and the setter. */
  page: number;
  onPage: (page: number) => void;
  onClose: () => void;
}

/**
 * The fan of round buttons around the floating button. The buttons are listed
 * in the order a person reaches for them and placed in at most two rows of
 * arcs by `fanLayout`, on a phone as on a desktop: the first in the inner row.
 * The window decides how many two rows hold (`fanCapacity`); when the buttons
 * are more than that, as when a touch client adds its own, the fan shows what
 * fits and ends the page with the dots button, which turns to the next page
 * and from the last back to the first. It is drawn again when the window
 * changes size, so a rotation re-deals the pages. The tooltips are not
 * hoverable, since a tooltip opening outward from the inner row lies over the
 * outer row, and the pointer must pass through it to the button beneath. It
 * closes on a click outside it or on Escape; a Base UI popup is a body child,
 * so a click in one belongs to the fan, and Escape reaches the menu first
 * while the menu is open.
 * @param props Where the button is docked, the page shown, and the state the buttons open things by.
 */
function Fan({ dock, fabSize, anchorRef, bar, page, onPage, onClose }: FanProps) {
  const d = useDashboard();
  const ctx = usePanelContext();
  const viewport = useViewport();
  const main = useMainTools(bar);
  const gaming = useGamingTool();
  const layoutTool = useLayoutTool();
  const translucencyTool = useTranslucencyTool();
  const touch = useTouchTools(bar);
  const hasMenu = useToolboxHasItems();
  const item = fanItemSize();

  React.useEffect(() => {
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      const element = target instanceof Element ? target : null;
      if (anchorRef.current?.contains(target) || element?.closest("[data-edge-fan], [data-base-ui-portal]")) return;
      onClose();
    };
    const onKey = (event: KeyboardEvent) => {
      // A dialog the fan opened (files, apps) takes the Escape first, as it does over the toolbar.
      if (event.key !== "Escape" || bar.menuOpen || document.querySelector('[data-slot="dialog-content"]')) return;
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
  }, [onClose, anchorRef, bar.menuOpen]);

  const r = d.renderable;
  const primary = !d.isSecondaryDisplay;
  // The dialog panels (files, apps) and adding a second screen, each a button.
  const dialogs: ToolSpec[] = PANELS.filter((panel) => panel.dialog && panel.shown(ctx, "menu")).map((panel) => ({
    key: panel.id, label: t(panel.title), className: `${panel.id}-button`,
    icon: panel.icon && <panel.icon className="size-4" />, run: () => panel.dialog && d.openDialog(panel.dialog),
  }));
  const addScreen: ToolSpec[] = primary && r.secondScreen
    ? [{ key: "add-screen", label: t('sections.screen.addScreenTitle'), className: "add-screen-button", icon: <ScreenShare className="size-4" />, run: d.addScreen }]
    : [];
  const named = (keys: string[]) => keys.map((key) => main.find((spec) => spec.key === key)).filter((spec): spec is ToolSpec => !!spec);
  const floats = main.filter((spec) => ![...LEAD, ...NEXT, ...CHROME].includes(spec.key));
  // A null is the toolbox menu, which is not a spec: it is a menu with a button.
  const all: Array<ToolSpec | null> = [
    ...named(LEAD), ...(hasMenu ? [null] : []), ...named(NEXT), ...addScreen, ...(gaming ? [gaming] : []), ...touch, ...floats, ...dialogs, ...named(CHROME), translucencyTool, ...(layoutTool ? [layoutTool] : []),
  ];

  // Two rows hold `capacity` buttons; with more than that, the page button takes one slot of each page.
  const capacity = fanCapacity(dock, fabSize, viewport, item);
  const perPage = all.length <= capacity ? all.length : Math.max(1, capacity - 1);
  const pages = Math.max(1, Math.ceil(all.length / perPage));
  const current = Math.min(page, pages - 1);
  const shown = all.slice(current * perPage, (current + 1) * perPage);
  const more: ToolSpec | null = pages > 1
    ? { key: "more", label: `${t('topMenu.more')} ${current + 1}/${pages}`, icon: <Ellipsis className="size-4" />, run: () => onPage((current + 1) % pages) }
    : null;
  const entries: Array<ToolSpec | null> = more ? [...shown, more] : shown;
  const layout = fanLayout(dock, fabSize, viewport, item, entries.length);

  return (
    <>
      {layout.points.map((point, index) => {
        const spec = entries[index];
        const side = outwardSide(layout.angles[index]);
        return (
          // Above the floating panels, the figures strip, and the button itself, which
          // the fan never overlaps, so an open fan is never covered; the portals'
          // tooltips and menus stay above it.
          <div key={spec?.key ?? "menu"} data-edge-fan className="fixed z-[2055] animate-in fade-in zoom-in-50 duration-200"
            style={{ left: point.x, top: point.y, width: item, height: item, animationDelay: `${index * FAN_STAGGER}ms`, animationFillMode: "backwards" }}>
            {spec
              ? <ToolButton spec={spec} side={side} className={FAN_BUTTON} passive />
              : <ToolboxMenu open={bar.menuOpen} onOpenChange={bar.onMenuOpenChange} side={side} tooltipSide={side} className={FAN_BUTTON} passive />}
          </div>
        );
      })}
    </>
  );
}

/** The link's size in pixels before it is measured. */
const LINK_FALLBACK = 32;

/** What the project link takes. */
interface ProjectLinkProps {
  dock: Dock;
  /** The button's size, beside which the link slides out. */
  fabSize: { width: number; height: number };
  /** Whether the link is out; otherwise it lies under the button, unseen and out of the tab order. */
  shown: boolean;
  /** The reveal handlers it shares with the button, so crossing between the two keeps both out. */
  bind: Reveal["bind"];
}

/**
 * The project link: a smaller round button with the Selkies mark that slides
 * out from under the floating button, along the edge, while a mouse hovers or
 * the keyboard focuses it, and opens the project's page in a new tab. It is
 * mounted only where the server shows the logo, and measures itself on mount.
 * The hover and focus handlers sit on its wrapper, and the focus ones reach it
 * from the link inside as they bubble. The link, hidden, is out of the tab
 * order and of the pointer's reach.
 * @param props Where the button is docked, and whether the link is out.
 */
function ProjectLink({ dock, fabSize, shown, bind }: ProjectLinkProps) {
  const d = useDashboard();
  const viewport = useViewport();
  const ref = React.useRef<HTMLDivElement>(null);
  const size = useElementSize(ref, { width: LINK_FALLBACK, height: LINK_FALLBACK });
  const { at, tuck } = linkPoint(dock, fabSize, size, viewport);
  return (
    <div ref={ref} {...bind}
      className={cn("edge-link fixed z-[2049] size-(--bar-button) transition-[left,top,transform,opacity] duration-200", shown ? "opacity-100" : "pointer-events-none opacity-0")}
      style={{ left: at.x, top: at.y, transform: shown ? "none" : `translate(${tuck.x}px, ${tuck.y}px) scale(0.5)` }}>
      <Tooltip disableHoverablePopup>
        <TooltipTrigger render={
          <a href={PROJECT_URL} target="_blank" rel="noopener noreferrer" aria-label={d.uiTitle} tabIndex={shown ? 0 : -1} aria-hidden={!shown}
            className="edge-glass flex size-full items-center justify-center rounded-full border bg-background text-foreground shadow-lg outline-none transition-colors hover:text-primary focus-visible:ring-[3px] focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-black/70" />
        }>
          <SelkiesLogo width={18} height={18} />
        </TooltipTrigger>
        <TooltipContent side={PLACEMENT_SPECS[dock.placement].inward}>{d.uiTitle}</TooltipContent>
      </Tooltip>
    </div>
  );
}

/** The button, the project link beside it, the fan and the settings card hanging from it, and the figures strip. */
export default function EdgeLayout() {
  const bar = useBarState();
  const { barStrip, translucency } = useBarPrefs();
  const { softKeys } = useActions();
  const d = useDashboard();
  const viewport = useViewport();
  const [dock, setDock] = useDock();
  const buttonRef = React.useRef<HTMLButtonElement>(null);
  const size = useElementSize(buttonRef, { width: FAB_FALLBACK, height: FAB_FALLBACK });
  const drag = useDockDrag(dock, setDock, viewport, size);
  const [fanOpen, setFanOpen] = React.useState(false);
  const [page, setPage] = React.useState(0);
  const reveal = useReveal();
  // A fold, from Ctrl+Shift+M or gaming mode, or a drag, shuts the fan: adjusted while rendering, since it is a reset on a change of `folded`.
  const folded = bar.collapsed || drag.free !== null;
  const [wasFolded, setWasFolded] = React.useState(folded);
  if (wasFolded !== folded) {
    setWasFolded(folded);
    if (folded) {
      setFanOpen(false);
      setPage(0);
    }
  }
  const closeFan = React.useCallback(() => {
    setFanOpen(false);
    setPage(0);
  }, []);

  const spec = PLACEMENT_SPECS[dock.placement];
  const fanShown = fanOpen && !folded;
  const out = reveal.revealed || drag.free !== null || fanShown || bar.settingsOpen || bar.menuOpen;
  const glowing = !out && !bar.collapsed;
  const at = drag.free ?? dockPoint(dock, viewport, size);
  const showStats = !d.isSecondaryDisplay && d.renderable.stats;
  // The settings button and a floating panel's shut the fan, so what they open
  // is not drawn over the fan, nor the fan over it.
  const fanBar: BarState = {
    ...bar,
    toggleSettings: () => { closeFan(); bar.toggleSettings(); },
    toggleFloating: (id: string) => { closeFan(); bar.toggleFloating(id); },
  };

  const onTap = () => {
    if (drag.endsDrag()) return;
    if (bar.collapsed) bar.setCollapsed(false);
    else if (bar.settingsOpen) bar.closeSettings();
    else if (fanOpen) closeFan();
    else setFanOpen(true);
  };

  return (
    <>
      {/* Where the button rests, sunk into its edge: a pointer there keeps it out
          and a click there is the button's, so the button sliding out from under
          a pointer pressed into a corner does not leave it and sink back. */}
      {drag.free === null && !bar.collapsed && (
        <div aria-hidden className="fixed z-[2049]" {...reveal.bind} onClick={onTap}
          style={{ left: at.x, top: at.y, width: size.width, height: size.height, transform: fabShift(dock, false, false) }} />
      )}
      <button ref={buttonRef} type="button" aria-label={t('topMenu.showToolbar')} title={t('topMenu.showToolbar')} aria-expanded={fanShown}
        {...drag.handlers}
        {...reveal.bind}
        onClick={onTap}
        className={cn("edge-button edge-glass fixed z-[2050] flex size-[calc(var(--bar-button)*1.4)] touch-none items-center justify-center rounded-full border bg-background text-foreground outline-none select-none cursor-grab focus-visible:ring-[3px] focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-black/70 active:cursor-grabbing",
          drag.free ? "transition-none" : "transition-[left,top,transform,opacity,box-shadow,border-color] duration-200", out ? "opacity-100" : "opacity-70", glowing ? GLOW : "shadow-lg")}
        style={{ left: at.x, top: at.y, transform: fabShift(dock, out, bar.collapsed) }}>
        <CircleArrowOutUpRight className="size-5" style={{ transform: `rotate(${arrowAngle(dock)}deg)` }} />
      </button>

      {d.uiShowLogo && <ProjectLink dock={dock} fabSize={size} shown={reveal.revealed && drag.free === null && !bar.collapsed && !fanShown} bind={reveal.bind} />}

      {fanShown && <Fan dock={dock} fabSize={size} anchorRef={buttonRef} bar={fanBar} page={page} onPage={setPage} onClose={closeFan} />}

      <SettingsPopup open={bar.settingsOpen} onClose={bar.closeSettings} barRef={buttonRef} style={cardStyle(dock, size, viewport)} className={spec.popup} />

      {/* The strip's own spot whatever edge the button is docked on: top-center over
          the stream, on the dark ground the sidebar's strip takes. */}
      {showStats && barStrip && (
        <div className={cn("dark pointer-events-none fixed top-2 left-1/2 z-[2040] flex max-w-[calc(100vw-1rem)] -translate-x-1/2 items-center justify-center rounded-md px-2.5 py-1 shadow-lg chrome-panel",
          ["bg-black/70", "bg-black/55", "bg-black/45", "bg-black/35"][translucency])}>
          <StreamStats bar />
        </div>
      )}

      <FloatingToolPanels open={bar.floating} onClose={bar.toggleFloating} />

      <SoftKeysPanel open={softKeys && bar.softKeysOpen && !bar.collapsed} />
    </>
  );
}
