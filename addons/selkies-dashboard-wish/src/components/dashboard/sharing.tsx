/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Card, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Info } from "lucide-react";
import { toast } from "sonner";
import { computeRenderableSettings, getLastServerSettings } from "@/utils";
import { t } from "@/i18n";

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
	onClose: () => void;
}

export const Sharing = ({ show, onClose }: SharingProps) => {
	const [copiedId, setCopiedId] = useState<string | null>(null);
	const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));

	const baseUrl =
		typeof window !== "undefined" ? window.location.href.split("#")[0] : "";

	// --- Server Settings Message Listener ---
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

	const handleCopyLink = async (fullUrl: string, id: string, label: string) => {
		if (!navigator.clipboard) {
			console.warn("Clipboard API not available.");
			return;
		}
		try {
			await navigator.clipboard.writeText(fullUrl);
			setCopiedId(id);
			setTimeout(() => setCopiedId(null), 2000);
			
			// Show success toast
			toast.success(t('notifications.copiedTitle', { label }), {
				description: t('notifications.copiedMessage', { textToCopy: fullUrl }),
				duration: 3000,
			});
		} catch (err) {
			console.error("Failed to copy link: ", err);

			// Show error toast
			toast.error(t('notifications.copyFailedTitle', { label }), {
				description: t('notifications.copyFailedError'),
				duration: 5000,
			});
		}
	};

	if (!show) return null;

	// Filter sharing links based on server settings
	const filteredSharingLinks = sharingLinks.filter(link => {
		if (link.id === 'shared') return renderableSettings.enableShared ?? true;
		if (link.id === 'player2') return renderableSettings.enablePlayer2 ?? true;
		if (link.id === 'player3') return renderableSettings.enablePlayer3 ?? true;
		if (link.id === 'player4') return renderableSettings.enablePlayer4 ?? true;
		return false;
	});

	// Don't show sharing panel if sharing is disabled
	if (renderableSettings.enableSharing === false) {
		return (
			<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-lg rounded-lg relative p-4">
				<div className="text-center text-muted-foreground">
					<Info className="h-8 w-8 mx-auto mb-2" />
					<p className="text-sm">{t('sharing.disabledByAdmin')}</p>
				</div>
			</Card>
		);
	}

	return (
		<Card className="w-[320px] bg-background/95 backdrop-blur-sm border shadow-lg rounded-lg relative p-2">
			<div className="px-2 py-0 flex flex-col gap-2">
				<div className="flex items-center justify-between mb-1">
					<CardTitle className="text-xs font-bold">{t('sharing.shareLinksTitle')}</CardTitle>
					<Tooltip>
						<TooltipTrigger asChild>
							<span className="inline-block cursor-help">
								<Info className="h-4 w-4 text-muted-foreground" />
							</span>
						</TooltipTrigger>
						<TooltipContent className="text-sm bg-primary text-primary-foreground">
							{t('sharing.tooltipLine1')}<br />
							{t('sharing.tooltipLine2')}
						</TooltipContent>
					</Tooltip>
				</div>
				<div className="grid grid-cols-2 gap-3">
					{filteredSharingLinks.map((link) => {
						const fullUrl = `${baseUrl}${link.hash}`;
						return (
							<div key={link.id} className="flex flex-col items-start justify-between bg-muted/60 rounded-md p-3 gap-2 shadow-sm">
								<span className="font-medium text-xs text-foreground text-left w-full">{link.label}</span>
								<Badge variant="green" className="text-[10px] px-2 py-0.5 mb-1">{link.badge}</Badge>
								<div className="flex w-full justify-end">
									<Button
										variant="outline"
										size="sm"
										className="px-2 py-0.5 h-6 text-xs font-medium"
										onClick={() => handleCopyLink(fullUrl, link.id, link.label)}
										aria-label={t('sharing.copyAria', { label: link.label })}
									>
										{copiedId === link.id ? (
											<span className="text-green-600">{t('sharing.copied')}</span>
										) : (
											<span>{t('sharing.copy')}</span>
										)}
									</Button>
								</div>
							</div>
						);
					})}
				</div>
				{filteredSharingLinks.length === 0 && (
					<div className="text-center text-muted-foreground py-4">
						<p className="text-sm">{t('sharing.noneAvailable')}</p>
					</div>
				)}
			</div>
		</Card>
	);
};
