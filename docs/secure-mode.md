---
title: Secure Mode
description: "Token authentication for orchestrated sessions: the master token, session tokens, and how they bind the streaming and API routes."
---

Selkies has two authentication modes. Without a master token it is in **legacy mode**: the server is either open or behind HTTP Basic authentication (`--enable-basic-auth`, the default, with an optional view-only password), and the browser presents the same credentials on every route. Setting a master token (`--master-token` / `SELKIES_MASTER_TOKEN`) switches the server into **secure mode**, where an orchestrator provisions per-session tokens and every client presents its token instead of a shared login. This page describes secure mode; the login settings are in the [Settings Reference](settings.md).

## Master Token and Session Tokens

The master token is the administrative credential. It is never sent to clients and authenticates two control-plane requests as an `Authorization: Bearer <master token>` header:

- `POST /api/tokens` replaces the session token table. The body is a JSON object keyed by token; each entry carries the client's `role` (`controller` or `viewer`), its gamepad `slot` (`1`-`4` or `null`), and optionally `mk_control: true` to hand keyboard and mouse authority to that one token (everyone else becomes read-only until the next table drops it). A viewer holding it becomes a read-write collaborator only while `--enable-collab` is on, which is the switch that keeps a deployment view-only whatever the table says; `cmd` and every settings-mutating message stay controller-only either way. No client streams until the first table arrives (the WebSocket transport holds the handshake, WebRTC refuses it). A new table is reconciled against the connected clients at once: a token that disappeared is disconnected, a changed role reconnects the client, a slot change is pushed live, and the input verdict is re-announced on both transports.
- `POST /api/switch` changes the streaming transport (when dual mode is enabled). The dashboards prompt for the master token when they need it.

Both control-plane requests also accept the master token in a `Selkies-Authorization: Bearer <master token>` header, tried after `Authorization`. A request carries a single `Authorization` header, so a caller behind a reverse proxy that demands HTTP Basic authentication must spend it on the Basic credentials; the named header lets both travel together:

```console
curl -u proxyuser:proxypass \
  -H "Selkies-Authorization: Bearer <master token>" \
  -X POST https://selkies.example/api/tokens -d @tokens.json
```

The master token is also accepted as a Bearer credential on every other API route below, so an operator can upload, download, or scrape metrics with it.

A session token is what a client holds. The client page is opened as `https://host/?token=<session token>` (a deployment subfolder goes in front as usual, and `#display2` and the other hashes still apply). The token's provisioned role is authoritative: a viewer token cannot drive input, own a display, open a second display, or upload, whatever the page asks for.

Tokens ride the URL, so the access log is written without query strings and the server's own logs never print them; a reverse proxy in front keeps its own request and referrer logs, which is worth a thought when tokens travel in URLs.

## How the Routes Are Bound

The static web client (`/`, its scripts and assets) is served without credentials: it is what presents the token. `/api/status` and `/api/health` stay open for probes. Everything else is bound to a token:

| Route | Credential |
|---|---|
| `/api/websockets` (WebSocket data transport) | `?token=` on the handshake |
| `/api/webrtc/signaling` (WebRTC signaling) | `client_token` in the HELLO message; the in-process server peer presents the master token |
| `/api/tokens`, `/api/switch` | master token only (`Authorization` Bearer, or the `Selkies-Authorization` fallback) |
| `/api/upload` | session or master token; viewer tokens are refused (403) |
| `/api/files/...` (listing and downloads) | session or master token |
| `/api/turn` (WebRTC ICE/TURN configuration) | session or master token |
| `/api/metrics` (when `--enable-metrics-http`) | session or master token |

An API request can present its session token in three ways, tried in this order: the `Authorization: Bearer <token>` header, a `?token=` query parameter, or the `selkies_token` cookie. Without a valid token the answer is `401` with `WWW-Authenticate: Bearer realm="Selkies Restricted"`; a valid token with insufficient rights (a viewer uploading) is `403`. Tokens are compared in constant time against the provisioned table; a revoked token stops working immediately.

The web client uses all three for you:

- Every script-driven call (the upload `POST`, the TURN fetch, the transport probes) sends the Bearer header.
- The file manager the dashboards open in an iframe is loaded as `/api/files/?token=…`, and the listing carries the token on its own links, so it keeps working where a cookie cannot follow (for example when Selkies is itself embedded cross-site).
- On load the client mirrors its token, URL-encoded, into a `selkies_token` session cookie scoped to `<subfolder>/api/` with `SameSite=Strict` (`Secure` over HTTPS), which covers anything the browser requests on its own, such as a download link or a listing opened by hand. A request that changes state on the cookie alone (an upload) is additionally held to the same-origin rule the mode switch applies. The cookie disappears when the browser closes; the next page load with a token overwrites it.

A Prometheus scrape job uses the master token as its bearer credential:

```yaml
scrape_configs:
  - job_name: selkies
    metrics_path: /api/metrics
    authorization:
      credentials: <master token>
    static_configs:
      - targets: ["selkies.example:8080"]
```

## Secure Mode with Basic Authentication

Both can be on. Basic authentication then guards the page load and anything that presents no token; a request that carries a valid session token is accepted on the API routes without Basic credentials (a script's own `Authorization` header replaces the browser's cached ones, so this is what lets the tokened page work behind a login). The WebSocket handshakes skip Basic in secure mode and rely on the token, since a browser cannot attach fresh credentials to an upgrade. The mode switch still takes the master token or the Basic login, not a session token.

## Origin Checks

Independent of the mode: `--allowed-origins` (`SELKIES_ALLOWED_ORIGINS`) is the cross-site WebSocket-hijacking guard on the streaming socket. Empty, the default, admits same-origin browsers and non-browser clients that send no `Origin` at all; a comma-separated list admits exactly those origins, which is what an embedding page on another host needs, and `*` admits any.

## Without a Master Token

Nothing above applies but the origin check. The routes are open, or Basic-gated when `--enable-basic-auth` is on: the main password authenticates a controller and the optional `--basic-auth-viewonly-password` a viewer, which is refused the same uploads and mode switches as a viewer token. Bearer headers and the token cookie are ignored, and the roles a client can take are the [sharing links](usage.md#session-sharing) instead of provisioned tokens.
