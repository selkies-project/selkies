/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Which dashboard a page shows and in which color mode, resolved from two
 * client settings the server publishes like any other: the shell
 * (`ui_shell`, the dashboard itself) and the color mode (`ui_mode`).
 *
 * Each resolves through one ladder: a locked server value, else the single
 * value the operator left, else the client's stored pick, else the server's
 * value. An operator who narrows a setting to one value locks it, and the
 * dashboards hide the control then, as they do for any other setting.
 * @module
 */

/** The shells the dashboard ships, in the order a menu lists them. */
export const SHELLS = ['classic', 'wish'];
/** The color modes, in the order a menu lists them. */
export const MODES = ['dark', 'light', 'system'];
/** The shell a session starts on when nothing names one. */
export const DEFAULT_SHELL = 'classic';
/** The color mode a session starts on when nothing names one. */
export const DEFAULT_MODE = 'dark';

/**
 * The values a payload entry offers: its `allowed` list, else every value.
 * @param {({allowed?: string[]}|undefined)} entry The setting's payload entry.
 * @param {string[]} all Every value the setting takes.
 * @returns {string[]}
 */
export function allowedOf(entry, all) {
  const allowed = entry && Array.isArray(entry.allowed) ? entry.allowed.filter((v) => all.includes(v)) : [];
  return allowed.length ? allowed : all;
}

/**
 * Resolves one choice: a locked server value, else the single value the
 * operator left, else the client's stored pick, else the server's value,
 * else `fallback`.
 * @param {({value?: string, allowed?: string[], locked?: boolean}|undefined)} entry
 *     The setting's payload entry.
 * @param {(string|null|undefined)} stored The client's stored pick.
 * @param {string[]} all Every value the setting takes.
 * @param {string} fallback What stands in without a server entry.
 * @returns {string}
 */
export function resolveChoice(entry, stored, all, fallback) {
  const allowed = allowedOf(entry, all);
  if (entry && entry.locked && allowed.includes(entry.value)) return entry.value;
  if (allowed.length === 1) return allowed[0];
  if (stored && allowed.includes(stored)) return stored;
  if (entry && allowed.includes(entry.value)) return entry.value;
  return allowed.includes(fallback) ? fallback : allowed[0];
}

/**
 * @typedef {object} Appearance
 * @property {string} shell The shell in force.
 * @property {string} mode The color mode in force, possibly `system`.
 * @property {string[]} shells The shells offered.
 * @property {string[]} modes The modes offered.
 */

/**
 * Resolves both choices against the server's payload and the client's
 * stored picks.
 * @param {object|null} serverSettings The `serverSettings` payload, or null before one arrives.
 * @param {(key: string) => (string|null)} readStored Reads a stored setting by name.
 * @returns {Appearance}
 */
export function resolveAppearance(serverSettings, readStored) {
  const s = serverSettings || {};
  return {
    shell: resolveChoice(s.ui_shell, readStored('ui_shell'), SHELLS, DEFAULT_SHELL),
    mode: resolveChoice(s.ui_mode, readStored('ui_mode'), MODES, DEFAULT_MODE),
    shells: allowedOf(s.ui_shell, SHELLS),
    modes: allowedOf(s.ui_mode, MODES),
  };
}

/**
 * The mode drawn for a choice: `system` follows the browser's preference.
 * @param {string} mode `dark`, `light`, or `system`.
 * @returns {string} `dark` or `light`.
 */
export function effectiveMode(mode) {
  if (mode !== 'system') return mode;
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}
