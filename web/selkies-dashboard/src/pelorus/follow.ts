/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A log that keeps its newest line in view until the person scrolls up to
 * read, used by the Pelorus page and the dashboard's panel alike.
 *
 * The log follows its end whenever its content grows, not only when an event
 * arrives: a screenshot decodes and takes its height a moment after the event
 * that carried it, and a log that scrolled on events alone stopped halfway
 * through the newest message. Growth is watched with a `ResizeObserver` on
 * the content.
 *
 * Only the person's own scrolling stops the following: a wheel or touch
 * moving up, a key that scrolls, or a press on the scroll bar, followed by a
 * scroll that leaves the end. The browser scrolls a box itself too (its
 * scroll anchoring moves the view as pictures above the end take their
 * height), and a rule that read every scroll away from the end as the person
 * leaving it stopped following on its own. Anchoring is turned off on the
 * box all the same (`overflow-anchor: none`), since this box decides where it
 * scrolls. Scrolling back to the end follows again.
 * @module
 */
import * as React from "react";

/** How near the end, in pixels, counts as at it. */
const NEAR_END = 48;

/** How long after a gesture of the person's a scroll is taken as theirs, in milliseconds. */
const GESTURE_MS = 800;

/** The keys that scroll a focused box. */
const SCROLL_KEYS = new Set(["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "]);

/** What a log binds: its scrolling box, its content, and a way to follow the end again. */
export interface FollowEnd {
  /** The box that scrolls. */
  scroller: React.RefObject<HTMLDivElement | null>;
  /** The content inside it, whose growth is followed. */
  content: React.RefObject<HTMLDivElement | null>;
  /** Follows the end again and goes to it, as when the person sends a task. */
  stick: () => void;
}

/**
 * Keeps a log at its end until the person scrolls away from it.
 * @param changed A value that changes with the log's content, so new content is followed at once.
 * @returns The refs to bind and the way to follow again.
 */
export function useFollowEnd(changed: unknown): FollowEnd {
  const scroller = React.useRef<HTMLDivElement>(null);
  const content = React.useRef<HTMLDivElement>(null);
  const following = React.useRef(true);

  const toEnd = React.useCallback(() => {
    const box = scroller.current;
    if (box && following.current) box.scrollTop = box.scrollHeight;
  }, []);

  React.useEffect(() => {
    const box = scroller.current;
    const inner = content.current;
    if (!box || !inner) return;
    box.style.overflowAnchor = "none";
    let gesture = 0;
    const touched = () => { gesture = performance.now(); };
    const onWheel = (event: WheelEvent) => { if (event.deltaY < 0) touched(); };
    const onKey = (event: KeyboardEvent) => { if (SCROLL_KEYS.has(event.key)) touched(); };
    const onPointer = (event: PointerEvent) => {
      // A press on the scroll bar lands on the box itself, outside its content.
      if (event.target === box && event.offsetX >= box.clientWidth) touched();
    };
    const onScroll = () => {
      const fromEnd = box.scrollHeight - box.scrollTop - box.clientHeight;
      if (fromEnd <= NEAR_END) following.current = true;
      else if (performance.now() - gesture < GESTURE_MS) following.current = false;
    };
    box.addEventListener("wheel", onWheel, { passive: true });
    box.addEventListener("touchmove", touched, { passive: true });
    box.addEventListener("keydown", onKey);
    box.addEventListener("pointerdown", onPointer);
    box.addEventListener("scroll", onScroll, { passive: true });
    const observer = new ResizeObserver(toEnd);
    observer.observe(inner);
    observer.observe(box);
    return () => {
      observer.disconnect();
      box.removeEventListener("wheel", onWheel);
      box.removeEventListener("touchmove", touched);
      box.removeEventListener("keydown", onKey);
      box.removeEventListener("pointerdown", onPointer);
      box.removeEventListener("scroll", onScroll);
    };
  }, [toEnd]);

  React.useLayoutEffect(toEnd, [changed, toEnd]);

  const stick = React.useCallback(() => {
    following.current = true;
    toEnd();
  }, [toEnd]);
  return { scroller, content, stick };
}
