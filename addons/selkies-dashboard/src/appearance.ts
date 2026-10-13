/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The shell and the color mode in force on this page, shared by both
 * dashboards.
 *
 * Both are client settings resolved by `lib/appearance.js` against the
 * server's payload and the picks stored under `ui_shell` and `ui_mode`, like
 * any other setting the dashboards keep. The mode drawn is kept as
 * `ui_mode_effective` for the file browser, which mirrors it; a `system`
 * mode follows the browser's preference as it changes. A shell pick reloads
 * the page, so the two dashboards' stylesheets never share a document.
 *
 * Picks made before this page were kept under `theme` by the classic
 * dashboard and `vite-ui-theme` by the wish dashboard; the first page that
 * finds one adopts it as its stored mode and forgets the old key.
 * @module
 */

import { useSyncExternalStore } from "react";
import { effectiveMode, resolveAppearance, type Appearance } from "../../selkies-web-core/lib/appearance.js";
import { getPrefixedKey } from "./utils";

const listeners = new Set<() => void>();
let serverSettings: any = null;
let current: Appearance = resolve();

/** The stored pick of a setting, or null for none or no storage. */
function readStored(key: string): string | null {
  try {
    return window.localStorage.getItem(getPrefixedKey(key));
  } catch {
    return null;
  }
}

function writeStored(key: string, value: string): void {
  try {
    window.localStorage.setItem(getPrefixedKey(key), value);
  } catch {
    /* storage unavailable: the pick lives for the page */
  }
}

function resolve(): Appearance {
  return resolveAppearance(serverSettings, readStored);
}

/** Adopts a mode a previous dashboard stored under its own key, once. */
function migrateLegacyMode(): void {
  if (readStored("ui_mode") !== null) return;
  try {
    const legacy = localStorage.getItem("theme") || localStorage.getItem("vite-ui-theme");
    if (legacy === "dark" || legacy === "light" || legacy === "system") writeStored("ui_mode", legacy);
    localStorage.removeItem("theme");
    localStorage.removeItem("vite-ui-theme");
  } catch {
    /* storage unavailable */
  }
}

/** Re-resolves and tells the subscribers when something changed. */
function refresh(): void {
  const next = resolve();
  const changed = next.shell !== current.shell || next.mode !== current.mode
    || next.shells.join() !== current.shells.join() || next.modes.join() !== current.modes.join();
  if (changed) {
    current = next;
    for (const l of listeners) l();
  }
  writeStored("ui_mode_effective", effectiveMode(current.mode));
}

if (typeof window !== "undefined") {
  migrateLegacyMode();
  refresh();
  window.addEventListener("message", (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    if (event.data?.type === "serverSettings") {
      serverSettings = event.data.payload;
      refresh();
    }
  });
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (current.mode === "system") for (const l of listeners) l();
  });
}

/** The shell and mode in force. */
export function getAppearance(): Appearance {
  return current;
}

const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
};

/** The shell and mode in force, re-rendering as they change. */
export function useAppearance(): Appearance {
  return useSyncExternalStore(subscribe, getAppearance, getAppearance);
}

/** The mode drawn for the mode in force: `dark` or `light`. */
export function drawnMode(): "dark" | "light" {
  return effectiveMode(current.mode) as "dark" | "light";
}

/** Stores a shell pick and reloads into it. */
export function setShell(shell: string): void {
  if (shell === current.shell) return;
  writeStored("ui_shell", shell);
  window.location.reload();
}

/** Stores a mode pick and applies it. */
export function setMode(mode: string): void {
  writeStored("ui_mode", mode);
  refresh();
  for (const l of listeners) l();
}
