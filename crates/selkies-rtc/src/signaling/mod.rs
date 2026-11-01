/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! WebRTC signaling: the peer registry, the session and room relay, and the secure-mode
//! gates, as a tokio state machine with no HTTP library in it.
//!
//! One server peer owns the media graph; browser client peers (controllers and viewers, one
//! set per display) pair with it through SESSION messages, or exchange ROOM messages for
//! multi-peer rooms. The server crate mounts `PeerRegistry::serve` on its WebSocket route
//! through `socket::socket`; the service registers as the server peer through
//! `client::SignalingClient`.
//!
//! Wire protocol (text frames, space-separated): a peer opens with `HELLO <server|client>
//! [<json-metadata>]` (`wire::HelloMetadata`: `client_type`, `client_slot`,
//! `client_strict_viewer`, `client_token`, `server_token`, `display_id`, `display_position`,
//! `fullcolor_codecs`: the codec names the client decodes at 4:4:4, `tenbit_codecs`: the
//! formats it decodes at 10 bits, a codec name for 4:2:0 and the name with `444` after it for
//! 4:4:4, `client_tab_id`, `cc_start_kbps`: the rate congestion control last held the page's
//! display at, `page_size`: the `[width, height]` its first resize will ask for,
//! `resume_key`: a secret of the page's session) and is answered `HELLO`. A server that does
//! not know a key ignores it. A client whose socket was cut with no close frame keeps its
//! session for `SESSION_HOLD`: a hello naming its `resume_key` is answered `RESUMED
//! <callee-id>` and carries on that session, under its old peer id, with what was addressed
//! to it meanwhile; the media path never needed the socket. `SESSION <peer-id|server>` pairs
//! the caller with the callee: the caller gets `SESSION_OK <callee-id>`, the callee
//! `SESSION_START <caller-id> <client_type> <display_id> <display_position> [<client_token>]
//! [fullcolor=<codec,...>] [tenbit=<format,...>]`, and a disconnect sends the partner
//! `SESSION_END <peer-id> <client_type>`. In a session every message is addressed `<peer-id>
//! <message>` and relayed as `<sender-id> <message>`, only between session partners; the
//! messages are `{"sdp": {"type": ..., "sdp": ...}}` and `{"ice": {"candidate": ...,
//! "sdpMLineIndex": ...}}`. `ROOM <room-id>` joins or creates a room (`ROOM_OK <member-ids>`,
//! members get `ROOM_PEER_JOINED` / `ROOM_PEER_LEFT <peer-id>`), where `ROOM_PEER_MSG
//! <peer-id> <message>` is relayed as `ROOM_PEER_MSG <sender-id> <message>`. Errors are
//! `ERROR <text>`.
//!
//! Close codes: 1000 a client removed or a stale socket a resume replaced; 1001 the server
//! going away, which a page reconnects from; 1002 a hello that is no hello; 1009 a hello past
//! `HELLO_MAX_BYTES`; 4000 a verdict at the handshake, with its reason, which a page does not
//! retry; 4001 a bad token, or a page a newer one superseded.

pub mod client;
pub mod lifecycle;
pub mod registry;
pub mod socket;
pub mod wire;

pub use client::{ClientEvent, Connector, RegistryConnector, SignalingClient, SignalingError};
pub use lifecycle::{DisplayGraphs, NoGraph, RECONNECT_GRACE, Release, StopGrace};
pub use registry::{
    HELD_LINES_MAX, OpenAuthority, PeerInfo, PeerRegistry, SESSION_HOLD, SessionAuthority,
    SignalingOptions, TokenPermissions,
};
pub use socket::{
    Inbound, OUTBOX_MAX_BYTES, Outbound, PeerSocket, SocketGone, SocketOutbox, socket,
};
pub use wire::{
    ClientType, HELLO_MAX_BYTES, Hello, HelloMetadata, IceBody, IceMessage, MAX_PAGE_SIZE, SdpBody,
    SdpMessage, ServerLine, SessionStart, hello_page_size, parse_hello, parse_server_line,
};

#[cfg(test)]
mod tests;
