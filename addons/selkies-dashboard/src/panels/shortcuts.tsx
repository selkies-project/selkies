/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { Fragment, useState } from "react";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { SwitchRow } from "@/panels/controls";
import { computeRenderableSettings, getLastServerSettings, getPrefixedKey } from "@/utils";
import { t } from "@/i18n";

/**
 * The keyboard shortcuts: the core-owned chords as
 * dense rows of keys, the switch that hands them to the session instead, and
 * the citation link.
 * @module
 */

const shortcuts = [
	{ label: t('sections.shortcuts.fullscreen'), combo: "Ctrl + Shift + F" },
	{ label: t('sections.shortcuts.gamingMode'), combo: "Ctrl + Shift + X" },
	{ label: t('sections.shortcuts.openMenu'), combo: "Ctrl + Shift + M" },
	{ label: t('sections.shortcuts.toggleGamepad'), combo: "Ctrl + Shift + G" },
	{ label: t('sections.shortcuts.pointerLock'), combo: "Ctrl + Shift + Left Click" },
];

/** Renders the switch and the shortcut list. */
export function ShortcutsMenu() {
	const renderableSettings: any = computeRenderableSettings(getLastServerSettings());
	const [enabled, setEnabled] = useState(() => {
		const server = getLastServerSettings()?.keyboard_shortcuts;
		if (server?.locked) return !!server.value;
		const saved = localStorage.getItem(getPrefixedKey("keyboard_shortcuts"));
		return saved !== null ? saved === 'true' : (server ? !!server.value : true);
	});
	const toggle = () => {
		const value = !enabled;
		setEnabled(value);
		localStorage.setItem(getPrefixedKey("keyboard_shortcuts"), String(value));
		window.postMessage({ type: 'settings', settings: { keyboard_shortcuts: value } },
			window.location.origin);
	};
	return (
		<div className="flex w-full flex-col gap-1 p-2">
			{(renderableSettings.keyboardShortcuts ?? true) && (
				<div className="px-2 py-1">
					<SwitchRow id="keyboardShortcutsToggle" label={t('sections.shortcuts.enabledLabel')} title={t('sections.shortcuts.enabledDetails')}
						checked={enabled} onToggle={toggle} />
				</div>
			)}
			{shortcuts.map((s, i) => (
				<div key={i} className="flex items-center justify-between gap-3 px-2 py-1">
					<span className="text-sm text-foreground">{s.label}</span>
					<KbdGroup>
						{s.combo.split(' + ').map((key, j) => (
							<Fragment key={key}>
								{j > 0 && <span className="text-xs text-muted-foreground">+</span>}
								<Kbd>{key}</Kbd>
							</Fragment>
						))}
					</KbdGroup>
				</div>
			))}
			<a
				className="px-2 py-1 text-xs text-violet-600 hover:text-violet-800 dark:text-violet-400 dark:hover:text-violet-300"
				target="_blank"
				rel="noopener noreferrer"
				href="https://docs.selkies.io/citation/"
			>
				<b>{t('sections.shortcuts.citeNotice')}{" ↗"}</b>
			</a>
		</div>
	);
};

export default ShortcutsMenu;