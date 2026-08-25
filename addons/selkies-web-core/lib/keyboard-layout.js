/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Best-effort detection of the client's physical keyboard layout for the
 * SETTINGS `keyboardLayout` hint, as xkb layout names.
 *
 * Chromium exposes `navigator.keyboard.getLayoutMap()`, and a handful of
 * probe keys identify the layout family (`KeyY` producing `z` is QWERTZ,
 * `KeyQ` producing `a` is AZERTY, ...). Everywhere else, and when the probes
 * are inconclusive, the BCP 47 `navigator.language` tag maps language and
 * region to a layout. Both resolve to `null` when unknown, and callers omit
 * the hint then.
 * @module
 */

/**
 * Language subtag to xkb layout, for languages whose dominant layout name
 * differs from (or matches) the subtag. Consulted after any region match.
 */
const LANGUAGE_LAYOUTS = {
	de: 'de', fr: 'fr', es: 'es', it: 'it', pt: 'pt', ru: 'ru', pl: 'pl',
	cs: 'cz', sk: 'sk', hu: 'hu', tr: 'tr', da: 'dk', sv: 'se', nb: 'no',
	nn: 'no', no: 'no', fi: 'fi', nl: 'nl', ja: 'jp', ko: 'kr', el: 'gr',
	he: 'il', uk: 'ua', en: 'us',
};

/**
 * Region subtag to xkb layout, where the region picks a distinct national
 * layout regardless of the language subtag (en-GB, pt-BR, fr-CH, ...).
 */
const REGION_LAYOUTS = {
	GB: 'gb', BR: 'br', CH: 'ch', BE: 'be',
};

/**
 * Maps a BCP 47 language tag to an xkb layout name.
 * @param {string} lang A tag such as `en-GB` or `de`.
 * @returns {string|null} The layout, or `null` when the tag names none.
 */
export function layoutFromLanguage(lang) {
	if (!lang || typeof lang !== 'string') return null;
	const [base, region] = lang.split('-');
	if (region) {
		const byRegion = REGION_LAYOUTS[region.toUpperCase()];
		if (byRegion) return byRegion;
	}
	return LANGUAGE_LAYOUTS[base.toLowerCase()] || null;
}

/**
 * Detects the physical layout through the keyboard layout map where the
 * engine exposes one, falling back to the language tag.
 * @returns {Promise<string|null>} The xkb layout name, or `null` when unknown.
 */
export async function detectKeyboardLayout() {
	try {
		const kb = navigator.keyboard;
		if (kb && typeof kb.getLayoutMap === 'function') {
			const map = await kb.getLayoutMap();
			if (map && map.size) {
				const key = (code) => (map.get(code) || '').toLowerCase();
				if (key('KeyY') === 'z' && key('KeyZ') === 'y') {
					// Swiss keeps QWERTZ but drops the German ß.
					return key('Minus') === 'ß' ? 'de'
						: (layoutFromLanguage(navigator.language) || 'de');
				}
				if (key('KeyQ') === 'a' && key('KeyA') === 'q') return 'fr';
				if (key('Semicolon') === 'ñ') return 'es';
				if (key('Semicolon') === 'ò') return 'it';
				if (key('KeyY') === 'y' && key('KeyQ') === 'q') {
					// The UK ISO layout puts '#' on the Backslash code; national
					// QWERTY punctuation (Nordics etc.) is left to the language tag.
					if (key('Backslash') === '#') return 'gb';
					if (key('Semicolon') === ';') return 'us';
				}
			}
		}
	} catch (_) { /* best-effort probe */ }
	return layoutFromLanguage(navigator.language);
}
