/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { useEffect, useState } from "react";
import { TriangleAlert } from "lucide-react";
import { t } from "@/i18n";
import { STATS_EVENT } from "../../../../selkies-web-core/lib/stream-stats.js";

/**
 * The mark over the stream while the server finds this page's connection poor
 * (`connection` in `window.stream_client`): too many of the display's frames
 * lost on the way or held back for the link. The server says so only when its
 * verdict changes, so the mark needs no stats open, and it takes no pointer
 * input.
 * @module
 */

const isPoor = () => (window as any).stream_client?.connection === "poor";

/** Mounted next to the toaster, apart from the menu bar the settings can hide. */
export function ConnectionIndicator() {
    const [poor, setPoor] = useState(isPoor);

    useEffect(() => {
        const read = () => setPoor(isPoor());
        read();
        window.addEventListener(STATS_EVENT, read);
        return () => window.removeEventListener(STATS_EVENT, read);
    }, []);

    if (!poor) return null;
    return (
        <div
            role="status"
            className="pointer-events-none fixed bottom-5 left-5 z-40 flex select-none items-center gap-1.5 rounded-md bg-black/55 px-2.5 py-1 text-xs text-white"
        >
            <TriangleAlert className="h-3.5 w-3.5 shrink-0 text-[#fab219]" aria-hidden />
            {t("notifications.poorConnection")}
        </div>
    );
}

export default ConnectionIndicator;
