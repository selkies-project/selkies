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
import { type Fraction, isFraction, place, useAnchorDrag, useElementSize, useViewport } from "@/panels/anchor";

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
  const { dragging, begin } = useAnchorDrag(setAnchor, viewport, size, MARGIN, (last) => {
    if (storageKey && last) localStorage.setItem(getPrefixedKey(storageKey), JSON.stringify(last));
  });

  const onPointerDown = (event: React.PointerEvent) => {
    const target = event.target as HTMLElement;
    if (!target.closest("[data-drag-handle]") || target.closest("button, input, select, a")) return;
    begin(event, position);
  };

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
