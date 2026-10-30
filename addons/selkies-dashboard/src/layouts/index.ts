/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The layouts: how the one set of panels is arranged over the stream. A
 * layout is a component that reads `useDashboard` and `useSettings` and
 * decides where each panel is painted; the panels themselves know nothing of
 * it. Adding a layout is one entry here, the server's `ui_layout` enum, and
 * its name in the dictionary (`layout.<id>`).
 *
 * Which layout a page shows: the pick stored under the prefixed `ui_layout`
 * key, else what the server names in `ui_layout` (its `/api/status` probe
 * before the stream connects, its settings payload after), else the first
 * entry. A single allowed value locks it and hides the toggle.
 * @module
 */
import * as React from "react";
import { Columns2, PanelTop } from "lucide-react";
import { getPrefixedKey } from "@/utils";

/** One layout of the registry. */
export interface LayoutSpec {
  /** The value `ui_layout` names it by. */
  id: string;
  /** The dictionary key of its name. */
  labelKey: string;
  /** The mark the layout toggle draws for it. */
  icon: React.ComponentType<{ className?: string }>;
  /** The component that arranges the panels. */
  component: React.LazyExoticComponent<React.ComponentType>;
}

/** Every layout, in the order the toggle cycles them. */
export const LAYOUTS: LayoutSpec[] = [
  {
    id: "default",
    labelKey: "layout.default",
    icon: Columns2,
    component: React.lazy(() => import("./sidebar")),
  },
  {
    id: "wish",
    labelKey: "layout.wish",
    icon: PanelTop,
    component: React.lazy(() => import("./toolbar")),
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
