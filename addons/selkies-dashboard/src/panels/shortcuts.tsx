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
 * The keyboard shortcuts: the core-owned chords as dense rows of keys and the
 * switch that hands them to the session instead; and the citation link the
 * layouts put at their foot.
 * @module
 */

const shortcuts = [
	{ label: t('sections.shortcuts.fullscreen'), combo: "Ctrl + Shift + F" },
	{ label: t('sections.shortcuts.gamingMode'), combo: "Ctrl + Shift + X" },
	{ label: t('sections.shortcuts.openMenu'), combo: "Ctrl + Shift + M" },
	{ label: t('sections.shortcuts.toggleGamepad'), combo: "Ctrl + Shift + G" },
	{ label: t('sections.shortcuts.pointerLock'), combo: "Ctrl + Shift + Left Click" },
];

/** The link to the paper to cite, for the foot of a layout. */
export function CiteLink({ className }: { className?: string }) {
	return (
		<a className={`cite-link text-xs font-semibold text-(--primary-text) underline underline-offset-2 ${className ?? ""}`}
			target="_blank" rel="noopener noreferrer" href="https://docs.selkies.io/citation/">
			{t('sections.shortcuts.citeNotice')}{" ↗"}
		</a>
	);
}

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
		<div className="shortcuts flex w-full flex-col gap-(--stack-gap) p-2">
			{(renderableSettings.keyboardShortcuts ?? true) && (
				<SwitchRow id="keyboardShortcutsToggle" label={t('sections.shortcuts.enabledLabel')} title={t('sections.shortcuts.enabledDetails')}
					checked={enabled} onToggle={toggle} />
			)}
			{shortcuts.map((s, i) => (
				<div key={i} className="shortcut-item flex items-center justify-between gap-3">
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
		</div>
	);
};

export default ShortcutsMenu;