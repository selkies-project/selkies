/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useEffect } from "react";
import { toast } from "sonner";
import { Printer } from "lucide-react";
import { Button } from "@/components/ui/button";
import { t } from "@/i18n";
import { isMobileClient } from "@/utils";
import { resolveFailedAppCommand } from "../../../../selkies-web-core/lib/app-commands.js";

/**
 * Toasts for the notices the core sends on the `fileUpload` message channel:
 * upload progress and failures, clipboard-image refusals, and app commands
 * that did not run. On a touch-first client each `printDocument` message is
 * a toast as well, kept until its link opens the document in a new tab or
 * the toast is closed, since such a client prints only from its own PDF
 * viewer and the menu is closed while an application prints.
 * @module
 */

/**
 * Renders nothing; it only listens.
 *
 * Mounted next to the toaster rather than inside the panel that triggers the
 * notices. A listener that lives in a menu is unmounted whenever that menu is
 * closed, and these arrive while the user is somewhere else in the
 * interface: an app install that failed has to settle its optimistic badge
 * wherever the notice lands. A warning carries an optional translation code
 * (`clipboard*`, `commandFailed`, `keyboardLock*`); the translator returns the key itself
 * for an unknown code, since a future core may ship new ones, so the raw
 * message is the fallback, as in the classic dashboard.
 */
export function UploadNotifications() {
    useEffect(() => {
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message !== 'object' || message === null) return;
            if (message.type === 'printDocument' && isMobileClient && typeof message.url === 'string') {
                const url: string = message.url;
                // One row in place of a title: the link, the name, the
                // printer mark at the end, so a short name leaves no hollow box.
                toast(
                    <div className="flex w-full items-center gap-2">
                        <Button variant="outline" size="sm" asChild>
                            <a href={url} target="_blank" onClick={() => toast.dismiss(url)}>
                                {t('sections.printing.openButton')}
                            </a>
                        </Button>
                        <span className="min-w-0 flex-1 truncate" title={message.name}>{message.name}</span>
                        <Printer className="h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                    </div>,
                    { id: url, duration: Infinity, classNames: { content: 'min-w-0 flex-1', title: 'w-full' } },
                );
                return;
            }
            if (message.type !== 'fileUpload') return;

            const { status, fileName, progress, message: errMsg, code } = message.payload;

            if (status === 'start') {
                toast.loading(t('uploads.uploadingFile', { fileName }), {
                    id: fileName,
                });
            } else if (status === 'progress') {
                toast.loading(t('uploads.uploadingFileProgress', { fileName, progress }), {
                    id: fileName,
                });
            } else if (status === 'end') {
                toast.success(t('uploads.uploadSuccessFile', { fileName }), {
                    id: fileName,
                });
            } else if (status === 'error') {
                const errorMessage = errMsg ? `${t('notifications.errorPrefix')} ${errMsg}` : t('notifications.unknownError');
                toast.error(t('uploads.uploadFailedFile', { fileName, errorMessage }), {
                    id: fileName,
                });
            } else if (status === 'warning') {
                // A failed apps command settles its pending optimistic
                // update first; a stale launch match is lifecycle noise,
                // not a notice.
                if (code === 'commandFailed' && !resolveFailedAppCommand(errMsg)) return;
                const codeKey = (typeof code === 'string' &&
                    (code.startsWith('clipboard') || code === 'commandFailed' || code.startsWith('keyboardLock')))
                    ? `notifications.${code}` : null;
                const codeMsg = codeKey ? t(codeKey, { detail: errMsg }) : null;
                const warnMsg = (codeMsg && codeMsg !== codeKey)
                    ? codeMsg
                    : (errMsg || t('notifications.unknownError'));
                toast.warning(warnMsg, {
                    id: fileName,
                });
            }
        };

        window.addEventListener('message', handleWindowMessage);
        return () => window.removeEventListener('message', handleWindowMessage);
    }, []);

    return null;
}

export default UploadNotifications;
