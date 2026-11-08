/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the edge layout decides apart from its markup: where its button is
 * docked, how a drag moves it there, and where what opens from it is placed.
 *
 * The button is docked on one of the window's four edges at a position along
 * it (`Dock`). The position is a fraction of the room the edge leaves the
 * button, so a resize or a rotation keeps it in proportion, as the other
 * floating chrome does (`panels/anchor.ts`). `PLACEMENT_SPECS` holds what
 * differs between the edges: whether the edge is a side, the direction the
 * button sinks into it, the side its menus and tooltips open on, and the
 * entry animation of what opens. The component reads this table, so it never
 * branches on the edge.
 *
 * A drag (`useDockDrag`) carries the button freely under the pointer. On
 * release it docks on the edge its center is nearest, at the position it was
 * left, or in the corner where that is within `CORNER_SNAP` button sizes of
 * one: a corner is the offset 0 or 1, and the button sits flush in it. A press
 * that never travels `DRAG_SLOP` pixels is a tap, so a tap on a touch screen
 * is never taken for a drag. At rest the button sinks half into its edge
 * (`PEEK`), or diagonally into its corner so that it hides behind both edges
 * (`CORNER_PEEK`), and comes fully out for a hover, a focus, a drag, or while
 * anything it opened is open (`fabShift`).
 *
 * A tap opens a fan of round buttons in rows of arcs around the floating
 * button (`fanLayout`). An arc is the half circle facing into the window, cut
 * short where the window's other edges and corners would clip a button, so it
 * is a quarter circle in a corner and a smaller arc in a narrow window. A few
 * buttons make a single arc; more make two rows, one a button and a gap
 * outside the other, on the smallest radius, up to what the window allows,
 * at which the two arcs hold them a button's width and a gap apart. There are
 * never more than two rows, on a phone as on a desktop. What two rows hold in
 * the window is the page size (`fanCapacity`); a layout with more buttons
 * shows them in pages, one slot of each page being the button that turns to
 * the next. A button's tooltip opens outward from its arc (`outwardSide`).
 *
 * The button's arrow points the opposite way to the one the button sinks
 * (`arrowAngle`), into the window, so it reads as a pull out of the edge or
 * corner it is hiding in. While a mouse
 * hovers it, or the keyboard focuses it, a smaller button carrying the
 * project link slides out from under it, along the edge toward the middle of
 * it (`linkPoint`, `useReveal`).
 *
 * The dock is kept per browser under the prefixed `edge_placement` and
 * `edge_offset` keys, so a reload lands where the page was left; a page that
 * has chosen none gets the top right corner.
 * @module
 */
import * as React from "react";
import { fractionOf, place, type Point, type Size } from "@/panels/anchor";
import type { Side } from "@/panels/toolbox";
import { getPrefixedKey } from "@/utils";

/** The edge of the window the button is docked on. */
export type Placement = "top" | "bottom" | "left" | "right";

/** Every placement, so a stored value is checked against literals. */
export const PLACEMENTS: Placement[] = ["top", "right", "bottom", "left"];

/** Where the button is docked: an edge, and how far along it from 0 (the start) to 1 (the end). */
export interface Dock {
  placement: Placement;
  offset: number;
}

/** The dock a page that has chosen none gets: the top right corner. */
const DEFAULT_DOCK: Dock = { placement: "top", offset: 1 };

/** Pixels the button keeps from the corners of its edge, and what opens from it keeps from the window's edges. */
export const MARGIN = 8;

/** Pixels between the button and what opens from it. */
const GAP = 8;

/** Pixels a press must travel before it is a drag and not a tap. */
const DRAG_SLOP = 5;

/** The button's size in pixels before it is measured. */
export const FAB_FALLBACK = 44;

/** How much of the button sinks into its edge at rest, and how much of a button folded away by Ctrl+Shift+M or gaming mode. */
const PEEK = 0.5;
const FOLDED = 0.82;

/**
 * How much of the button sinks into a corner at rest, and when folded away,
 * along each of the two edges it meets: less than on an edge, since two sides
 * hide it. Sunk the edge's 82% along both, a folded button would leave a
 * square of the corner under 9 pixels across, too small to tap.
 */
const CORNER_PEEK = 0.4;
const CORNER_FOLDED = 0.6;

/** How close to a corner a drop snaps into it, in button sizes along the edge. */
const CORNER_SNAP = 1.25;

/** The width of the settings card (`panels/settings-card.tsx`), which a card docked beside the button centers on. */
const CARD_WIDTH = 272;

/** Pixels between the button and the project link that slides out beside it. */
const LINK_GAP = 6;

/** Milliseconds a hover or a focus is held after it leaves, so the pointer can cross to the project link. */
const REVEAL_HOLD = 160;

/** How one placement is drawn. */
export interface PlacementSpec {
  /** The edge is a side of the window, so what opens from it is a column away from it. */
  vertical: boolean;
  /** The direction, in button sizes, the button sinks into the edge. */
  outward: Point;
  /** The side of the button its menu and tooltip open on, away from the edge. */
  inward: Side;
  /** The entry of what opens from the button: it slides in from the button's side. */
  popup: string;
}

export const PLACEMENT_SPECS: Record<Placement, PlacementSpec> = {
  top: { vertical: false, outward: { x: 0, y: -1 }, inward: "bottom", popup: "slide-in-from-top-2" },
  bottom: { vertical: false, outward: { x: 0, y: 1 }, inward: "top", popup: "slide-in-from-bottom-2" },
  left: { vertical: true, outward: { x: -1, y: 0 }, inward: "right", popup: "slide-in-from-left-2" },
  right: { vertical: true, outward: { x: 1, y: 0 }, inward: "left", popup: "slide-in-from-right-2" },
};

/**
 * Whether the dock is a corner: the button at either end of its edge, flush
 * with the window's corner instead of `MARGIN` short of it.
 * @param dock Where it is docked.
 */
export function isCorner(dock: Dock): boolean {
  return dock.offset <= 0 || dock.offset >= 1;
}

/**
 * Where the button's top left is when it sits fully out on its edge.
 * @param dock Where it is docked.
 * @param viewport The window's size.
 * @param size The button's size.
 */
export function dockPoint(dock: Dock, viewport: Size, size: Size): Point {
  const along = place({ x: dock.offset, y: dock.offset }, viewport, size, isCorner(dock) ? 0 : MARGIN);
  switch (dock.placement) {
    case "top": return { x: along.x, y: 0 };
    case "bottom": return { x: along.x, y: Math.max(0, viewport.height - size.height) };
    case "left": return { x: 0, y: along.y };
    default: return { x: Math.max(0, viewport.width - size.width), y: along.y };
  }
}

/**
 * The dock a drag leaves the button on: the edge its center is nearest, at
 * the position along that edge it was dropped, or the corner of that edge
 * where the drop is within `CORNER_SNAP` button sizes of it.
 * @param point The button's top left where it was dropped.
 * @param viewport The window's size.
 * @param size The button's size.
 */
export function nearestDock(point: Point, viewport: Size, size: Size): Dock {
  const x = point.x + size.width / 2;
  const y = point.y + size.height / 2;
  const gaps: Array<[Placement, number]> = [["top", y], ["bottom", viewport.height - y], ["left", x], ["right", viewport.width - x]];
  const [placement] = gaps.reduce((best, gap) => (gap[1] < best[1] ? gap : best));
  const vertical = PLACEMENT_SPECS[placement].vertical;
  const along = vertical ? point.y : point.x;
  const room = vertical ? viewport.height - size.height : viewport.width - size.width;
  const snap = CORNER_SNAP * (vertical ? size.height : size.width);
  if (along < snap) return { placement, offset: 0 };
  if (room - along < snap) return { placement, offset: 1 };
  const fraction = fractionOf(point, viewport, size, MARGIN);
  return { placement, offset: vertical ? fraction.y : fraction.x };
}

/**
 * The direction, in button sizes, the button sinks: out through its edge, and
 * in a corner out through the second edge it meets as well, so that it hides
 * diagonally behind both.
 * @param dock Where it is docked.
 */
function outwardOf(dock: Dock): Point {
  const { outward, vertical } = PLACEMENT_SPECS[dock.placement];
  if (!isCorner(dock)) return outward;
  const end = dock.offset <= 0 ? -1 : 1;
  return vertical ? { x: outward.x, y: end } : { x: end, y: outward.y };
}

/**
 * The rotation, in degrees from 0 up to 360, that points an arrow drawn
 * toward the upper right the opposite way to the one the button sinks, into
 * the window: half a turn in the top right corner, where the arrow points
 * down and to the left, a quarter turn clockwise for each corner after it,
 * and an eighth turn for an edge between two corners.
 * @param dock Where it is docked.
 */
export function arrowAngle(dock: Dock): number {
  const { x, y } = outwardOf(dock);
  const turned = Math.round((Math.atan2(y, x) * 180) / Math.PI + 45 + 180);
  return ((turned % 360) + 360) % 360;
}

/**
 * The transform that sinks the button into its edge, or into its corner.
 * @param dock Where it is docked.
 * @param out The button is fully out: hovered, focused, dragged, or holding something open.
 * @param folded The page folded it away (Ctrl+Shift+M or gaming mode), leaving a sliver to bring it back.
 */
export function fabShift(dock: Dock, out: boolean, folded: boolean): string {
  const corner = isCorner(dock);
  const share = out ? 0 : folded ? (corner ? CORNER_FOLDED : FOLDED) : corner ? CORNER_PEEK : PEEK;
  const outward = outwardOf(dock);
  return `translate(${outward.x * share * 100}%, ${outward.y * share * 100}%)`;
}

/** A button of the fan is this many pixels across, and this many on a coarse pointer, where a finger needs more. */
const FAN_ITEM = 40;
const FAN_ITEM_COARSE = 46;

/** Pixels between two buttons of the fan along its arc, and between the first ring and the floating button. */
const FAN_GAP = 6;
const FAN_CLEARANCE = 14;

/** The largest radius, in pixels, a fan reaches, and the share of the window's short side it may take. */
const FAN_MAX_RADIUS = 240;
const FAN_MAX_SHARE = 0.45;

/** How much the radius grows between the sizes tried. */
const FAN_RADIUS_STEP = 6;

/** The most buttons that make a single arc: more are two rows. */
const FAN_ONE_ROW = 5;

/** The most rows the fan has, on every screen; what does not fit in them goes on further pages. */
const FAN_ROWS = 2;

const DEGREE = Math.PI / 180;

/** The size of a fan button on this device: larger where the pointer is coarse. */
export function fanItemSize(): number {
  return window.matchMedia("(pointer: coarse)").matches ? FAN_ITEM_COARSE : FAN_ITEM;
}

/** The angle, in radians from the +x axis with y down, of the way into the window from an edge. */
function inwardAngle(placement: Placement): number {
  const { outward } = PLACEMENT_SPECS[placement];
  return Math.atan2(-outward.y, -outward.x);
}

/** The center of the button when it sits fully out, which the fan is drawn around. */
function fanCenter(dock: Dock, size: Size, viewport: Size): Point {
  const at = dockPoint(dock, viewport, size);
  return { x: at.x + size.width / 2, y: at.y + size.height / 2 };
}

/**
 * The radii the fan may take: from just clear of the button up to the
 * largest the window allows, so a small window gets a small fan.
 * @param size The button's size.
 * @param viewport The window's size.
 * @param item A fan button's size.
 */
function fanBounds(size: Size, viewport: Size, item: number): { low: number; high: number } {
  const low = size.width / 2 + item / 2 + FAN_CLEARANCE;
  const high = Math.max(low, Math.min(FAN_MAX_RADIUS, FAN_MAX_SHARE * Math.min(viewport.width, viewport.height)));
  return { low, high };
}

/**
 * The longest run of angles on the half circle facing into the window at
 * which a button of the fan stays inside the window, which the window's
 * other edges and corners cut short. Next to a corner it is a quarter
 * circle, on a plain edge a half, and in a narrow window less.
 * @param normal The direction into the window.
 * @param center The button's center.
 * @param viewport The window's size.
 * @param item A fan button's size.
 * @param radius The fan's radius.
 * @returns The first and last angle of the run, or null where no angle fits.
 */
function fanArc(normal: number, center: Point, viewport: Size, item: number, radius: number): [number, number] | null {
  const room = item / 2 + MARGIN;
  const first = normal - Math.PI / 2;
  let best: [number, number] | null = null;
  let start: number | null = null;
  for (let i = 0; i <= 181; i++) {
    const angle = first + i * DEGREE;
    const x = center.x + radius * Math.cos(angle);
    const y = center.y + radius * Math.sin(angle);
    const fits = i <= 180 && x >= room && x <= viewport.width - room && y >= room && y <= viewport.height - room;
    if (fits && start === null) start = angle;
    if (!fits && start !== null) {
      const end = angle - DEGREE;
      if (!best || end - start > best[1] - best[0]) best = [start, end];
      start = null;
    }
  }
  return best;
}

/** How many buttons an arc holds at a radius, a button's width and a gap apart along it. */
function fanSlots(arc: [number, number] | null, radius: number, item: number): number {
  return arc ? Math.floor((radius * (arc[1] - arc[0])) / (item + FAN_GAP)) + 1 : 0;
}

/** One row of the fan: an arc at a radius, and how many buttons it holds. */
interface FanRow {
  radius: number;
  arc: [number, number] | null;
  slots: number;
}

/** Where the buttons of the fan sit. */
export interface FanLayout {
  /** The top left of each button placed, in the order the buttons were given: the inner row first, each row along its arc. */
  points: Point[];
  /** The direction from the floating button to each, in radians, which its tooltip opens along. */
  angles: number[];
}

/** What the fan is drawn in: the window's room around the button, and the sizes of what it holds. */
interface FanSpace {
  center: Point;
  /** The direction into the window. */
  normal: number;
  viewport: Size;
  item: number;
  /** The radii the fan may take, innermost to outermost. */
  low: number;
  high: number;
  /** The distance between two rows, and the most rows the window has room for, never above `FAN_ROWS`. */
  pitch: number;
  mostRows: number;
}

/** The room the fan has. */
function fanSpace(dock: Dock, size: Size, viewport: Size, item: number): FanSpace {
  const { low, high } = fanBounds(size, viewport, item);
  const pitch = item + FAN_GAP;
  // A row nearer the button than `low` would lie on it, so the window decides how many rows there can be.
  const mostRows = Math.min(FAN_ROWS, Math.max(1, Math.floor((high - low) / pitch) + 1));
  return { center: fanCenter(dock, size, viewport), normal: inwardAngle(dock.placement), viewport, item, low, high, pitch, mostRows };
}

/** `rows` rows ending at the radius `outer`, the innermost first. */
function fanRows(space: FanSpace, outer: number, rows: number): FanRow[] {
  return Array.from({ length: rows }, (_, i) => {
    const radius = outer - (rows - 1 - i) * space.pitch;
    const arc = fanArc(space.normal, space.center, space.viewport, space.item, radius);
    return { radius, arc, slots: fanSlots(arc, radius, space.item) };
  });
}

const rowsHold = (rows: FanRow[]) => rows.reduce((sum, row) => sum + row.slots, 0);

/**
 * The outer radii to try for `rows` rows: from the smallest that keeps the
 * inner row clear of the button up to the largest the window allows.
 */
function* outerRadii(space: FanSpace, rows: number): Generator<number> {
  for (let outer = space.low + (rows - 1) * space.pitch; ; outer = Math.min(outer + FAN_RADIUS_STEP, space.high)) {
    yield outer;
    if (outer >= space.high) return;
  }
}

/** The rows, as many as the window has room for, at the radius where they hold the most buttons. */
function fullestRows(space: FanSpace): FanRow[] {
  let best: FanRow[] = [];
  for (const outer of outerRadii(space, space.mostRows)) {
    const rows = fanRows(space, outer, space.mostRows);
    if (rowsHold(rows) > rowsHold(best)) best = rows;
  }
  return best;
}

/**
 * How many buttons the fan holds in this window: the most that two rows
 * (one where the window has room for no more) hold at any radius the window
 * allows. A layout with more buttons shows them in pages.
 * @param dock Where the button is docked.
 * @param size The button's size.
 * @param viewport The window's size.
 * @param item A fan button's size.
 */
export function fanCapacity(dock: Dock, size: Size, viewport: Size, item: number): number {
  return rowsHold(fullestRows(fanSpace(dock, size, viewport, item)));
}

/**
 * Places `count` buttons in rows of arcs around the floating button, never
 * more than two rows on any screen. A few make a single arc. More make two
 * rows, one a button and a gap outside the other, on the smallest outer
 * radius whose two arcs hold them. The buttons are divided among the rows in
 * proportion to what each holds, so the rows fill evenly, the first of them
 * in the inner row, and each row's buttons are centered on its arc. A caller
 * with more buttons than `fanCapacity` pages them; asked to place more than
 * fit, the layout places as many as do, from the first on.
 * @param dock Where the button is docked.
 * @param size The button's size.
 * @param viewport The window's size.
 * @param item A fan button's size.
 * @param count How many buttons to place, no more than `fanCapacity`.
 * @returns The places of the buttons that fit.
 */
export function fanLayout(dock: Dock, size: Size, viewport: Size, item: number, count: number): FanLayout {
  const space = fanSpace(dock, size, viewport, item);
  const { center, pitch } = space;
  let chosen: FanRow[] | null = null;
  // A window with room for one row tries that one row for any count.
  for (let rows = Math.min(count <= FAN_ONE_ROW ? 1 : 2, space.mostRows); rows <= space.mostRows && !chosen; rows++) {
    for (const outer of outerRadii(space, rows)) {
      const candidate = fanRows(space, outer, rows);
      if (rowsHold(candidate) >= count) {
        chosen = candidate;
        break;
      }
    }
  }
  // More than fit: the rows that hold the most, as `fanCapacity` counts them.
  const rows = chosen ?? fullestRows(space);

  const placed = Math.min(count, rowsHold(rows));
  let remaining = placed;
  let room = rowsHold(rows);
  const points: Point[] = [];
  const angles: number[] = [];
  for (const row of rows) {
    const share = room > 0 ? Math.min(row.slots, Math.round((remaining * row.slots) / room)) : 0;
    remaining -= share;
    room -= row.slots;
    if (!row.arc || share === 0) continue;
    const middle = (row.arc[0] + row.arc[1]) / 2;
    const step = pitch / row.radius;
    for (let i = 0; i < share; i++) {
      const angle = middle + (i - (share - 1) / 2) * step;
      angles.push(angle);
      points.push({ x: center.x + row.radius * Math.cos(angle) - item / 2, y: center.y + row.radius * Math.sin(angle) - item / 2 });
    }
  }
  return { points, angles };
}

/**
 * The side of a fan button its tooltip opens on: the one that faces away
 * from the floating button, so it never lies on the buttons beside it.
 * @param angle The direction from the floating button to the fan button.
 */
export function outwardSide(angle: number): Side {
  const x = Math.cos(angle);
  const y = Math.sin(angle);
  if (Math.abs(x) > Math.abs(y)) return x > 0 ? "right" : "left";
  return y > 0 ? "bottom" : "top";
}

/**
 * The style that hangs the settings card against the button. Its height is
 * not known here, so it hangs from the button's edge by a CSS offset, and on
 * a side it centers on the window, where the card, which never grows past
 * the window's height, always fits.
 * @param dock Where the button is docked.
 * @param size The button's size.
 * @param viewport The window's size.
 */
export function cardStyle(dock: Dock, size: Size, viewport: Size): React.CSSProperties {
  const at = dockPoint(dock, viewport, size);
  const width = Math.min(CARD_WIDTH, viewport.width - 2 * MARGIN);
  const left = Math.max(MARGIN, Math.min(at.x + size.width / 2 - width / 2, viewport.width - width - MARGIN));
  switch (dock.placement) {
    case "top": return { top: at.y + size.height + GAP, left };
    case "bottom": return { bottom: viewport.height - at.y + GAP, left };
    case "left": return { left: at.x + size.width + GAP, top: 0, bottom: 0, marginBlock: "auto", height: "fit-content" };
    default: return { right: viewport.width - at.x + GAP, top: 0, bottom: 0, marginBlock: "auto", height: "fit-content" };
  }
}

/**
 * Where the project link rests when it is out, and how far under the button
 * it starts. The link sits beside the button along the edge, on the side that
 * has room (toward the middle of the edge), level with the button's center;
 * it slides out from under the button by the `tuck` offset.
 * @param dock Where the button is docked.
 * @param size The button's size.
 * @param link The link's size.
 * @param viewport The window's size.
 * @returns The link's top left when out, and the translation that puts its center under the button's.
 */
export function linkPoint(dock: Dock, size: Size, link: Size, viewport: Size): { at: Point; tuck: Point } {
  const base = dockPoint(dock, viewport, size);
  const forward = dock.offset <= 0.5;
  const at = PLACEMENT_SPECS[dock.placement].vertical
    ? { x: base.x + (size.width - link.width) / 2, y: forward ? base.y + size.height + LINK_GAP : base.y - link.height - LINK_GAP }
    : { x: forward ? base.x + size.width + LINK_GAP : base.x - link.width - LINK_GAP, y: base.y + (size.height - link.height) / 2 };
  const tuck = { x: base.x + size.width / 2 - (at.x + link.width / 2), y: base.y + size.height / 2 - (at.y + link.height / 2) };
  return { at, tuck };
}

/** What `useReveal` returns. */
export interface Reveal {
  /** The pointer or the keyboard is on the button or on what slides out beside it. */
  revealed: boolean;
  /** Spread on the button and on what slides out beside it, so the two share one state. */
  bind: Pick<React.HTMLAttributes<HTMLElement>, "onPointerEnter" | "onPointerLeave" | "onFocus" | "onBlur">;
}

/**
 * Whether the button's hover or focus is on, shared with what slides out
 * beside it. A mouse pointer or a keyboard focus reveals; a touch does not,
 * since it has no hover. Leaving holds for `REVEAL_HOLD` milliseconds first,
 * so the pointer crosses the gap between the two without the second going.
 */
export function useReveal(): Reveal {
  const [hovered, setHovered] = React.useState(false);
  const [focused, setFocused] = React.useState(false);
  const timers = React.useRef<{ hover?: number; focus?: number }>({});
  React.useEffect(() => {
    const held = timers.current;
    return () => {
      window.clearTimeout(held.hover);
      window.clearTimeout(held.focus);
    };
  }, []);
  const hold = (key: "hover" | "focus", set: (on: boolean) => void, on: boolean) => {
    window.clearTimeout(timers.current[key]);
    if (on) set(true);
    else timers.current[key] = window.setTimeout(() => set(false), REVEAL_HOLD);
  };
  return {
    revealed: hovered || focused,
    bind: {
      onPointerEnter: (event) => { if (event.pointerType === "mouse") hold("hover", setHovered, true); },
      onPointerLeave: () => hold("hover", setHovered, false),
      onFocus: (event) => { if (event.target.matches(":focus-visible")) hold("focus", setFocused, true); },
      onBlur: () => hold("focus", setFocused, false),
    },
  };
}

/** Reads the stored dock: the edge and position it names, each defaulted where it is missing or not a value. */
function readDock(): Dock {
  const placement = localStorage.getItem(getPrefixedKey("edge_placement"));
  const stored = localStorage.getItem(getPrefixedKey("edge_offset"));
  const offset = stored === null ? NaN : Number(stored);
  return {
    placement: PLACEMENTS.find((candidate) => candidate === placement) ?? DEFAULT_DOCK.placement,
    offset: Number.isFinite(offset) && offset >= 0 && offset <= 1 ? offset : DEFAULT_DOCK.offset,
  };
}

/**
 * The dock the button is on, written back to storage as it changes.
 * @returns The dock and its setter.
 */
export function useDock(): [Dock, (dock: Dock) => void] {
  const [dock, setDock] = React.useState<Dock>(readDock);
  React.useEffect(() => {
    localStorage.setItem(getPrefixedKey("edge_placement"), dock.placement);
    localStorage.setItem(getPrefixedKey("edge_offset"), String(dock.offset));
  }, [dock]);
  return [dock, setDock];
}

/** What `useDockDrag` returns. */
export interface DockDrag {
  /** The button's top left while it is carried; null at rest. */
  free: Point | null;
  /** Spread on the button: the pointer handler that starts a drag, which then listens for itself until the release. */
  handlers: Pick<React.ButtonHTMLAttributes<HTMLButtonElement>, "onPointerDown">;
  /**
   * Whether the click that follows a press was the end of a drag. The first
   * call answers and clears it, so a keyboard activation after a drag is
   * still a tap.
   */
  endsDrag: () => boolean;
}

/**
 * Carries the button under the pointer and docks it where it is dropped. A
 * press starts it; the moves and the release are heard on the window, so the
 * drag does not depend on the button receiving them. The pointer is captured
 * as well, so a drag that outruns the button does not land on the stream,
 * where the core would adopt the held button.
 * @param dock Where the button is docked now.
 * @param setDock Takes the dock a drag leaves it on.
 * @param viewport The window's size.
 * @param size The button's size.
 */
export function useDockDrag(dock: Dock, setDock: (dock: Dock) => void, viewport: Size, size: Size): DockDrag {
  const [free, setFree] = React.useState<Point | null>(null);
  const ended = React.useRef(false);
  /** Stops the drag in progress and removes its listeners; null at rest. */
  const stopListening = React.useRef<(() => void) | null>(null);
  React.useEffect(() => () => stopListening.current?.(), []);

  return {
    free,
    endsDrag: () => {
      const was = ended.current;
      ended.current = false;
      return was;
    },
    handlers: {
      onPointerDown: (event) => {
        if (event.button !== 0) return;
        stopListening.current?.();
        const button = event.currentTarget;
        const id = event.pointerId;
        try {
          button.setPointerCapture(id);
        } catch {
          // A pointer that is already gone has nothing to capture.
        }
        const start = { x: event.clientX, y: event.clientY };
        const origin = dockPoint(dock, viewport, size);
        let moved = false;
        let last: Point | null = null;

        const move = (e: PointerEvent) => {
          if (e.pointerId !== id) return;
          const dx = e.clientX - start.x;
          const dy = e.clientY - start.y;
          if (!moved && Math.hypot(dx, dy) < DRAG_SLOP) return;
          moved = true;
          last = {
            x: Math.max(0, Math.min(origin.x + dx, viewport.width - size.width)),
            y: Math.max(0, Math.min(origin.y + dy, viewport.height - size.height)),
          };
          setFree(last);
        };
        const stop = (e: PointerEvent) => {
          if (e.pointerId !== id) return;
          stopListening.current?.();
          if (!moved || !last) return;
          setDock(nearestDock(last, viewport, size));
          setFree(null);
          ended.current = true;
          // The click that follows this release arrives before the next task; a drag that gets none must not swallow a later tap.
          setTimeout(() => { ended.current = false; });
        };

        window.addEventListener("pointermove", move);
        window.addEventListener("pointerup", stop);
        window.addEventListener("pointercancel", stop);
        stopListening.current = () => {
          window.removeEventListener("pointermove", move);
          window.removeEventListener("pointerup", stop);
          window.removeEventListener("pointercancel", stop);
          try {
            if (button.hasPointerCapture(id)) button.releasePointerCapture(id);
          } catch {
            // The pointer is gone, and with it the capture.
          }
          stopListening.current = null;
        };
      },
    },
  };
}
