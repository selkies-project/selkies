/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Where a floating element sits, kept as a fraction of the room the window
 * leaves it rather than as pixels: a resize then places it in proportion, so
 * a centered bar stays centered, a handle at the bottom edge stays there, and
 * nothing is left off the canvas by a window that shrank or a bar that grew.
 * @module
 */
import * as React from "react";

/** A place in the room the window leaves an element, each axis 0 to 1. */
export interface Fraction { x: number; y: number }
export interface Size { width: number; height: number }
export interface Point { x: number; y: number }

const clamp01 = (value: number) => Math.max(0, Math.min(1, value));

/** The window's inner size, read again on every resize. */
export function useViewport(): Size {
  const read = () => ({ width: window.innerWidth, height: window.innerHeight });
  const [viewport, setViewport] = React.useState(read);
  React.useEffect(() => {
    const onResize = () => setViewport(read());
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return viewport;
}

/**
 * The element's rendered size, `fallback` until it is measured, and again
 * whenever its content changes it.
 * @param ref The element to measure.
 * @param fallback The size assumed before the first measurement.
 */
export function useElementSize(ref: React.RefObject<HTMLElement | null>, fallback: Size): Size {
  const [size, setSize] = React.useState(fallback);
  React.useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return undefined;
    const measure = () => setSize({ width: el.offsetWidth, height: el.offsetHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return size;
}

/**
 * The pixel offset of `fraction` in the room `viewport` leaves an element of
 * `size`, `margin` pixels in from each edge where the room allows it.
 */
export function place(fraction: Fraction, viewport: Size, size: Size, margin = 0): Point {
  const roomX = Math.max(0, viewport.width - size.width - 2 * margin);
  const roomY = Math.max(0, viewport.height - size.height - 2 * margin);
  return {
    x: Math.max(0, Math.min(margin, viewport.width - size.width)) + clamp01(fraction.x) * roomX,
    y: Math.max(0, Math.min(margin, viewport.height - size.height)) + clamp01(fraction.y) * roomY,
  };
}

/** The fraction a pixel offset is of that room, clamped into it. */
export function fractionOf(point: Point, viewport: Size, size: Size, margin = 0): Fraction {
  const roomX = viewport.width - size.width - 2 * margin;
  const roomY = viewport.height - size.height - 2 * margin;
  return {
    x: roomX > 0 ? clamp01((point.x - margin) / roomX) : 0,
    y: roomY > 0 ? clamp01((point.y - margin) / roomY) : 0,
  };
}

/** Whether `value` is a stored fraction, both axes within 0 to 1. */
export function isFraction(value: unknown): value is Fraction {
  const v = value as Fraction | null;
  return !!v && Number.isFinite(v.x) && Number.isFinite(v.y) && v.x >= 0 && v.x <= 1 && v.y >= 0 && v.y <= 1;
}
