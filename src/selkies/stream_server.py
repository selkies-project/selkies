# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.


import os
import ssl
import hmac
import json
import html
import base64
import pathlib
import asyncio
import logging
import urllib.parse
import aiofiles
from aiohttp import web
from datetime import datetime
from typing import Optional, Dict
from abc import ABCMeta, abstractmethod


logger = logging.getLogger("stream_server")


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
    def register_routes(self, prefix: str, main_router: web.UrlDispatcher):
        """Service registers its absolute paths directly on the main router."""
        pass


class CentralisedStreamServer:
    def __init__(self, settings, services: Dict[str, BaseStreamingService] = None):
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
            os.path.expanduser(self.settings.upload_dir)
        ).resolve()
        self.header_html = "nginx/header.html"
        self.footer_html = "nginx/footer.html"

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

    def _get_static_content_path(self) -> str:
        web_path = ""
        try:
            import importlib.util

            spec = importlib.util.find_spec(self.STATIC_CONTENT_PATH)
            if spec and spec.submodule_search_locations:
                # For namespace packages or directory packages
                web_path = spec.submodule_search_locations[0]
            else:
                raise RuntimeError(
                    f"Could not find static content package: {self.STATIC_CONTENT_PATH}"
                )
        except RuntimeError as e:
            logger.warning(f"{e}: checking web_root path")
            web_root = getattr(self.settings, "web_root", "")
            if web_root:
                web_path = os.path.expanduser(web_root)
                if os.path.isdir(web_path) and os.path.isfile(
                    os.path.join(web_path, "index.html")
                ):
                    logger.info(f"Using web_root directory: {web_path}")
                    return web_path
                logger.error(
                    f"web_root directory {web_path} not found or missing index.html"
                )
                return ""
        return web_path

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
            last_mtime = new_mtime

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
                logger.info(
                    "New TCPSite started with reloaded certificates on %s:%s",
                    self.settings.addr,
                    self.settings.port,
                )
            except Exception as exc:
                logger.critical(
                    "Failed to start new TCPSite: %s. HTTPS server may be down!",
                    exc,
                )

    @web.middleware
    async def _auth_middleware(self, request: web.Request, handler):
        """
        Global Guard: Handles Basic Auth for the entire server.
        """
        settings = request.app["settings"]
        supervisor = request.app["supervisor"]
        mode = supervisor.current_mode
        auth_header = request.headers.get("Authorization")
        token_path = request.path.endswith("/tokens")

        if (
            mode == self.STREAMING_MODE_WEBSOCKETS
            and settings.master_token
            and token_path
        ):
            if (
                not auth_header
                or not auth_header.startswith("Bearer ")
                or len(auth_header.split()) < 2
                or auth_header.split()[1] != settings.master_token
            ):
                return web.Response(status=401, text="Unauthorized")
            return await handler(request)

        # Authentication flow for regular Selkies deployment
        if not settings.enable_basic_auth[0]:
            logger.debug("Basic auth not enabled, forwarding to routers")
            return await handler(request)
        if not auth_header or not auth_header.startswith("Basic "):
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
            is_valid = hmac.compare_digest(
                username, settings.basic_auth_user
            ) and hmac.compare_digest(password, settings.basic_auth_password)
            if not is_valid:
                logger.warning(
                    f"Invalid credentials provided for user: {settings.basic_auth_user}"
                )
                return web.Response(status=401, text="Invalid Credentials")
        except Exception:
            return web.Response(status=401, text="Invalid Credentials")
        return await handler(request)

    def _update_auth_credentials(self):
        custom_user = os.environ.get("CUSTOM_USER", None)
        password = os.environ.get("PASSWORD", None)

        # Update basic auth credentials to Sealskin provided ones
        if custom_user and password:
            setattr(self.settings, "basic_auth_user", custom_user)
            setattr(self.settings, "basic_auth_password", password)

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
            self.active_task = asyncio.create_task(service.start())
            self.current_mode = mode_name

    async def _stop_service(self):
        if not self.current_mode:
            return
        logger.info(f"Stopping service: {self.current_mode}")

        await self.services[self.current_mode].stop()
        if self.active_task:
            try:
                await asyncio.wait_for(self.active_task, timeout=5)
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

    def get_status(self):
        return {
            "active_mode": self.current_mode,
            "available_modes": list(self.services.keys()),
        }

    async def handle_switch(self, request: web.Request) -> web.Response:
        dual_mode = getattr(self.settings, "enable_dual_mode", (False,))[0]
        if not dual_mode:
            return web.json_response(
                {"status": "error", "message": "Dual streaming mode disabled"},
                status=403,
            )

        data = await request.json()
        target_mode = data.get("mode")
        try:
            await self.switch_to_mode(target_mode)
            return web.json_response({"status": "success", "mode": target_mode})
        except Exception as e:
            return web.json_response({"status": "error", "message": str(e)}, status=400)

    async def handle_status(self, request: web.Request) -> web.Response:
        status = self.get_status()
        return web.json_response(status)

    async def handle_health(self, _) -> web.Response:
        return web.Response(text="OK")

    async def fancy_index_handler(self, request: web.Request):
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

        # If it's a directory, generate the "FancyIndex" HTML
        items = []
        # Add parent directory link if not at root
        if full_path != self.upload_dir:
            items.append({"name": "../", "size": "-", "mtime": "-", "is_dir": True})

        try:
            with os.scandir(full_path) as it:
                for entry in it:
                    stats = entry.stat()
                    mtime = datetime.fromtimestamp(stats.st_mtime).strftime(
                        "%Y-%m-%d %H:%M:%S"
                    )
                    is_dir = entry.is_dir()
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

        header_path = os.path.join(self.static_fs_path, self.header_html)
        footer_path = os.path.join(self.static_fs_path, self.footer_html)

        async with aiofiles.open(header_path, "r") as f:
            header = await f.read()
        async with aiofiles.open(footer_path, "r") as f:
            footer = await f.read()

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
        current_display_path = f"/files/{escaped_rel_path}"

        # JavaScript string escaping for upload_dir
        js_safe_upload_dir = json.dumps(str(self.upload_dir))
        path_injection = f"<script>window.__SELKIES_INJECTED_PATH_PREFIX__ = {js_safe_upload_dir};</script>"

        html_content = f"""
        {header}
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
        {footer}
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
        self.app.add_routes(
            [
                web.get(f"{api_prefix}/status", self.handle_status),
                web.get(f"{api_prefix}/health", self.handle_health),
                web.post(f"{api_prefix}/switch", self.handle_switch),
            ]
        )

        # Register service routes
        for service in self.services.values():
            service.register_routes(api_prefix, self.app.router)

        self.static_fs_path = self._get_static_content_path()
        if self.static_fs_path:

            async def index_handler(_):
                return web.FileResponse(os.path.join(self.static_fs_path, "index.html"))

            self.app.router.add_get(
                f"{api_prefix}/files/{{path:.*}}", self.fancy_index_handler
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
