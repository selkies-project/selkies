/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { X } from "lucide-react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from "@/components/ui/dialog";
import { computeRenderableSettings, getLastServerSettings } from "@/utils";
import { t } from "@/i18n";
import { withSessionToken } from "../../../../selkies-web-core/lib/session-token.js";

interface FilesProps {
    // Opens the Download Files dialog. The dialog is NOT rendered here: this
    // component lives inside a menubar submenu, and Radix closes every open
    // menu when the window blurs, which a click inside the file-manager iframe
    // causes. A dialog mounted in the submenu would be torn down by that
    // click, so TopMenu hosts FilesDialog beside the menubar (as it does the
    // Apps modal) and only the request to open it comes from here.
    onOpenDownloads?: () => void;
}

export function Files({ onOpenDownloads }: FilesProps) {
    const [renderableSettings, setRenderableSettings] = useState<any>(() => computeRenderableSettings(getLastServerSettings()));

    const handleUploadClick = () => {
        window.dispatchEvent(new CustomEvent('requestFileUpload'));
    };

    // The upload and command notices are raised by UploadNotifications, which
    // stays mounted while this panel is closed.
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

    const showUpload = renderableSettings.fileUpload ?? true;
    const showDownload = renderableSettings.fileDownload ?? true;

    if (!showUpload && !showDownload) return null;

    return (
        <div className="w-auto p-4 flex flex-col gap-2">
            {showUpload && (
                <Button
                    variant="outline"
                    className="mb-2"
                    onClick={handleUploadClick}
                >
                    {t('sections.files.uploadButton')}
                </Button>
            )}
            {showDownload && (
                <Button
                    variant="outline"
                    className="mb-2"
                    onClick={onOpenDownloads}
                >
                    {t('sections.files.downloadButtonTitle')}
                </Button>
            )}
        </div>
    );
}

interface FilesDialogProps {
    open: boolean;
    onOpenChange: (open: boolean) => void;
}

// The Download Files dialog: the server's file index in an iframe, whose file
// links are attachments. Mounted by TopMenu outside the menubar so a click in
// the iframe (which blurs the window and closes the menus) leaves it open.
export function FilesDialog({ open, onOpenChange }: FilesDialogProps) {
    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent className="max-h-[90vh] sm:max-w-[80vw] p-0">
                <DialogHeader className="sticky top-0 z-10 bg-background p-6 border-b">
                    <div className="flex flex-col space-y-6">
                        <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-6">
                            <div>
                                <DialogTitle>{t('sections.files.title')}</DialogTitle>
                                <DialogDescription>
                                    {t('files.subtitle')}
                                </DialogDescription>
                            </div>
                            <div className="flex items-center gap-2">
                                <Button
                                    variant="secondary"
                                    size="icon"
                                    onClick={() => onOpenChange(false)}
                                    className="h-10 w-10"
                                >
                                    <X className="h-4 w-4" />
                                    <span className="sr-only">{t('common.close')}</span>
                                </Button>
                            </div>
                        </div>
                    </div>
                </DialogHeader>

                <div className="flex-1 overflow-hidden">
                    <iframe
                        src={withSessionToken("api/files/")}
                        title={t('filesModal.iframeTitle')}
                        className="w-full h-[calc(90vh-8rem)] border-0"
                    />
                </div>
            </DialogContent>
        </Dialog>
    );
}
