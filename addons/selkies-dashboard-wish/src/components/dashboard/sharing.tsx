/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Badge } from "@/components/ui/badge";
import { Check, Copy, Info } from "lucide-react";
import { toast } from "sonner";
import { computeRenderableSettings, getLastServerSettings } from "@/utils";
import { t } from "@/i18n";
import { shareablePageURL } from "../../../../selkies-web-core/lib/page-url.js";

/**
 * The sharing view of the overflow menu: copyable links for the viewer
 * (`#shared`) and the controller slots (`#player2` to `#player4`), one row
 * each with a copy button that shows the copy's outcome, gated by the
 * matching server setting from `serverSettings` messages.
 * @module
 */

/** Every shareable hash with its label and badge; the server settings filter it. */
const sharingLinks = [
	{
		id: "shared",
		label: t('sharing.viewerLabel'),
		badge: t('sharing.viewerBadge'),
		hash: "#shared",
	},
	{
		id: "player2",
		label: t('sharing.controllerLabel', { n: 2 }),
		badge: t('sharing.controllerBadge', { n: 2 }),
		hash: "#player2",
	},
	{
		id: "player3",
		label: t('sharing.controllerLabel', { n: 3 }),
		badge: t('sharing.controllerBadge', { n: 3 }),
		hash: "#player3",
	},
	{
		id: "player4",
		label: t('sharing.controllerLabel', { n: 4 }),
		badge: t('sharing.controllerBadge', { n: 4 }),
		hash: "#player4",
	},
];

interface SharingProps {
	show: boolean;
}

/**
 * Renders the link cards, a notice when the admin disabled sharing, or
 * nothing while `show` is false.
 */
export const Sharing = ({ show }: SharingProps) => {
	const [copiedId, setCopiedId] = useState<string | null>(null);
	const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));

	const baseUrl =
		typeof window !== "undefined" ? shareablePageURL() : "";

	useEffect(() => {
		const handleMessage = (event: MessageEvent) => {
			if (
				event.origin === window.location.origin &&
				event.data?.type === "serverSettings"
			) {
				console.log("Sharing received server settings:", event.data.payload);
				setRenderableSettings(computeRenderableSettings(event.data.payload));
			}
		};
		window.addEventListener("message", handleMessage);
		return () => {
			window.removeEventListener("message", handleMessage);
		};
	}, []);

	/** Copies a link to the local clipboard and reports the outcome as a toast. */
	const handleCopyLink = async (fullUrl: string, id: string, label: string) => {
		if (!navigator.clipboard) {
			console.warn("Clipboard API not available.");
			return;
		}
		try {
			await navigator.clipboard.writeText(fullUrl);
			setCopiedId(id);
			setTimeout(() => setCopiedId(null), 2000);
			
			toast.success(t('notifications.copiedTitle', { label }), {
				description: t('notifications.copiedMessage', { textToCopy: fullUrl }),
				duration: 3000,
			});
		} catch (err) {
			console.error("Failed to copy link: ", err);

			toast.error(t('notifications.copyFailedTitle', { label }), {
				description: t('notifications.copyFailedError'),
				duration: 5000,
			});
		}
	};

	if (!show) return null;

	const filteredSharingLinks = sharingLinks.filter(link => {
		if (link.id === 'shared') return renderableSettings.enableShared ?? true;
		if (link.id === 'player2') return renderableSettings.enablePlayer2 ?? true;
		if (link.id === 'player3') return renderableSettings.enablePlayer3 ?? true;
		if (link.id === 'player4') return renderableSettings.enablePlayer4 ?? true;
		return false;
	});

	if (renderableSettings.enableSharing === false) {
		return (
			<p className="flex items-center justify-center gap-2 p-3 text-sm text-muted-foreground">
				<Info className="h-4 w-4 shrink-0" />
				{t('sharing.disabledByAdmin')}
			</p>
		);
	}

	return (
		<div className="flex w-full flex-col gap-0.5 p-2">
			<div className="flex items-center justify-between px-2 py-1">
				<span className="text-xs font-medium text-muted-foreground">{t('sharing.shareLinksTitle')}</span>
				<Tooltip>
					<TooltipTrigger
						render={<span className="inline-block cursor-help" />}
					>
						<Info className="h-4 w-4 text-muted-foreground" />
					</TooltipTrigger>
					<TooltipContent className="text-sm bg-primary text-primary-foreground">
						{t('sharing.tooltipLine1')}<br />
						{t('sharing.tooltipLine2')}
					</TooltipContent>
				</Tooltip>
			</div>
			{filteredSharingLinks.map((link) => {
				const fullUrl = `${baseUrl}${link.hash}`;
				return (
					<div key={link.id} className="flex items-center justify-between gap-2 rounded-md px-2 py-1 hover:bg-accent">
						<span className="flex min-w-0 items-center gap-2">
							<span className="truncate text-sm font-medium">{link.label}</span>
							<Badge variant="green" className="shrink-0 px-1.5 text-[10px]">{link.badge}</Badge>
						</span>
						<Button
							variant="ghost"
							size="icon"
							className="h-6 w-6 shrink-0"
							onClick={() => handleCopyLink(fullUrl, link.id, link.label)}
							aria-label={t('sharing.copyAria', { label: link.label })}
						>
							{copiedId === link.id ? (
								<Check className="h-3.5 w-3.5 text-green-600" />
							) : (
								<Copy className="h-3.5 w-3.5" />
							)}
						</Button>
					</div>
				);
			})}
			{filteredSharingLinks.length === 0 && (
				<p className="px-2 py-3 text-center text-sm text-muted-foreground">{t('sharing.noneAvailable')}</p>
			)}
		</div>
	);
};
