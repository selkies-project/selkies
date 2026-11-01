/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The system's on-screen keyboard, raised for the session: a hidden text
 * field (`#keyboard-input-assist`) takes the focus, and the input handler
 * relays what is typed into it. Both cores keep one such field and answer the
 * dashboard's `showVirtualKeyboard` with `showVirtualKeyboard` below.
 *
 * Focus alone raises the keyboard on a phone, but not everywhere: Chromium on
 * Windows leaves the touch keyboard down for a field focused by script from a
 * tap elsewhere, and always while a keyboard is attached (a Surface with its
 * cover on). Where the engine has the VirtualKeyboard API the field is set to
 * the manual policy and the keyboard asked for outright, which the user's tap
 * allows; with the manual policy the engine no longer hides it on blur, so
 * the field does.
 * @module
 */

/** The hidden field's id, which the input handler and the dashboards know it by. */
export const ASSIST_ID = 'keyboard-input-assist';

/**
 * The page's hidden field, added to the body the first time it is asked for.
 * @returns {HTMLInputElement} The field.
 */
export function keyboardAssist() {
  let field = /** @type {HTMLInputElement|null} */ (document.getElementById(ASSIST_ID));
  if (field) return field;
  field = document.createElement('input');
  field.type = 'search';
  field.id = ASSIST_ID;
  Object.assign(field.style, {
    position: 'absolute', left: '-9999px', top: '-9999px', width: '1px', height: '1px',
    opacity: '0', border: '0', padding: '0', caretColor: 'transparent',
  });
  field.setAttribute('aria-hidden', 'true');
  field.setAttribute('autocomplete', 'off');
  field.setAttribute('autocorrect', 'off');
  field.setAttribute('autocapitalize', 'off');
  field.setAttribute('spellcheck', 'false');
  document.body.appendChild(field);
  return field;
}

/**
 * Raises the on-screen keyboard; the next touch of `overlay` (the stream)
 * lowers it again.
 * @param {Element|null} overlay The stream's input overlay.
 * @returns {boolean} Whether the keyboard was asked for.
 */
export function showVirtualKeyboard(overlay) {
  const field = document.getElementById(ASSIST_ID);
  if (!field) return false;
  const vk = typeof navigator !== 'undefined' ? navigator.virtualKeyboard : undefined;
  const manual = !!vk && typeof vk.show === 'function';
  field.removeAttribute('aria-hidden');
  field.value = '';
  if (manual) field.setAttribute('virtualkeyboardpolicy', 'manual');
  field.focus({ preventScroll: true });
  if (manual) {
    try {
      vk.show();
    } catch (e) {
      console.warn('The on-screen keyboard was not shown:', e);
    }
  }
  field.addEventListener('blur', () => {
    field.setAttribute('aria-hidden', 'true');
    if (manual && typeof vk.hide === 'function') {
      try { vk.hide(); } catch (e) { /* already down */ }
    }
  }, { once: true });
  if (overlay) {
    overlay.addEventListener('touchstart', () => {
      if (document.activeElement === field) field.blur();
    }, { once: true, passive: true });
  }
  return true;
}
