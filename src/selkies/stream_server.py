# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.


import os
import ssl
import hmac
import json
import html
import shutil
import base64
import pathlib
import asyncio
import logging
import urllib.parse
import tempfile
from aiohttp import web
from datetime import datetime
from prometheus_client import generate_latest
from typing import Optional, Dict, Any
try:
    # pyrefly: ignore[missing-import]
    import importlib_resources as importlib_resources  # pyright: ignore[reportMissingImports]
except ImportError:
    import importlib.resources as importlib_resources

from abc import ABCMeta, abstractmethod


logger = logging.getLogger("stream_server")


# Inlined header/footer HTML for the /api/files directory index.
FILE_INDEX_HEADER = """<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta http-equiv="x-ua-compatible" content="IE=edge">
    <title>Desktop Files</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        :root {
            --page-bg: #282c34;
            --text-color: #abb2bf;
            --header-color: #61dafb;
            --border-color: #3a3f47;
            --table-header-bg: #3a3f47;
            --table-row-hover-bg: #454b54;
            --link-color: #61dafb;
            --link-hover-color: #a4d9f5;
            --shadow-color: rgba(0, 0, 0, 0.5);

            --container-max-width: 960px;
            --font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            --border-radius: 8px;
        }
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }
        body {
            font-family: var(--font-family);
            background-color: var(--page-bg);
            color: var(--text-color);
            line-height: 1.6;
            padding-top: 20px;
            padding-bottom: 60px;
        }
        .page-container {
            max-width: var(--container-max-width);
            margin: 0 auto;
            padding: 0 20px;
            position: relative;
        }
        h1 {
            color: var(--header-color);
            font-size: 2em;
            font-weight: 300;
            margin-bottom: 15px;
            padding-bottom: 15px;
            border-bottom: 1px solid var(--border-color);
            padding-right: 50px;
        }
        hr {
            display: none;
        }
        table {
            width: 100%;
            border-collapse: collapse;
            margin-top: 25px;
            font-size: 0.95em;
            border-radius: var(--border-radius);
            overflow: hidden;
            box-shadow: 0 4px 10px var(--shadow-color);
        }
        thead {
            background-color: var(--table-header-bg);
        }
        th {
            color: var(--header-color);
            font-weight: 600;
            text-transform: uppercase;
            font-size: 0.85em;
            letter-spacing: 0.05em;
        }
        th, td {
            padding: 12px 15px;
            text-align: left;
            border-bottom: 1px solid var(--border-color);
        }
        tbody tr {
            transition: background-color 0.2s ease-in-out;
        }
        tbody tr:hover {
            background-color: var(--table-row-hover-bg);
        }
        tbody tr:last-child td {
            border-bottom: none;
        }
        td a {
            color: var(--link-color);
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            transition: color 0.2s ease-in-out;
        }
        td a:hover {
            color: var(--link-hover-color);
            text-decoration: underline;
        }
        th a {
            color: var(--link-color);
            text-decoration: none;
            transition: color 0.2s ease-in-out;
        }
        th a:hover {
            color: var(--link-hover-color);
            text-decoration: underline;
        }
        th a:visited {
            color: var(--link-color);
        }
        th a:visited:hover {
            color: var(--link-hover-color);
        }
        #reload-page-button {
            position: absolute;
            top: 0;
            right: 0;
            background-color: transparent;
            color: var(--text-color);
            border: none;
            border-radius: var(--border-radius);
            padding: 8px;
            cursor: pointer;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            transition: color 0.2s ease-in-out, transform 0.2s ease-in-out;
            z-index: 10;
        }
        #reload-page-button:hover {
            color: var(--link-hover-color);
            transform: rotate(45deg);
        }
        #reload-page-button svg {
            width: 20px;
            height: 20px;
            fill: currentColor;
        }
        td:nth-child(1) {
            word-break: break-all;
        }

        td:nth-child(2), th:nth-child(2) {
            white-space: nowrap;
            width: 180px;
        }

        td:nth-child(3), th:nth-child(3) {
            text-align: right;
            white-space: nowrap;
            width: 100px;
        }
        td a::before {
            display: inline-block;
            content: '';
            width: 1.1em;
            height: 1.1em;
            margin-right: 0.75em;
            vertical-align: middle;
            background-repeat: no-repeat;
            background-size: contain;
            background-position: center;
            flex-shrink: 0;
        }
        td a[href="../"]::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
        }
        td a[href$="/"]:not([href="../"])::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
        }
        td a:not([href$="/"]):not([href="../"])::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
        }
        footer {
            text-align: center;
            margin-top: 40px;
            padding-top: 20px;
            border-top: 1px solid var(--border-color);
            font-size: 0.85em;
            color: var(--text-color);
            opacity: 0.7;
        }
        footer p {
            margin: 0;
        }
        @media (max-width: 768px) {
            body {
                font-size: 14px;
                padding-top: 10px;
                padding-bottom: 40px;
            }
            .page-container {
                padding: 0 10px;
            }
            h1 {
                font-size: 1.6em;
                padding-right: 40px;
            }
            #reload-page-button {
                top: -2px;
                right: 0px;
            }
            #reload-page-button svg {
                width: 18px;
                height: 18px;
            }
            th, td {
                padding: 10px 8px;
            }
            table {
                display: block;
                overflow-x: auto;
                white-space: nowrap;
                -webkit-overflow-scrolling: touch;
            }
            th, td {
                white-space: nowrap;
            }
            td:nth-child(1) {
                min-width: 200px;
            }
            td:nth-child(2), th:nth-child(2) {
                min-width: 150px;
                width: auto;
            }
            td:nth-child(3), th:nth-child(3) {
                min-width: 80px;
                width: auto;
            }
        }
    </style>
</head>
<body>
    <div class="page-container">
        <button id="reload-page-button" title="Reload Page">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
                <path d="M17.65 6.35C16.2 4.9 14.21 4 12 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08c-.82 2.33-3.04 4-5.65 4-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/>
            </svg>
        </button>

        <h1>
"""

FILE_INDEX_FOOTER = """    </div> <!-- closes .page-container -->
    <footer>
        <p>&copy; <script>document.write(new Date().getFullYear())</script> Selkies.</p>
    </footer>

    <script>
        const reloadButton = document.getElementById('reload-page-button');
        if (reloadButton) {
            reloadButton.addEventListener('click', function() {
                window.location.reload();
            });
        }

        function processDirectoryListing() {
            const webPathPrefix = '/api/files/';
            let diskPathPrefix = '';
            const injectedPathPrefix = window.__SELKIES_INJECTED_PATH_PREFIX__ || '';
            if (injectedPathPrefix) {
                diskPathPrefix = injectedPathPrefix;
            } else {
                diskPathPrefix = '';
            }
            const h1 = document.querySelector('h1');

            if (h1) {
                const originalText = h1.textContent;
                const newText = originalText.replace(webPathPrefix, diskPathPrefix);
                h1.textContent = newText;
            }

            const isAtRoot = window.location.pathname === webPathPrefix;
            if (isAtRoot) {
                const parentLink = document.querySelector('table#list td.link a[href^="../"]');
                if (parentLink && parentLink.textContent.trim() === "Parent directory/") {
                    const parentRow = parentLink.closest('tr');
                    if (parentRow) {
                        parentRow.style.display = 'none';
                    }
                }
            }
        }

        let attempts = 0;
        const maxAttempts = 20;
        const intervalId = setInterval(function() {
            attempts++;
            const h1 = document.querySelector('h1');
            const table = document.getElementById('list');

            if (h1 && table) {
                clearInterval(intervalId);
                processDirectoryListing();
            } else if (attempts >= maxAttempts) {
                clearInterval(intervalId);
                processDirectoryListing();
            }
        }, 100);

    </script>
</body>
</html>
"""


class BaseStreamingService(metaclass=ABCMeta):
    def __init__(self, name: str):
        self.mode = name

    @abstractmethod
    async def start(self):
        """Logic to setup and start the resource loops."""

    @abstractmethod
    async def stop(self):
        """Logic to cleanup resources and stop loops."""

    @abstractmethod
    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher):
        """Service registers its absolute paths directly on the main router."""
        pass


class CentralizedStreamServer:
    def __init__(self, settings, services: Optional[Dict[str, BaseStreamingService]] = None):
        self.settings = settings
        self.services = services or {}
        self.current_mode: Optional[str] = None
        self.lock = asyncio.Lock()
        self.active_task: Optional[asyncio.Task] = None

        self.app: Optional[web.Application] = None
        self.runner: Optional[web.AppRunner] = None
        self.site: Optional[web.TCPSite] = None
        self.cert_watcher: Optional[asyncio.Task] = None
        self.ssl_context: Optional[ssl.SSLContext] = None
        self.static_fs_path: str = ""
        self.upload_dir = pathlib.Path(
            os.path.expanduser(self.settings.file_manager_path)
        ).resolve()
        self.web_files_ctx = None

        self._clients_present = False
        self._client_hook_tasks = set()

        # Wayland deployments: bring the compositor socket up now, before any capture
        # or session app starts, so early-launched apps find WAYLAND_DISPLAY. All
        # configuration flows from settings; pixelflux reads no Selkies env.
        if bool(self.settings.wayland[0]):
            try:
                from pixelflux import ensure_wayland_display
                ensure_wayland_display(
                    width=int(self.settings.manual_width or 0),
                    height=int(self.settings.manual_height or 0),
                    render_node=self.settings.render_dri or "",
                    auto_gpu=str(self.settings.auto_gpu or ""),
                    cursor_size=int(self.settings.cursor_size),
                )
            except ImportError:
                logger.warning("pixelflux unavailable; Wayland display not initialized.")

        # Constants
        self.STREAMING_MODE_WEBRTC = "webrtc"
        self.STREAMING_MODE_WEBSOCKETS = "websockets"
        self.STATIC_CONTENT_PATH = "selkies.selkies_web"
        self.MIME_TYPES = {
            "html": "text/html",
            "js": "text/javascript",
            "css": "text/css",
            "json": "application/json",
            "png": "image/png",
            "jpg": "image/jpeg",
            "ico": "image/x-icon",
            "svg": "image/svg+xml",
        }

    def set_clients_present(self, present: bool):
        """Both modes report client presence here; run the configured hook
        command when the first client connects / the last one disconnects."""
        if present == self._clients_present:
            return
        self._clients_present = present
        cmd = (self.settings.run_after_connect if present
               else self.settings.run_after_disconnect)
        if cmd:
            # Hold a strong reference: the loop only keeps weak refs to tasks.
            task = asyncio.create_task(self._run_client_hook(cmd, present))
            self._client_hook_tasks.add(task)
            task.add_done_callback(self._client_hook_tasks.discard)

    async def _run_client_hook(self, cmd: str, present: bool):
        name = "run_after_connect" if present else "run_after_disconnect"
        try:
            proc = await asyncio.create_subprocess_shell(cmd)
            returncode = await proc.wait()
            if returncode != 0:
                logger.warning(f"{name} command exited with status {returncode}: {cmd}")
        except OSError as e:
            logger.error(f"Failed to run {name} command {cmd!r}: {e}")

    def _b64_decode(self, data: str) -> str:
        return base64.b64decode(data).decode("utf-8")

    def _get_https_certs(self):
        https_cert = getattr(self.settings, "https_cert", None)
        https_key = getattr(self.settings, "https_key", None)
        cert_pem = (
            os.path.abspath(https_cert)
            if https_cert and os.path.isfile(https_cert)
            else None
        )
        key_pem = (
            os.path.abspath(https_key)
            if https_key and os.path.isfile(https_key)
            else None
        )
        return cert_pem, key_pem

    def _create_ssl_context(self):
        enable_https = getattr(self.settings, "enable_https", None)
        if not enable_https or not enable_https[0]:
            return None

        cert_pem, key_pem = self._get_https_certs()
        if not cert_pem:
            raise FileNotFoundError(
                f"HTTPS enabled but certificate file not found at "
                f"{getattr(self.settings, 'https_cert', '<unset>')}"
            )

        logger.info(
            "Creating TLS context with certificate=%s key=%s", cert_pem, key_pem
        )
        sslctx = ssl.create_default_context(purpose=ssl.Purpose.CLIENT_AUTH)
        sslctx.check_hostname = False
        sslctx.verify_mode = ssl.CERT_NONE
        try:
            sslctx.load_cert_chain(cert_pem, keyfile=key_pem if key_pem else None)
        except Exception:
            logger.error(
                "Certificate or private key file not found or incorrect. "
                'To use a self-signed certificate, install the package "ssl-cert" '
                'and add the group "ssl-cert" to your user in Debian-based '
                "distributions, or generate a new certificate with root using: "
                "openssl req -x509 -newkey rsa:4096 "
                "-keyout /etc/ssl/private/ssl-cert-snakeoil.key "
                "-out /etc/ssl/certs/ssl-cert-snakeoil.pem -days 3650 -nodes "
                '-subj "/CN=localhost"'
            )
            raise
        return sslctx

    def _get_cert_mtime(self) -> float:
        """Return the most recent modification time of the cert and key files."""
        cert_pem, key_pem = self._get_https_certs()
        if not cert_pem:
            return 0.0
        try:
            cert_mtime = os.stat(cert_pem).st_mtime
            key_mtime = os.stat(key_pem).st_mtime if key_pem else 0.0
            return max(cert_mtime, key_mtime)
        except OSError:
            return 0.0

    async def _watch_and_reload_certs(self):
        """Background task: periodically checks whether the TLS certificate or
        private key files have changed on disk.
        """
        reload_interval = getattr(self.settings, "cert_reload_interval", 30)
        if reload_interval <= 0:
            logger.info("Automatic certificate reloading is disabled (interval=0)")
            return

        current_site = self.site
        last_mtime = self._get_cert_mtime()
        logger.info(
            "Certificate reload watcher started (interval=%ds, initial mtime=%.0f)",
            reload_interval,
            last_mtime,
        )

        while True:
            await asyncio.sleep(reload_interval)
            try:
                new_mtime = self._get_cert_mtime()
            except Exception as exc:
                logger.warning("Could not stat cert/key files: %s", exc)
                continue

            if new_mtime <= last_mtime:
                continue

            logger.info(
                "Certificate change detected (mtime %.0f -> %.0f), reloading…",
                last_mtime,
                new_mtime,
            )
            # last_mtime advances only once the new site is up (below), so a
            # failed reload is retried on the next poll.

            # Build the new SSL context *before* tearing down the old site so that
            # a bad certificate never takes the server offline.
            try:
                new_ssl_context = self._create_ssl_context()
            except Exception as exc:
                logger.error(
                    "Failed to create new SSL context, keeping old certificate: %s",
                    exc,
                )
                continue

            if new_ssl_context is None:
                logger.error(
                    "New SSL context is None (HTTPS disabled?), keeping old site."
                )
                continue

            try:
                await current_site.stop()
                logger.info("Old TCPSite stopped.")
            except Exception as exc:
                logger.warning("Error stopping old TCPSite: %s", exc)

            try:
                new_site = web.TCPSite(
                    self.runner,
                    host=self.settings.addr,
                    port=self.settings.port,
                    ssl_context=new_ssl_context,
                )
                await new_site.start()
                current_site = new_site
                self.site = new_site
                last_mtime = new_mtime
                logger.info(
                    "New TCPSite started with reloaded certificates on %s:%s",
                    self.settings.addr,
                    self.settings.port,
                )
            except Exception as exc:
                logger.critical(
                    "Failed to start new TCPSite: %s. HTTPS server may be down; "
                    "will retry on the next certificate poll.",
                    exc,
                )

    @staticmethod
    def _check_master_token(auth_header, master_token) -> bool:
        """Timing-safe check of a `Bearer <master_token>` header (UTF-8 bytes so non-ASCII is safe)."""
        if not auth_header or not auth_header.startswith("Bearer "):
            return False
        parts = auth_header.split()
        if len(parts) < 2:
            return False
        return hmac.compare_digest(
            parts[1].encode("utf-8"), str(master_token).encode("utf-8")
        )

    @staticmethod
    def _is_ws_origin_allowed(request: web.Request, settings) -> bool:
        """Whether a WebSocket upgrade's Origin is permitted.

        Empty allowed_origins means same-origin only (plus non-browser clients that
        send no Origin); '*' allows any; otherwise the Origin must be listed or match
        the Host header.
        """
        origin = request.headers.get("Origin")
        if not origin:
            return True  # native/non-browser clients omit Origin
        allowed = {
            o.strip()
            for o in (getattr(settings, "allowed_origins", "") or "").split(",")
            if o.strip()
        }
        if "*" in allowed or origin in allowed:
            return True
        host = request.headers.get("Host")
        if host:
            try:
                origin_parts = urllib.parse.urlsplit(origin)
                if origin_parts.netloc == host:
                    return True
                # Reverse proxies routinely forward Host without the port (e.g.
                # nginx's 'proxy_set_header Host $host' — this project's own
                # bundled nginx config does), while a browser Origin carries any
                # non-default port. A strict netloc comparison therefore rejects
                # every same-origin connection reached via an explicit port
                # (like the standard :6080 mapping). When the forwarded Host has
                # no port to compare, fall back to comparing hostnames.
                host_parts = urllib.parse.urlsplit("//" + host)
                if (
                    host_parts.port is None
                    and origin_parts.hostname
                    and origin_parts.hostname == host_parts.hostname
                ):
                    return True
            except ValueError:
                pass
        return False

    @web.middleware
    async def _auth_middleware(self, request: web.Request, handler):
        """
        Global Guard: Handles Basic Auth for the entire server.
        """
        settings = request.app["settings"]
        auth_header = request.headers.get("Authorization")
        path = request.path
        # Reject cross-site WebSocket upgrades: a page from another origin can open our
        # WS even where CORS blocks reading XHR responses. Non-browser clients send no
        # Origin and pass.
        if request.headers.get("Upgrade", "").lower() == "websocket":
            if not self._is_ws_origin_allowed(request, settings):
                logger.warning(
                    "Rejected WebSocket upgrade from disallowed Origin: %r",
                    request.headers.get("Origin", ""),
                )
                return web.Response(status=403, text="Forbidden origin")
        # Match the exact route, not a suffix, so /foo/tokens isn't treated as control-plane.
        api_prefix = (
            ("/" + settings.subfolder.strip("/"))
            if settings.subfolder
            else ""
        )
        # Health/liveness endpoints stay open so k8s/LB probes reach them without credentials.
        if path in (f"{api_prefix}/api/status", f"{api_prefix}/api/health"):
            return await handler(request)
        token_path = path == f"{api_prefix}/api/tokens"
        # Gate /tokens on the master token whenever set, independent of streaming mode.
        if settings.master_token and token_path:
            if not self._check_master_token(auth_header, settings.master_token):
                return web.Response(status=401, text="Unauthorized")
            return await handler(request)

        # /api/switch (when master_token set): accept a Bearer master token, else fall
        # through to the Basic check if Basic Auth is on, else 401.
        is_control_path = path == f"{api_prefix}/api/switch"
        if settings.master_token and is_control_path:
            if self._check_master_token(auth_header, settings.master_token):
                return await handler(request)
            if not settings.enable_basic_auth[0]:
                return web.Response(status=401, text="Unauthorized")

        # Authentication flow for regular Selkies deployment
        if not settings.enable_basic_auth[0]:
            logger.debug("Basic auth not enabled, forwarding to routers")
            return await handler(request)
        is_ws_upgrade = request.headers.get("Upgrade", "").lower() == "websocket"
        if is_ws_upgrade and settings.master_token:
            # A browser cannot attach fresh Basic credentials to a WebSocket
            # handshake, and these paths carry their own token gate (enforced in
            # the ws handlers) whenever a master token is configured -- Basic
            # adds no protection there, only an undebuggable 401.
            return await handler(request)
        if not auth_header or not auth_header.startswith("Basic "):
            if is_ws_upgrade:
                # No challenge/retry exists for a WS handshake: without this log the
                # rejection is invisible on both ends (the browser only reports a
                # generic connection failure).
                logger.warning(
                    "Rejected WebSocket upgrade from %s: basic auth is enabled and the "
                    "handshake carried no Authorization header (browsers only attach "
                    "cached credentials; set a master token or disable basic auth for "
                    "browser clients behind proxies).",
                    request.remote,
                )
            return web.Response(
                status=401,
                headers={
                    "WWW-Authenticate": 'Basic realm="Selkies Restricted", charset="UTF-8"'
                },
                text="Authorization Required",
            )
        try:
            auth_decoded = self._b64_decode(auth_header[6:])
            if ":" not in auth_decoded:
                return web.Response(status=401, text="Invalid Credentials")
            username, password = auth_decoded.split(":", 1)
            # Compare as UTF-8 bytes; hmac.compare_digest rejects non-ASCII str.
            is_valid = hmac.compare_digest(
                username.encode("utf-8"), str(settings.basic_auth_user).encode("utf-8")
            ) and hmac.compare_digest(
                password.encode("utf-8"), str(settings.basic_auth_password).encode("utf-8")
            )
            if not is_valid:
                logger.warning(
                    f"Invalid credentials provided for user: {settings.basic_auth_user}"
                )
                return web.Response(status=401, text="Invalid Credentials")
        except Exception:
            return web.Response(status=401, text="Invalid Credentials")
        return await handler(request)

    def _update_auth_credentials(self):
        try:
            if self.settings.enable_basic_auth[0] and self.settings.basic_auth_password == "mypasswd":
                logger.warning(
                    "Basic Auth is enabled with the well-known default password 'mypasswd'. "
                    "Set a strong password via --basic_auth_password, SELKIES_BASIC_AUTH_PASSWORD, or the PASSWORD env var."
                )
        except Exception:
            pass

    async def switch_to_mode(self, mode_name: str):
        """
        Orchestrates the transition between services.
        """
        if mode_name not in self.services:
            raise ValueError(f"Service {mode_name} not found")

        async with self.lock:
            if self.current_mode == mode_name:
                logger.info(f"Mode {mode_name} is already active.")
                return

            await self._stop_service()
            logger.info(f"Starting service: {mode_name}")
            service = self.services[mode_name]
            task = asyncio.create_task(service.start())
            self.active_task = task
            self.current_mode = mode_name

            # Clear the stale mode if the service dies unexpectedly, and retrieve
            # the exception so asyncio doesn't log it as never-retrieved.
            def _on_service_done(finished: asyncio.Task, mode=mode_name):
                if finished.cancelled():
                    return
                exc = finished.exception()
                if exc is not None:
                    logger.error(f"Service '{mode}' terminated unexpectedly: {exc!r}")
                    if self.active_task is finished:
                        self.current_mode = None
                        self.active_task = None

            task.add_done_callback(_on_service_done)

    async def _stop_service(self):
        if not self.current_mode:
            return
        logger.info(f"Stopping service: {self.current_mode}")

        await self.services[self.current_mode].stop()
        if self.active_task:
            try:
                # Let the service teardown finish (can include ~2s gamepad-close waits)
                # before a forced cancel that would leak resources.
                await asyncio.wait_for(self.active_task, timeout=15)
            except asyncio.TimeoutError:
                logger.warning(
                    f"Timeout while stopping '{self.current_mode}'. Cancelling task."
                )
                self.active_task.cancel()
                try:
                    await self.active_task
                except asyncio.CancelledError:
                    logger.info(
                        f"Task cancelled after timeout for '{self.current_mode}'."
                    )
                except Exception as e:
                    logger.warning(f"Service task raised during forced stop: {e!r}")
        # Clear stale references so a stopped/dead mode is never reported active.
        self.current_mode = None
        self.active_task = None

    def _get_status(self):
        return {
            "current_mode": self.current_mode,
            "available_modes": list(self.services.keys()),
            # Surfaced so the dashboard can render the WebSocket/WebRTC toggle
            # from this early, transport-independent probe. serverSettings (which
            # also carries enable_dual_mode) only arrives once a stream connects,
            # so without this a WebRTC session that never comes up would strand
            # the user with no way back to WebSockets.
            "enable_dual_mode": bool(
                getattr(self.settings, "enable_dual_mode", (False,))[0]
            ),
        }

    async def handle_switch(self, request: web.Request) -> web.Response:
        dual_mode = getattr(self.settings, "enable_dual_mode", (False,))[0]
        if not dual_mode:
            return web.json_response(
                {"status": "error", "message": "Dual streaming mode disabled"},
                status=403,
            )

        try:
            data = await request.json()
            if not isinstance(data, dict):
                raise ValueError("Request body must be a JSON object")
            target_mode = data.get("mode")
            await self.switch_to_mode(target_mode)
            return web.json_response({"status": "success", "mode": target_mode})
        except Exception as e:
            return web.json_response({"status": "error", "message": str(e)}, status=400)

    async def handle_upload(self, request: web.Request) -> web.Response:
        """Stream a client file upload to the file-manager directory over HTTP.

        Available in every streaming mode and not bounded by the data-channel /
        WebSocket per-message size, so it saturates the link where the per-chunk
        SCTP path cannot. The destination path (relative to the file-manager
        root) arrives URL-encoded in the X-Upload-Path header; the body streams
        straight to disk on the executor, so the event loop keeps serving the
        stream during the transfer. Path safety mirrors the data-channel path:
        no traversal outside the root, and O_NOFOLLOW blocks a planted symlink.
        """
        settings = request.app["settings"]
        if "upload" not in settings.file_transfers:
            return web.json_response({"status": "error", "message": "uploads disabled"}, status=403)
        root = getattr(settings, "file_manager_path", "") or ""
        if not root:
            return web.json_response({"status": "error", "message": "uploads disabled"}, status=403)
        root = os.path.expanduser(root)
        rel = urllib.parse.unquote(request.headers.get("X-Upload-Path", "") or "")
        sane = os.path.normpath(rel.strip("/\\"))
        parts = [c for c in sane.split(os.sep) if c and c != "."]
        if not parts or ".." in parts:
            return web.json_response({"status": "error", "message": "invalid upload path"}, status=400)
        dest = os.path.join(root, *parts)
        real_root = os.path.realpath(root)
        parent = os.path.realpath(os.path.dirname(dest))
        try:
            within = os.path.commonpath([real_root, parent]) == real_root
        except ValueError:
            within = False
        if not within:
            return web.json_response({"status": "error", "message": "path escape rejected"}, status=400)
        try:
            os.makedirs(parent, exist_ok=True)
        except OSError as e:
            return web.json_response({"status": "error", "message": f"mkdir failed: {e}"}, status=500)
        declared = request.content_length
        loop = asyncio.get_running_loop()
        fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o644)
        fh = os.fdopen(fd, "wb")
        written = 0
        try:
            async for chunk in request.content.iter_chunked(1 << 20):
                if declared is not None and written + len(chunk) > declared:
                    raise ValueError("body exceeds declared Content-Length")
                await loop.run_in_executor(None, fh.write, chunk)
                written += len(chunk)
            await loop.run_in_executor(None, fh.close)
        except Exception as e:
            try:
                fh.close()
            except Exception:
                pass
            try:
                os.remove(dest)
            except OSError:
                pass
            return web.json_response({"status": "error", "message": str(e)}, status=400)
        logger.info(f"HTTP upload finished: {dest} ({written} bytes)")
        return web.json_response({"status": "success", "bytes": written})

    async def handle_status(self, _: web.Request) -> web.Response:
        status = self._get_status()
        return web.json_response(status)

    async def handle_health(self, _) -> web.Response:
        return web.Response(text="OK")

    async def handle_metrics(self, request: web.Request) -> web.Response:
        """Prometheus exposition of the process-global registry."""
        data = await asyncio.to_thread(generate_latest)
        return web.Response(
            body=data,
            content_type='text/plain; version=1.0.0',
            headers={
                'Cache-Control': 'no-cache, no-store, must-revalidate',
                'Pragma': 'no-cache',
                'Expires': '0'
            }
        )

    async def _get_static_content_path(self) -> str:
        web_path = ""

        web_root = getattr(self.settings, "web_root", "")
        if web_root:
            web_path = os.path.expanduser(web_root)
            if os.path.isdir(web_path) and os.path.isfile(os.path.join(web_path, "index.html")):
                logger.info(f"Using custom web_root directory: {web_path}")
                return web_path
            logger.warning(f"web_root directory {web_path} not found or missing index.html")

        logger.info("Defaulting to packaged web files.")
        try:
            package_path = importlib_resources.files(self.STATIC_CONTENT_PATH)
            # Create a temporary directory and copy contents
            self.web_files_ctx = tempfile.TemporaryDirectory(prefix="selkies_web")
            temp_path = pathlib.Path(self.web_files_ctx.name)
            await asyncio.to_thread(self._copy_traversable, package_path, temp_path)

            if (temp_path / "index.html").exists():
                logger.info(f"Using extracted package path from temp dir: {temp_path}")
                return str(temp_path)
            else:
                logger.warning("Packaged web content missing index.html")
                self.web_files_ctx.cleanup()
        except Exception as e:
            logger.error(f"Failed to extract packaged web files: {e}")
            # Guard cleanup: web_files_ctx may be unset if extraction failed before
            # it was assigned. An unguarded cleanup() would raise and mask the real
            # error logged above.
            if self.web_files_ctx is not None:
                try:
                    self.web_files_ctx.cleanup()
                except Exception:
                    pass

        return ""

    def _copy_traversable(self, src: Any, dst: pathlib.Path):
        """Recursively copy a Traversable (file or directory) to a filesystem path."""
        if src.is_file():
            with src.open('rb') as f_src, open(dst, 'wb') as f_dst:
                shutil.copyfileobj(f_src, f_dst)
        elif src.is_dir():
            dst.mkdir(exist_ok=True)
            for child in src.iterdir():
                self._copy_traversable(child, dst / child.name)

    async def fancy_index_handler(self, request: web.Request):
        # The index exists solely to download files, so the listing is gated
        # with the bytes.
        if "download" not in self.settings.file_transfers:
            return web.Response(status=403, text="Forbidden: downloads disabled")
        rel_path = request.match_info.get("path", "").lstrip("/")
        full_path = (self.upload_dir / rel_path).resolve()

        try:
            full_path.relative_to(self.upload_dir)
        except ValueError:
            return web.Response(
                status=403, text="Forbidden: Directory Traversal detected"
            )

        if not full_path.exists():
            return web.Response(status=404, text="Not Found")

        if full_path.is_file():
            return web.FileResponse(full_path)

        # A directory URL must end in "/" so its relative links (../, name/) resolve
        # one level down instead of against the parent.
        if not request.path.endswith("/"):
            location = request.path + "/"
            if request.query_string:
                location += "?" + request.query_string
            raise web.HTTPMovedPermanently(location)

        # If it's a directory, generate the "FancyIndex" HTML
        items = []
        # Add parent directory link if not at root
        if full_path != self.upload_dir:
            items.append({"name": "../", "size": "-", "mtime": "-", "is_dir": True})

        try:
            with os.scandir(full_path) as it:
                for entry in it:
                    try:
                        stats = entry.stat()
                        mtime = datetime.fromtimestamp(stats.st_mtime).strftime(
                            "%Y-%m-%d %H:%M:%S"
                        )
                        is_dir = entry.is_dir()
                    except OSError as e:
                        # Entries can vanish or be unstat-able (broken symlinks, races).
                        logger.warning(f"Skipping unreadable directory entry {entry.name!r}: {e}")
                        continue
                    items.append(
                        {
                            "name": entry.name + ("/" if is_dir else ""),
                            "size": f"{stats.st_size / 1024:.1f} KB"
                            if not is_dir
                            else "-",
                            "mtime": mtime,
                            "is_dir": is_dir,
                        }
                    )
        except PermissionError:
            return web.Response(status=403, text="Permission Denied")

        # Sort: Directories first, then alphabetically
        items.sort(key=lambda x: (not x["is_dir"], x["name"].lower()))

        # Build Table Rows with proper escaping
        rows = ""
        for item in items:
            escaped_name = html.escape(item["name"])
            escaped_mtime = html.escape(item["mtime"])
            escaped_size = html.escape(item["size"])
            rows += f"""
            <tr>
                <td><a href="{urllib.parse.quote(item["name"])}">{escaped_name}</a></td>
                <td>{escaped_mtime}</td>
                <td>{escaped_size}</td>
            </tr>"""

        # Inject the current path into the H1 (which is left open in the header)
        escaped_rel_path = html.escape(rel_path)
        current_display_path = f"/api/files/{escaped_rel_path}"

        # JavaScript string escaping for upload_dir
        js_safe_upload_dir = json.dumps(str(self.upload_dir))
        path_injection = f"<script>window.__SELKIES_INJECTED_PATH_PREFIX__ = {js_safe_upload_dir};</script>"

        html_content = f"""
        {FILE_INDEX_HEADER}
        {current_display_path}</h1>
        {path_injection}
        <table id="list">
            <thead>
                <tr>
                    <th>File Name</th>
                    <th>Date</th>
                    <th>Size</th>
                </tr>
            </thead>
            <tbody>
                {rows}
            </tbody>
        </table>
        {FILE_INDEX_FOOTER}
        """

        return web.Response(text=html_content, content_type="text/html")

    async def initialize_app(self):
        """Initialize the web application with routes and middleware."""
        self._update_auth_credentials()

        # Create web application with auth middleware
        self.app = web.Application(middlewares=[self._auth_middleware])
        self.app["supervisor"] = self
        self.app["settings"] = self.settings

        # Register API routes
        api_prefix = (
            ("/" + self.settings.subfolder.strip("/"))
            if self.settings.subfolder
            else ""
        )
        if api_prefix:
            logger.info(f"Prepending api prefix: {api_prefix!r} to router handlers")

        # All control-plane endpoints live under /api so a fronting proxy can
        # route them — present and future — with a single rule.
        routes = [
            web.get(f"{api_prefix}/api/status", self.handle_status),
            web.get(f"{api_prefix}/api/health", self.handle_health),
            web.post(f"{api_prefix}/api/switch", self.handle_switch),
            web.post(f"{api_prefix}/api/upload", self.handle_upload),
        ]
        # The Prometheus registry is process-global, so one mode-agnostic
        # endpoint serves both streaming modes.
        if self.settings.enable_metrics_http[0]:
            routes.append(web.get(f"{api_prefix}/api/metrics", self.handle_metrics))
        self.app.add_routes(routes)

        # Register service routes
        for service in self.services.values():
            service.register_routes(api_prefix, self.app.router)

        self.static_fs_path = await self._get_static_content_path()
        if self.static_fs_path:
            async def index_handler(_):
                return web.FileResponse(os.path.join(self.static_fs_path, "index.html"))

            self.app.router.add_get(
                f"{api_prefix}/api/files/{{path:.*}}", self.fancy_index_handler
            )
            self.app.router.add_get(f"{api_prefix}/", index_handler)
            self.app.router.add_static(
                f"{api_prefix}/", self.static_fs_path, name="static"
            )
        else:
            logger.warning("Unable to find web content, skipping web routers handlers")
        return self.app

    async def start_server(self):
        """Start the HTTP/HTTPS server."""
        if not self.app:
            await self.initialize_app()

        # Setup SSL if enabled
        https = getattr(self.settings, "enable_https", (False,))[0]
        if https:
            try:
                self.ssl_context = self._create_ssl_context()
            except Exception as exc:
                logger.error("Failed to create SSL context at startup: %s", exc)
                raise

        # Create and start runner
        self.runner = web.AppRunner(self.app)
        await self.runner.setup()

        self.site = web.TCPSite(
            self.runner,
            host=self.settings.addr,
            port=self.settings.port,
            ssl_context=self.ssl_context,
        )

        logger.info(
            f"Selkies server running on "
            f"{'https' if https else 'http'}://{self.settings.addr}:{self.settings.port}"
        )
        await self.site.start()

        # Start certificate watcher if HTTPS is enabled
        if https:
            self.cert_watcher = asyncio.create_task(self._watch_and_reload_certs())

    async def stop_server(self):
        """Stop the server gracefully."""
        if self.cert_watcher and not self.cert_watcher.done():
            self.cert_watcher.cancel()
            try:
                await self.cert_watcher
            except asyncio.CancelledError:
                pass

        await self._stop_service()

        if self.web_files_ctx:
                self.web_files_ctx.cleanup()
        if self.site:
            await self.site.stop()
        if self.runner:
            await self.runner.cleanup()
            logger.info("Server cleanup complete.")

    async def run(self):
        """Main server loop - starts server and runs forever."""
        try:
            await self.start_server()
            await asyncio.Future()
        except asyncio.CancelledError:
            logger.info("Shutdown signal received...")
        finally:
            await self.stop_server()

    def register_service(self, name: str, service: BaseStreamingService):
        """Register a new streaming service."""
        self.services[name] = service
