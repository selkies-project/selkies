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

/**
 * The clipboard view of the overflow menu: the server clipboard's text,
 * editable, plus an image upload when the binary clipboard is on.
 *
 * Reads `clipboardContentUpdate` and `serverSettings` messages from the core
 * and posts `clipboardUpdateFromUI` (text, on blur), `clipboardImageUpdate`
 * (an image blob), `clipboardCopySecret` (the masked secret's copy button), and
 * `settings` (the `enable_binary_clipboard` toggle, which the core persists). A
 * rejected non-image file is reported through the `fileUpload` warning channel
 * core-emitted clipboard skips use.
 *
 * The image picker belongs to the page, not to the view: the view unmounts the
 * moment its menu closes, and a picker unmounted with it never reports the
 * pick; the page outlives every menu.
 * @module
 */

/** The page's image picker, created on first use and kept for the page's life. */
let imagePicker: HTMLInputElement | null = null;
/** The image picked last, shown again when the view mounts. */
let lastPickedImage: File | null = null;

/**
 * Opens the browser's file dialog for an image and hands the pick to
 * `onPicked`, whether or not the view that asked is still mounted.
 * @param onPicked Receives the chosen file.
 */
function pickClipboardImage(onPicked: (file: File) => void): void {
	if (!imagePicker) {
		imagePicker = document.createElement('input');
		imagePicker.type = 'file';
		imagePicker.accept = 'image/*';
		imagePicker.style.display = 'none';
		document.body.appendChild(imagePicker);
	}
	const picker = imagePicker;
	picker.onchange = () => {
		const file = picker.files?.[0];
		// Cleared so re-picking the same file fires a change event.
		picker.value = '';
		if (file) onPicked(file);
	};
	picker.click();
}

/**
 * Renders the clipboard text area, the binary-clipboard switch, and the image
 * upload controls.
 *
 * State is seeded from the cached `clipboardContentUpdate`: the panel mounts
 * when its submenu opens, usually long after the core last reported the
 * clipboard. Large server clipboards arrive as a bounded, truncated preview;
 * editing it would echo the cut-down text back over the real server
 * clipboard on blur, so truncated content renders read-only. Text the
 * session's owner marked secret arrives as that flag alone and renders masked
 * and read-only, with a button that has the core copy it to this device.
 */
/** Tallest the preview is shown, matching the max-h-32 the canvas carries. */
const PREVIEW_MAX_PX = 128;
/** What the clipboard box shows for a secret, which the core never hands over. */
const CLIPBOARD_SECRET_MASK = '\u2022'.repeat(8);

export function Clipboard() {
	const [dashboardClipboardContent, setDashboardClipboardContent] = useState(
		() => getLastClipboardContent()?.text ?? '');
	const [clipboardTruncated, setClipboardTruncated] = useState(
		() => getLastClipboardContent()?.truncated ?? false);
	const [clipboardSecret, setClipboardSecret] = useState(
		() => getLastClipboardContent()?.secret ?? false);
	const [clipboardImage, setClipboardImage] = useState<File | null>(() => lastPickedImage);
	const previewRef = useRef<HTMLCanvasElement>(null);
	const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));
	const storedBool = (key: string, fallback: boolean) => {
		const saved = localStorage.getItem(getPrefixedKey(key));
		return saved !== null ? saved === 'true' : fallback;
	};
	/** A switch's starting state: a locked server value, else the stored
	 * preference, else the server's value. The panel mounts when it opens,
	 * after the server settings arrived. */
	const startingBool = (key: string) => {
		const server = getLastServerSettings()?.[key];
		if (server?.locked) return !!server.value;
		return storedBool(key, server ? !!server.value : true);
	};
	const [enableBinaryClipboard, setEnableBinaryClipboard] = useState(() => startingBool("enable_binary_clipboard"));
	const [clipboardUp, setClipboardUp] = useState(() => storedBool("clipboard_in_enabled", true));
	const [clipboardDown, setClipboardDown] = useState(() => storedBool("clipboard_out_enabled", true));
	const [clipboardSeamless, setClipboardSeamless] = useState(() => startingBool("clipboard_seamless"));

	/** One clipboard switch: optimistic, stored, then posted like any setting. */
	const toggleClientSetting = (key: string, value: boolean,
		setValue: (v: boolean) => void) => {
		setValue(value);
		localStorage.setItem(getPrefixedKey(key), String(value));
		window.postMessage({ type: 'settings', settings: { [key]: value } }, window.location.origin);
	};

	const handleBinaryClipboardToggle = () => {
		const newState = !enableBinaryClipboard;
		setEnableBinaryClipboard(newState);
		window.postMessage(
			{ type: 'settings', settings: { enable_binary_clipboard: newState } },
			window.location.origin
		);
	};

	useEffect(() => {
		const handleWindowMessage = (event: MessageEvent) => {
			if (event.origin !== window.location.origin) return;
			const message = event.data;

			if (typeof message !== 'object' || message === null) return;

			if (message.type === 'clipboardContentUpdate') {
				if (typeof message.text === 'string') {
					setDashboardClipboardContent(message.text);
					setClipboardTruncated(message.truncated === true);
					setClipboardSecret(message.secret === true);
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
				const seamless = payload?.clipboard_seamless;
				if (seamless) {
					setClipboardSeamless(seamless.locked ? seamless.value
						: storedBool('clipboard_seamless', seamless.value));
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
		if (clipboardTruncated || clipboardSecret) return;
		window.postMessage({ type: 'clipboardUpdateFromUI', text: event.target.value }, window.location.origin);
	};

	/** Has the core write the masked secret to this device's clipboard, inside this click. */
	const handleCopySecret = () => {
		window.postMessage({ type: 'clipboardCopySecret' }, window.location.origin);
	};

	/** Sends a picked image to the session clipboard, or reports a non-image. */
	const handleImagePicked = (file: File) => {
		if (!file.type.startsWith('image/')) {
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
		lastPickedImage = file;
		setClipboardImage(file);
		window.postMessage({
			type: 'clipboardImageUpdate',
			imageBlob: file,
		}, window.location.origin);
	};

	// The preview draws the picked image rather than pointing an <img> at a URL
	// for it: decoding the bytes that were picked is the whole of it, so there
	// is no URL to mint, hand to the DOM, scheme-check, or revoke.
	useEffect(() => {
		const canvas = previewRef.current;
		if (!canvas || !clipboardImage) return;
		let canceled = false;
		createImageBitmap(clipboardImage).then(bitmap => {
			if (canceled) {
				bitmap.close();
				return;
			}
			// The panel shows it at most PREVIEW_MAX_PX tall; drawing it that
			// size keeps a multi-megapixel picture off the canvas as well.
			const scale = Math.min(1, PREVIEW_MAX_PX / bitmap.height);
			canvas.width = Math.max(1, Math.round(bitmap.width * scale));
			canvas.height = Math.max(1, Math.round(bitmap.height * scale));
			canvas.getContext('2d')?.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
			bitmap.close();
		}).catch(() => {});
		return () => { canceled = true; };
	}, [clipboardImage]);

	const handleImageButtonClick = () => {
		pickClipboardImage(handleImagePicked);
	};

	const handleClearImage = () => {
		lastPickedImage = null;
		setClipboardImage(null);
	};

	return (
		<div className="flex w-full flex-col gap-1.5 p-2">
			{(renderableSettings.clipboardUp ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium" title={t('sections.clipboard.upDetails')}>{t('sections.clipboard.upLabel')}</Label>
					<Switch
						checked={clipboardUp}
						onCheckedChange={() => toggleClientSetting("clipboard_in_enabled", !clipboardUp, setClipboardUp)}
					/>
				</div>
			)}

			{(renderableSettings.clipboardDown ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium" title={t('sections.clipboard.downDetails')}>{t('sections.clipboard.downLabel')}</Label>
					<Switch
						checked={clipboardDown}
						onCheckedChange={() => toggleClientSetting("clipboard_out_enabled", !clipboardDown, setClipboardDown)}
					/>
				</div>
			)}

			{(renderableSettings.clipboardSeamless ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium" title={t('sections.clipboard.seamlessDetails')}>{t('sections.clipboard.seamlessLabel')}</Label>
					<Switch
						checked={clipboardSeamless}
						onCheckedChange={() => toggleClientSetting("clipboard_seamless", !clipboardSeamless, setClipboardSeamless)}
					/>
				</div>
			)}

			{(renderableSettings.binaryClipboard ?? true) && (
				<div className="flex items-center justify-between">
					<Label className="text-sm font-medium" title={t('sections.clipboard.binaryModeDetails')}>{t('sections.clipboard.binaryModeLabel')}</Label>
					<Switch
						checked={enableBinaryClipboard}
						onCheckedChange={handleBinaryClipboardToggle}
					/>
				</div>
			)}

			<Label htmlFor="dashboardClipboardTextarea" className="text-sm font-medium">{t('sections.clipboard.title')}</Label>
			<Textarea
				id="dashboardClipboardTextarea"
				value={clipboardSecret ? CLIPBOARD_SECRET_MASK : dashboardClipboardContent}
				onChange={handleClipboardChange}
				onBlur={handleClipboardBlur}
				readOnly={clipboardTruncated || clipboardSecret}
				rows={3}
				placeholder={t('clipboard.inputPlaceholder')}
				className="allow-native-input resize-none bg-background/95 overflow-y-auto max-h-[96px] text-sm"
			/>

			{clipboardSecret && (
				<div className="flex flex-col gap-1.5">
					<p className="text-xs text-muted-foreground">{t('sections.clipboard.secretHidden')}</p>
					<Button variant="outline" size="sm" className="h-7" onClick={handleCopySecret}>
						{t('sections.clipboard.copySecret')}
					</Button>
				</div>
			)}

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
					{clipboardImage && (
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

				{clipboardImage && (
					<canvas
						ref={previewRef}
						role="img"
						aria-label={t('clipboard.previewAlt')}
						className="max-w-full max-h-24 rounded border"
					/>
				)}
			</div>
			)}
		</div>
	);
}
