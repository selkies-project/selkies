/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect, useRef } from "react";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { computeRenderableSettings, getLastClipboardContent, getLastServerSettings, getPrefixedKey } from "@/utils";
import { t } from "@/i18n";

export function Clipboard() {
	// Seeded from the cached clipboardContentUpdate: this panel mounts when its
	// submenu opens, usually long after the core last reported the clipboard.
	const [dashboardClipboardContent, setDashboardClipboardContent] = useState(
		() => getLastClipboardContent()?.text ?? '');
	// Large server clipboards arrive as a bounded, truncated preview; editing it
	// would echo the cut-down text back over the real server clipboard on blur,
	// so truncated content renders read-only.
	const [clipboardTruncated, setClipboardTruncated] = useState(
		() => getLastClipboardContent()?.truncated ?? false);
	const [clipboardImageUrl, setClipboardImageUrl] = useState<string | null>(null);
	const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));
	const [enableBinaryClipboard, setEnableBinaryClipboard] = useState(() => {
		const saved = localStorage.getItem(getPrefixedKey("enable_binary_clipboard"));
		return saved !== null ? saved === 'true' : true;
	});
	const fileInputRef = useRef<HTMLInputElement>(null);

	const handleBinaryClipboardToggle = () => {
		const newState = !enableBinaryClipboard;
		setEnableBinaryClipboard(newState);
		// Core persists enable_binary_clipboard when handling the settings message.
		window.postMessage(
			{ type: 'settings', settings: { enable_binary_clipboard: newState } },
			window.location.origin
		);
	};

	// --- Message Listener for Clipboard and Server Settings Updates ---
	useEffect(() => {
		const handleWindowMessage = (event: MessageEvent) => {
			if (event.origin !== window.location.origin) return;
			const message = event.data;

			if (typeof message !== 'object' || message === null) return;

			if (message.type === 'clipboardContentUpdate') {
				if (typeof message.text === 'string') {
					setDashboardClipboardContent(message.text);
					setClipboardTruncated(message.truncated === true);
				}
			}

			if (message.type === 'serverSettings') {
				const payload = message.payload;
				setRenderableSettings(computeRenderableSettings(payload));
				const s = payload?.enable_binary_clipboard;
				if (s) {
					const saved = localStorage.getItem(getPrefixedKey('enable_binary_clipboard'));
					const final = s.locked ? s.value : (saved !== null ? saved === 'true' : s.value);
					setEnableBinaryClipboard(final);
				}
			}
		};

		window.addEventListener('message', handleWindowMessage);
		return () => window.removeEventListener('message', handleWindowMessage);
	}, []);

	const handleClipboardChange = (event: React.ChangeEvent<HTMLTextAreaElement>) => {
		setDashboardClipboardContent(event.target.value);
	};

	const handleClipboardBlur = (event: React.FocusEvent<HTMLTextAreaElement>) => {
		if (clipboardTruncated) return;
		window.postMessage({ type: 'clipboardUpdateFromUI', text: event.target.value }, window.location.origin);
	};

	const handleImageUpload = (event: React.ChangeEvent<HTMLInputElement>) => {
		const file = event.target.files?.[0];
		// Allow re-picking the same file: without clearing the value, a
		// same-path selection fires no change event.
		event.target.value = '';
		if (!file) return;
		if (!file.type.startsWith('image/')) {
			// Same warning channel core-emitted clipboard skips use
			// (classic-dashboard parity).
			window.postMessage({
				type: 'fileUpload',
				payload: {
					status: 'warning',
					fileName: 'clipboard-image',
					message: t('notifications.clipboardImageRejected', {
						name: file.name,
						mime: file.type || 'unknown',
					}),
				},
			}, window.location.origin);
			return;
		}
		// The picked file is already the blob the core wants, and an object URL
		// previews it without reading a multi-megabyte image through base64 on
		// the main thread.
		setClipboardImageUrl(previous => {
			if (previous) URL.revokeObjectURL(previous);
			return URL.createObjectURL(file);
		});
		window.postMessage({
			type: 'clipboardImageUpdate',
			imageBlob: file,
		}, window.location.origin);
	};

	const handleImageButtonClick = () => {
		fileInputRef.current?.click();
	};

	const handleClearImage = () => {
		setClipboardImageUrl(previous => {
			if (previous) URL.revokeObjectURL(previous);
			return null;
		});
		if (fileInputRef.current) {
			fileInputRef.current.value = '';
		}
	};

	return (
		<div className="w-[300px] p-4 flex flex-col gap-2">
			{(renderableSettings.binaryClipboard ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium" title={t('sections.clipboard.binaryModeDetails')}>{t('sections.clipboard.binaryModeLabel')}</Label>
					<Switch
						checked={enableBinaryClipboard}
						onCheckedChange={handleBinaryClipboardToggle}
					/>
				</div>
			)}

			<Label htmlFor="dashboardClipboardTextarea">{t('sections.clipboard.title')}</Label>
			<Textarea
				id="dashboardClipboardTextarea"
				value={dashboardClipboardContent}
				onChange={handleClipboardChange}
				onBlur={handleClipboardBlur}
				readOnly={clipboardTruncated}
				rows={5}
				placeholder={t('clipboard.inputPlaceholder')}
				className="allow-native-input resize-none bg-background/95 overflow-y-auto max-h-[150px]"
			/>

			{/* Image writes need the binary clipboard: the server drops them otherwise. */}
			{(renderableSettings.binaryClipboard ?? true) && enableBinaryClipboard && (
			<div className="flex flex-col gap-2">
				<div className="flex gap-2">
					<Button
						variant="outline"
						size="sm"
						onClick={handleImageButtonClick}
						className="flex-1"
					>
						{t('clipboard.uploadImage')}
					</Button>
					{clipboardImageUrl && (
						<Button
							variant="outline"
							size="sm"
							onClick={handleClearImage}
							className="flex-1"
						>
							{t('clipboard.clearImage')}
						</Button>
					)}
				</div>

				<input
					ref={fileInputRef}
					type="file"
					accept="image/*"
					onChange={handleImageUpload}
					className="hidden"
				/>

				{clipboardImageUrl && (
					<div className="mt-2">
						<img
							src={clipboardImageUrl}
							alt={t('clipboard.previewAlt')}
							className="max-w-full max-h-32 object-contain rounded border"
						/>
					</div>
				)}
			</div>
			)}
		</div>
	);
}
