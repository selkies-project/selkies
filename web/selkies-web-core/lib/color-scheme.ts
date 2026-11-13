/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The light or dark color scheme a page asks the session's desktop to
 * follow, the SETTINGS `color_scheme` both cores send.
 *
 * The scheme is the browser's, read from the `prefers-color-scheme` media
 * query and followed live through its `change` event, unless a dashboard
 * names one of its own: a dashboard whose user picked a theme posts
 * `setColorScheme` with `light` or `dark`, and with `null` to hand the choice
 * back to the browser. The message is taken here, at module load, rather than
 * in a core's own handler, so a dashboard that posts while its page is still
 * starting is heard whichever core then runs. Listeners hear the scheme only
 * when it changes.
 * @module
 */

/** A scheme the desktop is asked to follow. */
export type ColorScheme = 'light' | 'dark';

/** The browser's dark preference, where the engine evaluates media queries. */
const darkQuery: MediaQueryList | null =
  typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null;

/** The dashboard's pick, `null` while the browser's preference stands. */
let picked: ColorScheme | null = null;
const listeners = new Set<(scheme: ColorScheme) => void>();

/**
 * The scheme the desktop is asked to follow now.
 * @returns The dashboard's pick, else the browser's preference.
 */
export function currentColorScheme(): ColorScheme {
  return picked ?? (darkQuery?.matches ? 'dark' : 'light');
}

let heard: ColorScheme = currentColorScheme();

/** Tells every listener the scheme, if it moved since they last heard it. */
function announce(): void {
  const scheme = currentColorScheme();
  if (scheme === heard) return;
  heard = scheme;
  for (const listener of listeners) listener(scheme);
}

/**
 * Registers a listener for every change of the scheme.
 * @param listener Called with the new scheme.
 * @returns A function that removes the listener.
 */
export function onColorSchemeChange(listener: (scheme: ColorScheme) => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

darkQuery?.addEventListener('change', announce);

/** Takes a `setColorScheme` message from this page's own origin. */
const onSchemeMessage = (event: MessageEvent<{ type?: unknown; value?: unknown } | null>): void => {
  if (event.origin !== window.location.origin) return;
  const message = event.data;
  if (typeof message !== 'object' || message === null || message.type !== 'setColorScheme') return;
  if (message.value === 'light' || message.value === 'dark') {
    picked = message.value;
  } else if (message.value === null) {
    picked = null;
  } else {
    console.warn('Ignoring setColorScheme with a value that names no scheme:', message.value);
    return;
  }
  announce();
};

if (typeof window !== 'undefined') {
  window.addEventListener('message', onSchemeMessage);
}
