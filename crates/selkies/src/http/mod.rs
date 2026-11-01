/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The HTTP/HTTPS control plane and static-content server: the axum application every
//! deployment fronts, its auth middleware and token table, the status and operator routes, the
//! web client, TLS with hot reload, and listening on TCP or a Unix socket (`stream_server.py`,
//! `sessions.py`, `metrics.py`).
//!
//! Being ported; the Python module named is the specification.
