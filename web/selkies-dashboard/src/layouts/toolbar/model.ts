/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the toolbar layout decides apart from its markup: where the bar is,
 * how a drag moves it, how it slides up out of sight, and where the settings
 * card hangs from it.
 *
 * The bar's place is kept as a fraction of the room the window leaves it
 * (`panels/anchor.ts`), centered at the top until it is dragged by its handle,
 * so a resize, or a group of buttons joining the bar, keeps it in proportion
 * rather than letting it run off the edge. The place is not kept between
 * visits. Hiding the bar slides it up by its measured height plus where it
 * sits, which hides it exactly wherever it was dragged to, and leaves a tab
 * that brings it back.
 * @module
 */
import * as React from "react";
import { place, useAnchorDrag, useElementSize, useViewport, type Fraction, type Point, type Size } from "@/panels/anchor";

/** The bar's height in pixels before it is measured. */
const BAR_HEIGHT = 50;

/** The bar's width in pixels before it is measured. */
const BAR_WIDTH = 400;

/** Pixels between the bar and the settings card hanging from it. */
const POPUP_GAP = 6;

/** Where the bar starts: centered, against the top edge. */
const START: Fraction = { x: 0.5, y: 0 };

/** Where the bar is, and the means to move it. */
export interface BarPlacement {
  /** The bar's top left in pixels. */
  position: Point;
  /** The bar's measured size. */
  size: Size;
  /** Starts a drag from a pointer down on the bar's handle. */
  begin: (event: React.PointerEvent) => void;
}

/**
 * The bar's place, measured and draggable.
 * @param barRef The bar's element, whose size is measured.
 * @returns Where the bar is, its size, and what starts a drag of it.
 */
export function usePlacement(barRef: React.RefObject<HTMLElement | null>): BarPlacement {
  const [anchor, setAnchor] = React.useState<Fraction>(START);
  const viewport = useViewport();
  const size = useElementSize(barRef, { width: BAR_WIDTH, height: BAR_HEIGHT });
  const position = place(anchor, viewport, size);
  const drag = useAnchorDrag(setAnchor, viewport, size);
  return { position, size, begin: (event) => drag.begin(event, position) };
}

/**
 * The style that puts the bar where it is, or slides it up out of sight.
 * @param placement Where the bar is.
 * @param collapsed The bar is hidden, leaving its tab.
 */
export function barStyle({ position, size }: BarPlacement, collapsed: boolean): React.CSSProperties {
  return {
    transform: `translate(${position.x}px, ${position.y}px)`,
    top: collapsed ? -(size.height + position.y) : 0,
    transition: "top 300ms",
  };
}

/**
 * The style that hangs the settings card under the bar, at its left.
 * @param placement Where the bar is.
 */
export function popupStyle({ position, size }: BarPlacement): React.CSSProperties {
  return { left: position.x, top: position.y + size.height + POPUP_GAP };
}
