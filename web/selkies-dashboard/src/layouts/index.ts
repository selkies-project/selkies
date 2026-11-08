/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The layouts: how the one set of panels is arranged over the stream. A
 * layout is a component that decides where each panel is painted and the
 * stats it lays over the stream; the panels themselves know nothing of it.
 * What a layout places is decided for all of them: the panels and their
 * order by `panels/catalog.ts` (`PANELS`, each with its gate and body), the
 * other controls by `useActions` and the events its chrome answers by
 * `useChromeEvents` (`./actions`). Its look is its own stylesheet plus the
 * size tokens `index.css` names on `:root` (`--control-h`, `--stack-gap` and
 * the rest), which it may set again on its root.
 *
 * Adding a layout is a component over those, one entry here, its value in the
 * server's `ui_layout` enum (`crates/selkies/src/settings/table.rs`), and its
 * name in the dictionary (`layout.<id>`).
 *
 * Which layout a page shows: the pick stored under the prefixed `ui_layout`
 * key, else what the server names in `ui_layout` (its `/api/status` probe
 * before the stream connects, its settings payload after), else the first
 * entry. A single allowed value locks it and hides the toggle.
 *
 * Every layout is imported statically, never split into a lazy chunk. Every
 * page paints a layout at once, so splitting saves nothing, and a split chunk
 * imports the entry back: under a page URL carrying `user:pass@`, Chrome
 * evaluates that entry a second time, which starts a second core that evicts
 * the first as the same tab, and a failed chunk fetch unmounts the dashboard.
 * @module
 */
import * as React from "react";
import { Columns2, PanelTop } from "lucide-react";
import { FloatingStats } from "@/panels/stats";
import { StreamStrip } from "@/panels/stats-section";
import { getPrefixedKey } from "@/utils";
import SidebarLayout from "./sidebar";
import ToolbarLayout from "./toolbar";

/** One layout of the registry. */
export interface LayoutSpec {
  /** The value `ui_layout` names it by. */
  id: string;
  /** The dictionary key of its name. */
  labelKey: string;
  /** The mark the layout toggle draws for it. */
  icon: React.ComponentType<{ className?: string }>;
  /** The component that arranges the panels. */
  component: React.ComponentType;
  /** What the stats switch lays over the stream. */
  stats: React.ComponentType;
}

/** Every layout, in the order the toggle cycles them. */
export const LAYOUTS: LayoutSpec[] = [
  {
    id: "default",
    labelKey: "layout.default",
    icon: Columns2,
    component: SidebarLayout,
    stats: StreamStrip,
  },
  {
    id: "wish",
    labelKey: "layout.wish",
    icon: PanelTop,
    component: ToolbarLayout,
    stats: FloatingStats,
  },
];

const STORAGE_KEY = "ui_layout";

/** The spec of `id`, else the first layout. */
export function layoutById(id: string | null | undefined): LayoutSpec {
  return LAYOUTS.find((layout) => layout.id === id) ?? LAYOUTS[0];
}

/**
 * The layout the page shows: the stored pick where the server allows it,
 * else the server's value, else the first layout.
 * @param serverValue What the server names, or null before it has.
 * @param allowed The values the server permits, or null for every one.
 */
export function resolveLayout(serverValue: string | null, allowed: string[] | null): LayoutSpec {
  const permitted = (id: string) => !allowed || allowed.includes(id);
  const stored = localStorage.getItem(getPrefixedKey(STORAGE_KEY));
  if (stored && permitted(stored) && LAYOUTS.some((layout) => layout.id === stored)) return layoutById(stored);
  if (serverValue && permitted(serverValue)) return layoutById(serverValue);
  return layoutById(allowed?.[0]);
}

/** Stores a pick, which outranks the server's default on this browser. */
export function storeLayout(id: string): void {
  localStorage.setItem(getPrefixedKey(STORAGE_KEY), id);
}
