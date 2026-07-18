/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Best-effort detection of the client's physical keyboard layout for the
// SETTINGS `keyboardLayout` hint (xkb layout names). Chromium exposes
// navigator.keyboard.getLayoutMap(); a handful of probe keys identify the
// layout family (KeyY->'z' is QWERTZ/de, KeyQ->'a' is AZERTY/fr, ...).
// Everywhere else — and when the probes are inconclusive — the BCP 47
// navigator.language tag maps language/region to a layout. Resolves to null
// when unknown; callers omit the field then.

// Language subtag -> xkb layout, for languages whose dominant layout name
// differs from (or matches) the subtag. Consulted after any region match.
const LANGUAGE_LAYOUTS = {
	de: 'de', fr: 'fr', es: 'es', it: 'it', pt: 'pt', ru: 'ru', pl: 'pl',
	cs: 'cz', sk: 'sk', hu: 'hu', tr: 'tr', da: 'dk', sv: 'se', nb: 'no',
	nn: 'no', no: 'no', fi: 'fi', nl: 'nl', ja: 'jp', ko: 'kr', el: 'gr',
	he: 'il', uk: 'ua', en: 'us',
};

// Region subtag -> xkb layout, where the region picks a distinct national
// layout regardless of the language subtag (en-GB, pt-BR, fr-CH, ...).
const REGION_LAYOUTS = {
	GB: 'gb', BR: 'br', CH: 'ch', BE: 'be',
};

export function layoutFromLanguage(lang) {
	if (!lang || typeof lang !== 'string') return null;
	const [base, region] = lang.split('-');
	if (region) {
		const byRegion = REGION_LAYOUTS[region.toUpperCase()];
		if (byRegion) return byRegion;
	}
	return LANGUAGE_LAYOUTS[base.toLowerCase()] || null;
}

export async function detectKeyboardLayout() {
	try {
		const kb = navigator.keyboard;
		if (kb && typeof kb.getLayoutMap === 'function') {
			const map = await kb.getLayoutMap();
			if (map && map.size) {
				const key = (code) => (map.get(code) || '').toLowerCase();
				if (key('KeyY') === 'z' && key('KeyZ') === 'y') {
					// QWERTZ family; Swiss keeps QWERTZ but drops the German ß.
					return key('Minus') === 'ß' ? 'de'
						: (layoutFromLanguage(navigator.language) || 'de');
				}
				if (key('KeyQ') === 'a' && key('KeyA') === 'q') return 'fr';
				if (key('Semicolon') === 'ñ') return 'es';
				if (key('Semicolon') === 'ò') return 'it';
				if (key('KeyY') === 'y' && key('KeyQ') === 'q') {
					// QWERTY: the UK ISO layout puts '#' on the Backslash code.
					if (key('Backslash') === '#') return 'gb';
					if (key('Semicolon') === ';') return 'us';
					// National QWERTY punctuation (Nordics etc.): the language
					// tag disambiguates better than more probes would.
				}
			}
		}
	} catch (_) { /* the probe is best-effort; fall through to the language */ }
	return layoutFromLanguage(navigator.language);
}
