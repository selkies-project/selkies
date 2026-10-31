/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from "@/components/ui/dialog";
import { computeRenderableSettings, getLastServerSettings, isViewerUrlMode } from "@/utils";
import { t } from "@/i18n";
import { FileManager } from "@/panels/file-manager";

/**
 * The file manager dialog, and the overflow menu's entry to it.
 *
 * The menu offers one `Files` item beside Apps and Clipboard, not a submenu:
 * uploading and downloading both happen inside the manager, so a second upload
 * button in the menu would be a second way to the same place. TopMenu hosts the
 * dialog beside the toolbar, as it does the Apps modal, because the overflow
 * menu's popup unmounts with it, and hides the entry when the server allows
 * neither transfer.
 * @module
 */

interface FilesDialogProps {
    open: boolean;
    onOpenChange: (open: boolean) => void;
}

/**
 * The file manager dialog: the file manager over the server's JSON file API,
 * with downloads served as attachments from the same tree. Hosted by TopMenu
 * beside the toolbar, as it does the Apps modal, because the overflow menu's
 * popup unmounts with it.
 */
export function FilesDialog({ open, onOpenChange }: FilesDialogProps) {
    const [renderableSettings, setRenderableSettings] = useState<any>(() => 
        computeRenderableSettings(getLastServerSettings())
    );

    useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;

            if (typeof message !== 'object' || message === null) return;

            if (message.type === 'serverSettings') {
                setRenderableSettings(computeRenderableSettings(message.payload));
            }
        };

        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, []);

    const canUpload = (renderableSettings.fileUpload ?? true) && !isViewerUrlMode;
    const allowDownload = (renderableSettings.fileDownload ?? true) && !isViewerUrlMode;

    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            {/* The manager is a window, not a sheet: it keeps a margin on a phone
                but stops growing at a size a file list reads well at, so a wide
                monitor does not stretch it. The `!` are the base dialog's own
                width and padding, which otherwise outrank these. */}
            <DialogContent
                className="allow-native-input flex! flex-col gap-0! overflow-hidden p-0! h-[90vh]! w-[94vw]! sm:h-[min(82vh,40rem)]! sm:max-h-[min(82vh,40rem)]! sm:w-[min(92vw,54rem)]! sm:max-w-[min(92vw,54rem)]!"
            >
                <DialogHeader className="px-4 pt-4 pb-3 border-b shrink-0">
                    <DialogTitle>{t('sections.files.title')}</DialogTitle>
                    <DialogDescription>
                        {t('files.subtitle')}
                    </DialogDescription>
                </DialogHeader>
                <div className="flex-1 min-h-0 overflow-hidden p-2">
                    <FileManager
                        canUpload={canUpload}
                        allowDownload={allowDownload}
                        t={t}
                        className="h-full"
                    />
                </div>
            </DialogContent>
        </Dialog>
    );
}
