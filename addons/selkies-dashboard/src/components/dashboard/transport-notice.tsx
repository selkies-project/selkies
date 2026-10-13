/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useEffect } from "react";
import { toast } from "sonner";
import { t } from "@/i18n";
import { computeRenderableSettings, getLastServerSettings, isViewerUrlMode } from "@/utils";
import { switchStreamMode } from "../../../../selkies-web-core/lib/mode-switch.js";

/**
 * The toast the WebRTC core's `transportAdvice` raises while its media path
 * keeps failing beside a working signaling socket, and withdraws once a
 * session connects. It offers the switch to WebSockets where the settings
 * panel would (dual mode on, not a viewer); elsewhere it says who can.
 * @module
 */

const TOAST_ID = "transport-advice";

/** Renders nothing; it only listens, mounted next to the toaster. */
export function TransportNotice() {
    useEffect(() => {
        let viewer = isViewerUrlMode;
        const handleWindowMessage = (event: MessageEvent) => {
            if (event.origin !== window.location.origin) return;
            const message = event.data;
            if (typeof message !== "object" || message === null) return;
            if (message.type === "clientRoleUpdate") {
                viewer = message.role === "viewer";
                return;
            }
            if (message.type !== "transportAdvice") return;
            if (message.offer !== "websockets") {
                toast.dismiss(TOAST_ID);
                return;
            }
            const dualMode = computeRenderableSettings(getLastServerSettings()).enableDualMode
                ?? (window as any).__SELKIES_DUAL_MODE__ ?? false;
            const canSwitch = dualMode && !viewer;
            toast.warning(t("notifications.webrtcFailedTitle"), {
                id: TOAST_ID,
                duration: Infinity,
                description: canSwitch ? t("notifications.webrtcFailedSwitch") : t("notifications.webrtcFailedNoSwitch"),
                action: canSwitch
                    ? { label: t("notifications.switchToWebsockets"), onClick: () => { switchStreamMode("websockets"); } }
                    : undefined,
            });
        };
        window.addEventListener("message", handleWindowMessage);
        return () => window.removeEventListener("message", handleWindowMessage);
    }, []);

    return null;
}

export default TransportNotice;
