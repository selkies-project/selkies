/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The mark over the stream while the server finds this page's connection poor
 * (`connection` in `window.stream_client`): too many of the display's frames
 * lost on the way or held back for the link. The server says so only when its
 * verdict changes, so the mark needs no stats open, and it takes no pointer
 * input.
 * @module
 */
import { useEffect, useState } from "react";
import { STATS_EVENT } from "../../../selkies-web-core/lib/stream-stats.js";
import StatusIcon from "./StatusIcon.jsx";

const isPoor = () => !!window.stream_client && window.stream_client.connection === "poor";

/**
 * @param {{t: function(string, (Object|string)=): string}} props
 */
export default function ConnectionIndicator({ t }) {
  const [poor, setPoor] = useState(isPoor);

  useEffect(() => {
    const read = () => setPoor(isPoor());
    read();
    window.addEventListener(STATS_EVENT, read);
    return () => window.removeEventListener(STATS_EVENT, read);
  }, []);

  if (!poor) return null;
  return (
    <div className="connection-indicator" role="status">
      <StatusIcon status="warn" />
      {t("notifications.poorConnection")}
    </div>
  );
}
