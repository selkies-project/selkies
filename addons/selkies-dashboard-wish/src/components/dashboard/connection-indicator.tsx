/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useEffect, useState } from "react";
import { TriangleAlert } from "lucide-react";
import { t } from "@/i18n";
import { getLastServerSettings } from "@/utils";
import { STATS_EVENT } from "../../../../selkies-web-core/lib/stream-stats.js";

/**
 * The mark over the stream while the server finds this page's connection poor
 * (`connection` in `window.stream_client`): too many of the display's frames
 * lost on the way or held back for the link. The server says so only when its
 * verdict changes, so the mark needs no stats open, and it takes no pointer
 * input. `ui_show_connection_indicator` hides it.
 * @module
 */

const isPoor = () => (window as any).stream_client?.connection === "poor";
const isEnabled = (settings: any) => settings?.ui_show_connection_indicator?.value !== false;

/**
 * Mounted next to the toaster, apart from the menu bar the settings can hide,
 * and above the soft keys where a touch page shows them (`--soft-keys-top`).
 */
export function ConnectionIndicator() {
    const [poor, setPoor] = useState(isPoor);
    const [enabled, setEnabled] = useState(() => isEnabled(getLastServerSettings()));

    useEffect(() => {
        const read = () => setPoor(isPoor());
        const onMessage = (event: MessageEvent) => {
            if (event.origin === window.location.origin && event.data?.type === "serverSettings") {
                setEnabled(isEnabled(event.data.payload));
            }
        };
        read();
        window.addEventListener(STATS_EVENT, read);
        window.addEventListener("message", onMessage);
        return () => {
            window.removeEventListener(STATS_EVENT, read);
            window.removeEventListener("message", onMessage);
        };
    }, []);

    if (!poor || !enabled) return null;
    return (
        <div
            role="status"
            className="pointer-events-none fixed left-5 z-40 flex select-none items-center gap-1.5 rounded-md bg-black/55 px-2.5 py-1 text-xs text-white"
            style={{ bottom: "min(calc(var(--soft-keys-top, 0.75rem) + 0.5rem), calc(100% - 2.5rem))" }}
        >
            <TriangleAlert className="h-3.5 w-3.5 shrink-0 text-[#fab219]" aria-hidden />
            {t("notifications.poorConnection")}
        </div>
    );
}

export default ConnectionIndicator;
