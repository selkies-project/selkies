/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The arrows that ask which side a second display goes, drawn over the whole
 * page while `placements` holds the sides; a click outside them dismisses it.
 * @module
 */
import { Button } from "@/components/ui/button";
import { useDashboard } from "@/state/dashboard";

const SIDES: Array<{ side: "up" | "down" | "left" | "right"; glyph: string; className: string }> = [
  { side: "up", glyph: "▲", className: "top-10 left-1/2 -translate-x-1/2" },
  { side: "down", glyph: "▼", className: "bottom-10 left-1/2 -translate-x-1/2" },
  { side: "left", glyph: "◄", className: "left-10 top-1/2 -translate-y-1/2" },
  { side: "right", glyph: "►", className: "right-10 top-1/2 -translate-y-1/2" },
];

/** Renders the arrows, or nothing while no placement is asked. */
export function ScreenPlacement() {
  const { placements, launchWindow, dismissPlacements } = useDashboard();
  if (!placements) return null;
  return (
    <div className="screen-placement-overlay fixed inset-0 z-[2150]" onClick={dismissPlacements}>
      {SIDES.filter(({ side }) => placements[side] !== undefined).map(({ side, glyph, className }) => (
        <Button key={side} className={`absolute size-24 text-4xl ${className}`}
          onClick={(event) => { event.stopPropagation(); launchWindow(side, placements[side]); }}>
          {glyph}
        </Button>
      ))}
    </div>
  );
}
