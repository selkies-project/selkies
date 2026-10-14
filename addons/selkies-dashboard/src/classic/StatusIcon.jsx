/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The status mark the stats section and the strip over the stream draw.
 * @module
 */

const STATUS_ICONS = {
  good: <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" />,
  warn: <path d="M1 21h22L12 2zm12-3h-2v-2h2zm0-4h-2v-4h2z" />,
  neutral: <circle cx="12" cy="12" r="4" />,
};

/** The mark beside a row: its state as a shape, so color never carries it alone. */
const StatusIcon = ({ status }) => (
  <svg className={`stream-status-icon ${status}`} viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
    {STATUS_ICONS[status]}
  </svg>
);

export default StatusIcon;
