/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Selkies-specific utilities shared by the dashboard components, kept apart
 * from `lib/utils.ts`, which shadcn manages.
 *
 * Storage keys: every localStorage key is prefixed with the storage namespace
 * the streaming cores derive from the page URL (`lib/util.js`), and the
 * per-display settings gain a `_display2` suffix on the secondary display,
 * selected by the `#display2` URL hash (`lib/storage-keys.js`, which the
 * cores read too). The route prefix is the cores' derivation as well, so
 * both dashboards and both cores agree on all of them. Which controls the
 * server's settings leave renderable is `lib/renderable-settings.js`.
 *
 * Core state cache: the core broadcasts `serverSettings` once per connection
 * and `clipboardContentUpdate`, `effectiveCursorState`, and
 * `audioDeviceSelected` only when something changes, but the panel components
 * (Settings, Sharing, Files, Clipboard) mount lazily when their menu opens,
 * after those messages. The latest of each is cached at module scope so a
 * late mount can seed its state synchronously; the panels' own listeners
 * still pick up later messages (reconnects, new clipboard events).
 * @module
 */

import { getRoutePrefix, getStorageAppName, isMobileClient } from "../../selkies-web-core/lib/util.js";
import { urlFragmentKeyword } from "../../selkies-web-core/lib/page-url.js";
import { hardwareKeyboard } from "../../selkies-web-core/lib/hardware-keyboard.js";
import { settingStorageKey } from "../../selkies-web-core/lib/storage-keys.js";

export { computeRenderableSettings, isSettingRenderable } from "../../selkies-web-core/lib/renderable-settings.js";

export { isMobileClient };
export { hardwareKeyboard };

export { getRoutePrefix, getStorageAppName };

const urlHash = typeof window !== 'undefined' ? urlFragmentKeyword() : '';
/** Which display this page is, from the `#display2` URL hash. */
export const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';
export const isSecondaryDisplay = displayId === 'display2';

/**
 * Viewer-designated URL modes, known before the server answers with a role.
 * The control UI keys off this for its first render so a shared viewer never
 * sees controls it cannot use in the gap before `clientRoleUpdate` arrives.
 */
export const isViewerUrlMode =
  urlHash.toLowerCase().startsWith('#shared') || /^#player[234]$/.test(urlHash.toLowerCase());

const storageAppName = getStorageAppName();

/** The localStorage key for a setting on this display. */
export function getPrefixedKey(key: string): string {
  return settingStorageKey(storageAppName, displayId, key);
}

/** A document printed in the session, as the core hands it over: a blob URL. */
export interface PrintJob { name: string; url: string }

let lastServerSettings: any = null;
/** The server clipboard preview; a secret arrives as the flag alone, never its text. */
export interface ClipboardPreview { text: string; truncated: boolean; secret: boolean }

let lastClipboardContent: ClipboardPreview | null = null;
let printJobs: PrintJob[] = [];
let lastEffectiveCursorState: boolean | null = null;
const lastAudioDevices: { input: string | null; output: string | null } = { input: null, output: null };
if (typeof window !== 'undefined') {
  window.addEventListener('message', (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    const message = event.data;
    if (typeof message !== 'object' || message === null) return;
    if (message.type === 'serverSettings') {
      lastServerSettings = message.payload;
    } else if (message.type === 'clipboardContentUpdate' && typeof message.text === 'string') {
      lastClipboardContent = { text: message.text, truncated: message.truncated === true,
        secret: message.secret === true };
    } else if (message.type === 'effectiveCursorState' && typeof message.value === 'boolean') {
      lastEffectiveCursorState = message.value;
    } else if (message.type === 'audioDeviceSelected' && message.deviceId) {
      if (message.context === 'input') lastAudioDevices.input = message.deviceId;
      else if (message.context === 'output') lastAudioDevices.output = message.deviceId;
    } else if (message.type === 'printDocument' && typeof message.url === 'string') {
      printJobs = [...printJobs, { name: message.name, url: message.url }];
      window.dispatchEvent(new Event('printJobsChanged'));
    }
  });
}

/** The documents printed so far this page; `printJobsChanged` fires on each. */
export function getPrintJobs(): PrintJob[] {
  return printJobs;
}

/** The last `serverSettings` payload, or null before the first connection. */
export function getLastServerSettings(): any {
  return lastServerSettings;
}

/** The last server clipboard preview; the core emits it only on clipboard events. */
export function getLastClipboardContent(): ClipboardPreview | null {
  return lastClipboardContent;
}

/**
 * The cursor mode actually in effect (multi-monitor forces browser cursors
 * on), emitted at connect and display-config time.
 */
export function getLastEffectiveCursorState(): boolean | null {
  return lastEffectiveCursorState;
}

/**
 * The dashboard's own audio device picks, which the core keeps for the life
 * of the page; a remounted Settings panel shows them again instead of
 * pretending the defaults are in use.
 */
export function getLastAudioDevices(): { input: string | null; output: string | null } {
  return lastAudioDevices;
}
