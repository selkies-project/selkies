/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A panel that floats over the stream and is dragged by any element inside a
 * `data-drag-handle` of its own. Its place is a fraction of the room the
 * window leaves it, a margin in from the edges, so a resize keeps it in
 * proportion and on screen; the fraction is kept under a prefixed key when
 * one is named.
 * @module
 */
import * as React from "react";
import { getPrefixedKey } from "@/utils";
import { type Fraction, fractionOf, isFraction, place, useElementSize, useViewport } from "@/panels/anchor";

const MARGIN = 16;

interface FloatingPanelProps {
  /** Where the panel starts, as a fraction of the room the window leaves it: `0` an edge, `1` the opposite one. */
  initial: Fraction;
  /** When given, the place is kept under this prefixed key and restored on the next load. */
  storageKey?: string;
  className?: string;
  children: React.ReactNode;
}

/** Renders `children` at a draggable place, scrolling within what the window leaves below its top. */
export function FloatingPanel({ initial, storageKey, className, children }: FloatingPanelProps) {
  const ref = React.useRef<HTMLDivElement>(null);
  const [anchor, setAnchor] = React.useState<Fraction>(() => {
    if (!storageKey) return initial;
    try {
      const saved: unknown = JSON.parse(localStorage.getItem(getPrefixedKey(storageKey)) ?? "null");
      if (isFraction(saved)) return saved;
    } catch { /* unreadable: the default stands */ }
    return initial;
  });
  const viewport = useViewport();
  const size = useElementSize(ref, { width: 300, height: 200 });
  const position = place(anchor, viewport, size, MARGIN);
  const [dragging, setDragging] = React.useState(false);
  const start = React.useRef({ x: 0, y: 0 });
  const last = React.useRef<Fraction | null>(null);

  const onPointerDown = (event: React.PointerEvent) => {
    const target = event.target as HTMLElement;
    if (!target.closest("[data-drag-handle]") || target.closest("button, input, select, a")) return;
    // Captured: a pointer that outruns the panel lands on the stream, where the core adopts the held button.
    event.currentTarget.setPointerCapture(event.pointerId);
    setDragging(true);
    start.current = { x: event.clientX - position.x, y: event.clientY - position.y };
  };

  React.useEffect(() => {
    if (!dragging) return undefined;
    const move = (event: PointerEvent) => {
      const next = fractionOf({ x: event.clientX - start.current.x, y: event.clientY - start.current.y }, viewport, size, MARGIN);
      last.current = next;
      setAnchor(next);
    };
    const up = () => {
      setDragging(false);
      if (storageKey && last.current) localStorage.setItem(getPrefixedKey(storageKey), JSON.stringify(last.current));
    };
    document.addEventListener("pointermove", move);
    document.addEventListener("pointerup", up);
    return () => {
      document.removeEventListener("pointermove", move);
      document.removeEventListener("pointerup", up);
    };
  }, [dragging, storageKey, viewport, size]);

  // The entrance animation keeps the kit's default length: a duration utility
  // would also set a transition on every property, and a drag would then
  // trail the pointer by that long.
  return (
    <div ref={ref} onPointerDown={onPointerDown} className={`fixed z-[2050] flex flex-col animate-in fade-in zoom-in-95 ${className ?? ""}`}
      style={{ left: position.x, top: position.y, maxHeight: `calc(100dvh - ${position.y}px - 1rem)`, cursor: dragging ? "grabbing" : undefined }}>
      {children}
    </div>
  );
}
