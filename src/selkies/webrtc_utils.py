# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""WebRTC support utilities: ICE configuration, metrics, and resource monitors.

Three groups of helpers live here:

- RTC ICE configuration: builders and parsers for RTCPeerConnection-style ICE
  server JSON, the prioritized `get_rtc_configuration` resolver, and refresh
  monitors (HMAC shared-secret, TURN REST, Cloudflare, and a config-file
  watcher) that push updated credentials through an `on_rtc_config` callback.
- Metrics: Prometheus gauges/histograms plus an optional per-connection CSV
  dump of client-reported WebRTC statistics. CSV writes run on a dedicated
  single-worker thread pool so they preserve row order, never block the event
  loop, and can be drained deterministically at teardown.
- System/GPU monitors: asyncio polling loops that off-load blocking psutil and
  GPU queries to worker threads and report through async callbacks.

All periodic loops share the same shutdown idiom: an `asyncio.Event` waited on
with a timeout, so `stop()` interrupts the sleep immediately instead of waiting
out the period.
"""

import json
import time
import psutil
import asyncio
import inspect
import aiohttp
import aiofiles
import logging
import urllib.parse
import hashlib
import hmac
import base64
from watchdog.observers import Observer
from typing import Awaitable, Callable, Tuple, List, Dict, Any, Optional, Union
from watchdog.events import FileClosedEvent, FileSystemEventHandler

import os
import csv
import stat
import threading
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from collections import OrderedDict
from prometheus_client import REGISTRY
from prometheus_client import Gauge, Histogram, Info

from . import gpu_stats


logger_rtcice = logging.getLogger("rtcice")
logger_rtcice.setLevel(logging.INFO)

DEFAULT_RTC_CONFIG = """{
  "lifetimeDuration": "86400s",
  "iceServers": [
    {
      "urls": [
        "stun:stun.l.google.com:19302"
      ]
    }
  ],
  "blockStatus": "NOT_BLOCKED",
  "iceTransportPolicy": "all"
}"""

DEFAULT_STUN_SERVERS = [
    ("stun.l.google.com", 19302),
    ("stun.cloudflare.com", 3478)
]


def _format_ice_host(host: str) -> str:
    """Brackets bare IPv6 literals so they are valid in `host:port` URLs."""
    if host and ":" in host and not (host.startswith("[") and host.endswith("]")):
        return f"[{host}]"
    return host


def _extract_host_port(url: str, scheme: str, default_port: int) -> Tuple[Optional[str], int]:
    """Parses the host and port out of an ICE URL such as `stun:host:port`.

    Args:
        url: The full ICE URL, beginning with `scheme` and a colon.
        scheme: The URL scheme (`stun`, `turn`, or `turns`), used to strip
            the prefix before parsing.
        default_port: Port to use when the URL omits one or carries an
            unparsable one.

    Returns:
        A `(host, port)` tuple; `host` is None when the URL has no host.
    """
    parsed = urllib.parse.urlparse("//" + url[len(scheme) + 1:])
    host = parsed.hostname
    if not host:
        return None, default_port
    try:
        port = parsed.port or default_port
    except ValueError:
        port = default_port
    return host, port


def _append_stun_url(stun_list: List[str], seen_stun: set, host: Optional[str], port: Any) -> None:
    """Appends a deduplicated `stun:host:port` URL to `stun_list`.

    Deduplication is case-insensitive on host and keyed on the parsed port
    (unparsable ports fall back to 3478), tracked via the caller-owned
    `seen_stun` set so multiple sources can share one dedup scope.
    """
    if not host:
        return
    try:
        port_num = int(port)
    except (TypeError, ValueError):
        port_num = 3478

    key = (host.lower(), port_num)
    if key in seen_stun:
        return

    seen_stun.add(key)
    stun_list.append(f"stun:{_format_ice_host(host)}:{port_num}")


async def _dispatch_rtc_callback(callback: Callable[[List[str], List[str], bytes], Any], stun_servers: List[str], turn_servers: List[str], rtc_config: bytes) -> None:
    """Invokes an `on_rtc_config` callback, async or sync.

    Sync callbacks run in a worker thread so a slow consumer cannot stall the
    event loop.
    """
    if inspect.iscoroutinefunction(callback):
        await callback(stun_servers, turn_servers, rtc_config)
        return
    await asyncio.to_thread(callback, stun_servers, turn_servers, rtc_config)


def _log_asyncio_task_error(task: asyncio.Task) -> None:
    """Surfaces exceptions from fire-and-forget callback tasks in the log.

    Cancellation (pending callbacks at shutdown) is expected and silent.
    """
    try:
        task.result()
    except asyncio.CancelledError:
        pass
    except Exception as e:
        logger_rtcice.warning(f"Error in on_rtc_config callback task: {e}")


def _schedule_rtc_callback(loop: asyncio.AbstractEventLoop, callback: Callable[[List[str], List[str], bytes], Any], stun_servers: List[str], turn_servers: List[str], rtc_config: bytes) -> None:
    """Schedules an `on_rtc_config` dispatch on the loop from any thread."""
    task = loop.create_task(_dispatch_rtc_callback(callback, stun_servers, turn_servers, rtc_config))
    task.add_done_callback(_log_asyncio_task_error)


def generate_rtc_config(
    turn_host: str,
    turn_port: Union[int, str],
    shared_secret: str,
    user: Optional[str],
    protocol: str = 'udp',
    turn_tls: bool = False,
    stun_host: Optional[str] = None,
    stun_port: Optional[Union[int, str]] = None
) -> str:
    """Builds an RTC config JSON string with coturn-style HMAC TURN credentials.

    Derives a short-term credential from the shared secret: the username is
    `expiry:user` (expiry 24 hours out) and the password is the base64 HMAC-SHA1
    of that username, matching coturn's `use-auth-secret` scheme. STUN servers
    are the optional explicit host, the TURN host itself, and the built-in
    defaults, deduplicated in that order.

    Args:
        turn_host: TURN server hostname or IP.
        turn_port: TURN server port.
        shared_secret: The secret shared with the TURN server for HMAC auth.
        user: Base username for the credential. Colons are replaced because
            they delimit the expiry field, and an empty/None value falls back
            to `selkies` so the username is never a bare `expiry:`.
        protocol: TURN transport, `udp` or `tcp`.
        turn_tls: Emit a `turns:` URL instead of `turn:`.
        stun_host: Optional additional STUN host to list first.
        stun_port: Port for `stun_host`.

    Returns:
        Pretty-printed RTC config JSON.
    """
    user = (user or "").strip() or "selkies"
    user = user.replace(":", "-")

    expiry_hour = 24

    exp = int(time.time()) + expiry_hour * 3600
    username = "{}:{}".format(exp, user)

    hashed = hmac.new(bytes(shared_secret, "utf-8"), bytes(username, "utf-8"), hashlib.sha1).digest()
    password = base64.b64encode(hashed).decode()

    stun_list: List[str] = []
    seen_stun: set = set()
    if stun_host is not None and stun_port is not None:
        _append_stun_url(stun_list, seen_stun, str(stun_host), stun_port)
    _append_stun_url(stun_list, seen_stun, str(turn_host), turn_port)
    _append_stun_url(stun_list, seen_stun, "stun.l.google.com", 19302)
    _append_stun_url(stun_list, seen_stun, "stun.cloudflare.com", 3478)

    rtc_config = {}
    rtc_config["lifetimeDuration"] = "{}s".format(expiry_hour * 3600)
    rtc_config["blockStatus"] = "NOT_BLOCKED"
    rtc_config["iceTransportPolicy"] = "all"
    rtc_config["iceServers"] = []
    rtc_config["iceServers"].append({
        "urls": stun_list
    })
    rtc_config["iceServers"].append({
        "urls": [
            "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', _format_ice_host(str(turn_host)), turn_port, protocol)
        ],
        "username": username,
        "credential": password
    })

    return json.dumps(rtc_config, indent=2)

class HMACRTCMonitor:
    """Periodically regenerates HMAC TURN credentials before they expire.

    Rebuilds the config every `period` seconds on the running event loop and
    delivers it through the `on_rtc_config` callback, which the consumer must
    assign before `start()`.
    """

    def __init__(
        self,
        turn_host: str,
        turn_port: str,
        turn_shared_secret: str,
        turn_username: str,
        turn_protocol: str = 'udp',
        turn_tls: bool = False,
        stun_host: Optional[str] = None,
        stun_port: Optional[str] = None,
        period: int = 60,
        enabled: bool = True
    ):
        self.turn_host = turn_host
        self.turn_port = turn_port
        self.turn_username = turn_username
        self.turn_shared_secret = turn_shared_secret
        self.turn_protocol = turn_protocol
        self.turn_tls = turn_tls
        self.stun_host = stun_host
        self.stun_port = stun_port
        self.period = period
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self._task: Optional[asyncio.Task] = None
        self.on_rtc_config: Callable[[List[str], List[str], bytes], Any] = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self) -> None:
        """Starts the periodic refresh task; no-op when disabled."""
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("HMAC RTC monitor started")

    async def _monitor_loop(self) -> None:
        """Regenerates and dispatches credentials until stopped.

        The HMAC generation and config parsing run in worker threads so the
        loop stays responsive; per-iteration failures are logged and retried
        on the next period rather than killing the monitor.
        """
        try:
            while not self.stop_event.is_set():
                try:
                    hmac_data = await asyncio.to_thread(
                        generate_rtc_config,
                        self.turn_host,
                        self.turn_port,
                        self.turn_shared_secret,
                        self.turn_username,
                        self.turn_protocol,
                        self.turn_tls,
                        self.stun_host,
                        self.stun_port)
                    stun_servers, turn_servers, rtc_config = await asyncio.to_thread(parse_rtc_config, hmac_data)
                    await _dispatch_rtc_callback(self.on_rtc_config, stun_servers, turn_servers, rtc_config)
                except Exception as e:
                    logger_rtcice.warning(f"could not fetch TURN HMAC config in periodic monitor: {e}")

                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=self.period)
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_rtcice.error(f"Error in HMAC RTC monitor: {e}")
        finally:
            logger_rtcice.info("HMAC RTC monitor stopped")

    async def stop(self) -> None:
        """Signals the loop to exit and waits for the task to finish."""
        self.stop_event.set()
        if self._task:
            await self._task

class RESTRTCMonitor:
    """Periodically re-fetches TURN credentials from a TURN REST API.

    Fetches every `period` seconds and delivers the parsed config through the
    `on_rtc_config` callback, which the consumer must assign before `start()`.
    Request parameters (protocol, TLS, username) travel in configurable HTTP
    headers so custom REST endpoints can be matched without code changes.
    """

    def __init__(
        self,
        turn_rest_uri: str,
        turn_rest_username: str,
        turn_rest_username_auth_header: str,
        turn_protocol: str = 'udp',
        turn_rest_protocol_header: str = 'x-turn-protocol',
        turn_tls: bool = False,
        turn_rest_tls_header: str = 'x-turn-tls',
        turn_api_key: Optional[str] = None,
        period: int = 60,
        enabled: bool = True
    ):
        self.period = period
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self._task: Optional[asyncio.Task] = None
        self.turn_rest_uri = turn_rest_uri
        self.turn_rest_username = turn_rest_username.replace(":", "-")
        self.turn_rest_username_auth_header = turn_rest_username_auth_header
        self.turn_protocol = turn_protocol
        self.turn_rest_protocol_header = turn_rest_protocol_header
        self.turn_tls = turn_tls
        self.turn_rest_tls_header = turn_rest_tls_header
        self.turn_api_key = turn_api_key if turn_api_key else None
        self.on_rtc_config: Callable[[List[str], List[str], bytes], Any] = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self) -> None:
        """Starts the periodic refresh task; no-op when disabled."""
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("TURN REST RTC monitor started")

    async def _monitor_loop(self) -> None:
        """Fetches and dispatches REST configs until stopped.

        Per-iteration failures are logged and retried on the next period
        rather than killing the monitor.
        """
        try:
            while not self.stop_event.is_set():
                try:
                    stun_servers, turn_servers, rtc_config = await fetch_turn_rest(
                        self.turn_rest_uri,
                        self.turn_rest_username,
                        self.turn_rest_username_auth_header,
                        self.turn_protocol,
                        self.turn_rest_protocol_header,
                        self.turn_tls,
                        self.turn_rest_tls_header,
                        self.turn_api_key
                    )
                    await _dispatch_rtc_callback(self.on_rtc_config, stun_servers, turn_servers, rtc_config)
                except Exception as e:
                    logger_rtcice.warning(f"could not fetch TURN REST config in periodic monitor: {e}")

                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=self.period)
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_rtcice.error(f"Error in TURN REST RTC monitor: {e}")
        finally:
            logger_rtcice.info("TURN REST RTC monitor stopped")

    async def stop(self) -> None:
        """Signals the loop to exit and waits for the task to finish."""
        self.stop_event.set()
        if self._task:
            await self._task

class CloudflareRTCMonitor:
    """Refreshes Cloudflare TURN credentials before their TTL (default 24h) expires.

    Delivers each refreshed config through the `on_rtc_config` callback, which
    the consumer must assign before `start()`. `period` defaults to half the
    TTL (at least a minute) so a refresh lands well within the credential
    lifetime.
    """

    def __init__(
        self,
        turn_token_id: str,
        api_token: str,
        ttl: int = 86400,
        period: Optional[int] = None,
        enabled: bool = True
    ):
        self.turn_token_id = turn_token_id
        self.api_token = api_token
        self.ttl = ttl
        self.period = period if period is not None else max(60, ttl // 2)
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self._task: Optional[asyncio.Task] = None
        self.on_rtc_config: Callable[[List[str], List[str], bytes], Any] = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self) -> None:
        """Starts the periodic refresh task; no-op when disabled."""
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("Cloudflare TURN RTC monitor started")

    async def _monitor_loop(self) -> None:
        """Refreshes and dispatches Cloudflare credentials until stopped.

        Each iteration waits a period before fetching: the initial credentials
        were already fetched at startup by `get_rtc_configuration`.
        """
        try:
            while not self.stop_event.is_set():
                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=self.period)
                    break
                except asyncio.TimeoutError:
                    pass

                try:
                    json_config = await fetch_cloudflare_turn(self.turn_token_id, self.api_token, self.ttl)
                    wrapped_config = json.dumps({"iceServers": [json_config["iceServers"]]})
                    stun_servers, turn_servers, rtc_config = parse_rtc_config(wrapped_config)
                    await _dispatch_rtc_callback(self.on_rtc_config, stun_servers, turn_servers, rtc_config)
                except Exception as e:
                    logger_rtcice.warning(f"could not refresh Cloudflare TURN config in periodic monitor: {e}")
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_rtcice.error(f"Error in Cloudflare TURN RTC monitor: {e}")
        finally:
            logger_rtcice.info("Cloudflare TURN RTC monitor stopped")

    async def stop(self) -> None:
        """Signals the loop to exit and waits for the task to finish."""
        self.stop_event.set()
        if self._task:
            await self._task

class RTCConfigFileMonitor(FileSystemEventHandler):
    """Watches an RTC config JSON file and dispatches it on every change.

    Runs a watchdog observer thread on the file's directory; parsed configs
    are marshalled back onto the event loop captured at construction time and
    delivered through the `on_rtc_config` callback. Must therefore be
    constructed on a running event loop. Reloads on `on_closed` (an
    in-place write) and on `on_moved`/`on_created`, which is how the
    write-temp-then-rename pattern surfaces (never as a close).
    """

    def __init__(self, rtc_file: str, enabled: bool = True):
        self.enabled = enabled
        self.rtc_file = os.path.abspath(rtc_file)
        self.watch_dir = os.path.dirname(self.rtc_file) or "."
        self._loop = asyncio.get_running_loop()
        self.on_rtc_config: Callable[[List[str], List[str], bytes], Any] = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

        self.observer = Observer()
        self.observer.schedule(self, self.watch_dir, recursive=False)

    async def start(self) -> None:
        """Starts the watchdog observer thread; no-op when disabled."""
        if not self.enabled:
            return

        await asyncio.to_thread(self.observer.start)
        logger_rtcice.info(f"RTC config file monitor started for: {self.rtc_file}")

    def _shutdown_observer(self) -> None:
        """Stops the observer and joins its thread; runs off the event loop."""
        if self.observer.is_alive():
            self.observer.stop()
            self.observer.join()

    async def stop(self) -> None:
        """Stops the watchdog observer; no-op when disabled."""
        if not self.enabled:
            return

        await asyncio.to_thread(self._shutdown_observer)
        logger_rtcice.info("RTC config file monitor stopped")

    def _reload_config(self, src_path: str) -> None:
        """Reads, parses, and dispatches the updated RTC config.

        Runs on the watchdog thread; the callback dispatch is handed to the
        event loop via `call_soon_threadsafe`. The file is re-checked for
        trusted ownership/permissions on every reload because it can be
        replaced between events.
        """
        try:
            logger_rtcice.info(f"Detected RTC JSON file change: {src_path}")
            if not _is_trusted_config_file(self.rtc_file):
                logger_rtcice.error(
                    f"Refusing to reload RTC config file '{self.rtc_file}': unsafe ownership or permissions."
                )
                return
            with open(self.rtc_file, 'rb') as f:
                data = f.read()

            stun_servers, turn_servers, rtc_config = parse_rtc_config(data)
            self._loop.call_soon_threadsafe(
                _schedule_rtc_callback,
                self._loop,
                self.on_rtc_config,
                stun_servers,
                turn_servers,
                rtc_config
            )
        except Exception as e:
            logger_rtcice.warning(f"Could not read or parse RTC JSON file: {self.rtc_file}: {e}")

    def on_closed(self, event: Any) -> None:
        """Reloads after an in-place write of the config file."""
        if not isinstance(event, FileClosedEvent):
            return
        if os.path.abspath(event.src_path) != self.rtc_file:
            return
        self._reload_config(event.src_path)

    def on_moved(self, event: Any) -> None:
        """Reloads when a temp file is renamed onto the config file."""
        dest = getattr(event, "dest_path", None)
        if dest and os.path.abspath(dest) == self.rtc_file:
            self._reload_config(dest)

    def on_created(self, event: Any) -> None:
        """Reloads when the config file is created anew."""
        if os.path.abspath(event.src_path) == self.rtc_file:
            self._reload_config(event.src_path)

def make_turn_rtc_config_json_legacy(
    turn_host: str,
    turn_port: int,
    username: str,
    password: str,
    protocol: str = 'udp',
    turn_tls: bool = False,
    stun_host: Optional[str] = None,
    stun_port: Optional[int] = None
) -> str:
    """Builds an RTC config JSON string from long-term TURN credentials.

    Unlike `generate_rtc_config`, the username/password pair is used verbatim
    (no HMAC derivation), matching TURN servers configured with static
    long-term credentials.

    Returns:
        Pretty-printed RTC config JSON.
    """
    stun_list: List[str] = []
    seen_stun: set = set()
    if stun_host is not None and stun_port is not None:
        _append_stun_url(stun_list, seen_stun, str(stun_host), stun_port)
    _append_stun_url(stun_list, seen_stun, str(turn_host), turn_port)
    for default_host, default_port in DEFAULT_STUN_SERVERS:
        _append_stun_url(stun_list, seen_stun, default_host, default_port)

    rtc_config: Dict[str, Any] = {}
    rtc_config["lifetimeDuration"] = "86400s"
    rtc_config["blockStatus"] = "NOT_BLOCKED"
    rtc_config["iceTransportPolicy"] = "all"
    rtc_config["iceServers"] = []
    rtc_config["iceServers"].append({
        "urls": stun_list
    })
    rtc_config["iceServers"].append({
        "urls": [
            "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', _format_ice_host(str(turn_host)), turn_port, protocol)
        ],
        "username": username,
        "credential": password
    })
    return json.dumps(rtc_config, indent=2)

def parse_rtc_config(data: Union[str, bytes]) -> Tuple[List[str], List[str], bytes]:
    """Parses an RTC config JSON document into STUN/TURN URI lists.

    Accepts the RTCPeerConnection `iceServers` shape as well as several
    variants seen in the wild — a lowercase `iceservers` key, TURN REST
    responses that use a top-level `uris` list with `username`/`password`,
    per-server `uris`/`password` keys, and string-valued `urls` — and
    normalizes them all to the spec shape. Entries and URLs of invalid type
    are dropped with a warning rather than failing the whole config, since
    the input may come from an external REST service or a user-edited file.

    Args:
        data: RTC config JSON as text or bytes.

    Returns:
        A tuple of `(stun_uris, turn_uris, config_bytes)`: deduplicated
        `stun://host:port` URIs, deduplicated `turn(s)://` URIs with embedded
        percent-encoded credentials when available, and the config as UTF-8
        JSON bytes (re-serialized only when normalization changed it).

    Raises:
        TypeError: If the root or `iceServers` value has the wrong type.
        KeyError: If no ice-server data can be located at all.
    """
    rtc_config = json.loads(data)
    if not isinstance(rtc_config, dict):
        raise TypeError(f"Invalid RTC config root type: {type(rtc_config)}")

    normalized_config = False
    ice_servers = rtc_config.get('iceServers')
    if ice_servers is None:
        ice_servers = rtc_config.get('iceservers')
        if ice_servers is not None:
            rtc_config['iceServers'] = ice_servers
            rtc_config.pop('iceservers', None)
            normalized_config = True

    if ice_servers is None and 'uris' in rtc_config:
        uris = rtc_config.get('uris')
        if uris is None:
            uris = []
        if isinstance(uris, str):
            uris = [uris]
        elif not isinstance(uris, list):
            logger_rtcice.warning("Invalid 'uris' type: %s", type(uris))
            uris = []

        turn_urls = [uri for uri in uris if isinstance(uri, str) and (uri.lower().startswith('turn:') or uri.lower().startswith('turns:'))]
        stun_urls = [uri for uri in uris if isinstance(uri, str) and uri.lower().startswith('stun:')]

        normalized_stun_urls: List[str] = []
        seen_stun: set = set()

        for stun_url in stun_urls:
            host, port = _extract_host_port(stun_url, 'stun', 3478)
            _append_stun_url(normalized_stun_urls, seen_stun, host, port)

        for turn_url in turn_urls:
            lower_turn = turn_url.lower()
            scheme = 'turns' if lower_turn.startswith('turns:') else 'turn'
            host, port = _extract_host_port(turn_url, scheme, 443 if scheme == 'turns' else 3478)
            _append_stun_url(normalized_stun_urls, seen_stun, host, port)

        for default_host, default_port in DEFAULT_STUN_SERVERS:
            _append_stun_url(normalized_stun_urls, seen_stun, default_host, default_port)

        ice_servers = []
        if normalized_stun_urls:
            ice_servers.append({
                "urls": normalized_stun_urls
            })
        if turn_urls:
            turn_entry: Dict[str, Any] = {
                "urls": turn_urls
            }
            turn_username = rtc_config.get('username')
            turn_password = rtc_config.get('password')
            if turn_username not in (None, '') and turn_password not in (None, ''):
                turn_entry["username"] = str(turn_username)
                turn_entry["credential"] = str(turn_password)
            ice_servers.append(turn_entry)

        ttl = rtc_config.get('ttl', 86400)
        try:
            ttl = int(ttl)
            if ttl <= 0:
                ttl = 86400
        except (ValueError, TypeError):
            ttl = 86400

        rtc_config = {
            "lifetimeDuration": "{}s".format(ttl),
            "iceServers": ice_servers,
            "blockStatus": "NOT_BLOCKED",
            "iceTransportPolicy": "all"
        }
        normalized_config = True

    if ice_servers is None:
        raise KeyError('missing "iceServers"/"iceservers" or TURN REST "uris" keys in RTC config')

    if not isinstance(ice_servers, list):
        raise TypeError(f"Invalid 'iceServers' type: {type(ice_servers)}")

    stun_uris = []
    turn_uris = []
    seen_stun_uris = set()
    seen_turn_uris = set()
    for ice_server in ice_servers:
        if not isinstance(ice_server, dict):
            logger_rtcice.warning("Invalid ice server entry type: %s", type(ice_server))
            normalized_config = True
            continue

        if "uris" in ice_server and "urls" not in ice_server:
            ice_server["urls"] = ice_server.pop("uris")
            normalized_config = True

        if "password" in ice_server and "credential" not in ice_server:
            ice_server["credential"] = ice_server.pop("password")
            normalized_config = True
        
        urls = ice_server.get("urls", [])
        if isinstance(urls, str):
            urls = [urls]
            normalized_config = True
        if not isinstance(urls, list):
            logger_rtcice.warning("Invalid 'urls' type: %s", type(urls))
            normalized_config = True
            continue

        filtered_urls = [url for url in urls if isinstance(url, str)]
        if len(filtered_urls) != len(urls):
            normalized_config = True
            urls = filtered_urls

        if ice_server.get("urls") != urls:
            ice_server["urls"] = urls
            normalized_config = True
        
        for url in urls:
            lower_url = url.lower()
            if lower_url.startswith("stun:"):
                stun_host, stun_port = _extract_host_port(url, "stun", 3478)
                if not stun_host:
                    continue
                stun_uri = "stun://%s:%s" % (
                    _format_ice_host(stun_host),
                    stun_port
                )
                if stun_uri not in seen_stun_uris:
                    stun_uris.append(stun_uri)
                    seen_stun_uris.add(stun_uri)
            elif lower_url.startswith("turn:") or lower_url.startswith("turns:"):
                protocol = "turn" if lower_url.startswith("turn:") else "turns"
                parsed_turn = urllib.parse.urlparse("//" + url[len(protocol) + 1:])
                turn_host = parsed_turn.hostname
                if not turn_host:
                    continue
                try:
                    turn_port = parsed_turn.port or (443 if protocol == "turns" else 3478)
                except ValueError:
                    turn_port = 443 if protocol == "turns" else 3478

                query = f"?{parsed_turn.query}" if parsed_turn.query else ""
                turn_user = ice_server.get('username')
                turn_password = ice_server.get('credential')

                if turn_user in (None, '') and parsed_turn.username is not None:
                    turn_user = urllib.parse.unquote(parsed_turn.username)
                if turn_password in (None, '') and parsed_turn.password is not None:
                    turn_password = urllib.parse.unquote(parsed_turn.password)

                has_credentials = turn_user not in (None, '') and turn_password not in (None, '')
                if has_credentials:
                    turn_uri = "%s://%s:%s@%s:%s%s" % (
                        protocol,
                        urllib.parse.quote(str(turn_user), safe=""),
                        urllib.parse.quote(str(turn_password), safe=""),
                        _format_ice_host(turn_host),
                        turn_port,
                        query
                    )
                else:
                    turn_uri = "%s://%s:%s%s" % (
                        protocol,
                        _format_ice_host(turn_host),
                        turn_port,
                        query
                    )
                if turn_uri not in seen_turn_uris:
                    turn_uris.append(turn_uri)
                    seen_turn_uris.add(turn_uri)
    if normalized_config:
        data = json.dumps(rtc_config).encode("utf-8")
    elif isinstance(data, str):
        data = data.encode("utf-8")
    return stun_uris, turn_uris, data

async def fetch_turn_rest(
    uri: str,
    user: str,
    auth_header_username: str = 'x-auth-user',
    protocol: str = 'udp',
    header_protocol: str = 'x-turn-protocol',
    turn_tls: bool = False,
    header_tls: str = 'x-turn-tls',
    turn_api_key: Optional[str] = None
) -> Tuple[List[str], List[str], bytes]:
    """Fetches TURN configuration from a TURN REST API endpoint.

    The username, transport protocol, and TLS flag are sent both as HTTP
    headers (names configurable per deployment) and, for the username/API key,
    as query parameters, to cover the header- and query-style REST dialects.

    Args:
        uri: The REST endpoint URL.
        user: Username to request credentials for.
        auth_header_username: Header name carrying the username.
        protocol: TURN transport, `udp` or `tcp`.
        header_protocol: Header name carrying the transport.
        turn_tls: Request `turns:` URLs.
        header_tls: Header name carrying the TLS flag.
        turn_api_key: Optional API key, sent as both `key` and `api` query
            parameters to satisfy either dialect.

    Returns:
        The `parse_rtc_config` tuple of STUN URIs, TURN URIs, and config bytes.

    Raises:
        Exception: On HTTP errors, empty responses, timeouts, or network
            failures (original errors are chained).
    """
    auth_headers: Dict[str, str] = {}
    if auth_header_username:
        auth_headers[auth_header_username] = user
    if header_protocol:
        auth_headers[header_protocol] = protocol
    if header_tls:
        auth_headers[header_tls] = 'true' if turn_tls else 'false'

    params = {
        'service': 'turn',
        'username': user
    }
    if turn_api_key:
        params['key'] = turn_api_key
        params['api'] = turn_api_key

    timeout = aiohttp.ClientTimeout(total=10, connect=5)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        try:
            async with session.get(uri, headers=auth_headers, params=params) as response:
                content = await response.read()
                if response.status >= 400:
                    body = content.decode('utf-8', errors='replace')
                    raise Exception(f"Error fetching REST API config: {response.status} {response.reason}. Body: {body}")
                if not content:
                    raise Exception("Data from REST API service was empty")
                return parse_rtc_config(content)
        except asyncio.TimeoutError as e:
            raise Exception("Timeout while fetching REST API config") from e
        except aiohttp.ClientError as e:
            raise Exception(f"Network error while fetching REST API config: {e}") from e

async def fetch_cloudflare_turn(turn_token_id: str, api_token: str, ttl: int = 86400) -> Dict[str, Any]:
    """Obtains TURN credentials from the Cloudflare Calls API.

    Args:
        turn_token_id: Cloudflare TURN key ID.
        api_token: Cloudflare API bearer token.
        ttl: Requested credential lifetime in seconds.

    Returns:
        The decoded JSON response, whose `iceServers` member holds the
        credentialed server entry.

    Raises:
        Exception: On HTTP errors, timeouts, or network failures (original
            errors are chained).
    """
    auth_headers = {
        "authorization": f"Bearer {api_token}",
    }
    uri = f"https://rtc.live.cloudflare.com/v1/turn/keys/{turn_token_id}/credentials/generate"
    data_payload = {"ttl": ttl}

    timeout = aiohttp.ClientTimeout(total=10, connect=5)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        try:
            async with session.post(uri, headers=auth_headers, json=data_payload) as response:
                response.raise_for_status()
                return await response.json()
        except aiohttp.ClientResponseError as e:
            # ClientResponseError carries no `.response`, and the body is gone
            # once the `async with` exits; report status/message only.
            raise Exception(f"Could not obtain Cloudflare TURN credentials: {e.status} {e.message}.") from e
        except asyncio.TimeoutError as e:
            raise Exception("Timeout while fetching Cloudflare credentials") from e
        except aiohttp.ClientError as e:
            raise Exception(f"Network error while fetching Cloudflare credentials: {e}") from e

async def try_cloudflare(args: Any) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC using Cloudflare TURN.

    Returns:
        The parsed config tuple, or None when Cloudflare TURN is disabled,
        misconfigured, or the fetch fails (so the caller can fall through to
        the next configuration method).
    """
    if not args.enable_cloudflare_turn:
        return None

    if not (args.cloudflare_turn_token_id and args.cloudflare_turn_api_token):
        logger_rtcice.error("Cloudflare TURN is enabled but token ID and/or API token are missing.")
        return None

    try:
        json_config = await fetch_cloudflare_turn(args.cloudflare_turn_token_id, args.cloudflare_turn_api_token)
        # Do not log json_config: it contains live TURN username/credential values.
        logger_rtcice.info("Successfully fetched RTC configuration from Cloudflare.")
        wrapped_config = json.dumps({"iceServers": [json_config["iceServers"]]})
        return parse_rtc_config(wrapped_config)
    except Exception as e:
        logger_rtcice.warning(f"Failed to fetch TURN config from Cloudflare: {e}")
        return None

def _is_trusted_config_file(path: str) -> bool:
    """Returns True if the file is safe to trust as an RTC config source.

    The config file overrides all other STUN/TURN settings and its default
    location is world-writable /tmp, so it must not be a symlink, must be
    owned by root or the current user, and must not be group- or
    world-writable.
    """
    try:
        st = os.lstat(path)
    except OSError as e:
        logger_rtcice.warning(f"Could not stat RTC config file '{path}': {e}")
        return False
    if stat.S_ISLNK(st.st_mode):
        logger_rtcice.warning(f"Refusing to follow symlinked RTC config file '{path}'.")
        return False
    if st.st_uid not in (0, os.getuid()):
        logger_rtcice.warning(
            f"RTC config file '{path}' is owned by uid {st.st_uid}, not root or the current user ({os.getuid()})."
        )
        return False
    if st.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        logger_rtcice.warning(
            f"RTC config file '{path}' is group- or world-writable (mode {oct(stat.S_IMODE(st.st_mode))}); refusing to trust it."
        )
        return False
    return True


async def try_json_file(args: Any) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC from a local JSON file.

    Returns:
        The parsed config tuple, or None when the file is absent, untrusted
        (see `_is_trusted_config_file`), or unparsable.
    """
    if not os.path.exists(args.rtc_config_json):
        return None

    if not _is_trusted_config_file(args.rtc_config_json):
        logger_rtcice.error(
            f"Refusing to use RTC config file '{args.rtc_config_json}': unsafe ownership or permissions."
        )
        return None

    logger_rtcice.warning(f"Using JSON file '{args.rtc_config_json}' for RTC config, overrides all other STUN/TURN settings.")
    try:
        async with aiofiles.open(args.rtc_config_json, 'r') as f:
            content = await f.read()
            return parse_rtc_config(content)
    except Exception as e:
        logger_rtcice.error(f"Failed to read or parse RTC config file '{args.rtc_config_json}': {e}")
        return None

async def try_rest_api(args: Any, username: str, protocol: str, use_tls: bool) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC from a custom TURN REST API.

    Returns:
        The parsed config tuple, or None when no REST URI is configured or
        the fetch fails.
    """
    if not args.turn_rest_uri:
        return None

    try:
        api_key = getattr(args, 'turn_rest_api_key', None)
        config = await fetch_turn_rest(
            args.turn_rest_uri, username, args.turn_rest_username_auth_header,
            protocol, args.turn_rest_protocol_header, use_tls, args.turn_rest_tls_header, api_key
        )
        logger_rtcice.info("Using TURN REST API for RTC configuration.")
        return config
    except Exception as e:
        logger_rtcice.warning(f"Error fetching from TURN REST API, falling back to other methods: {e}")
        return None

def try_legacy_turn(args: Any, protocol: str, use_tls: bool) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC using long-term TURN credentials.

    Returns:
        The parsed config tuple, or None when any of host, port, username, or
        password is missing.
    """
    if not (args.turn_username and args.turn_password and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using long-term username/password for TURN credentials.")
    config_json = make_turn_rtc_config_json_legacy(
        args.turn_host, args.turn_port, args.turn_username, args.turn_password,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(config_json)

def try_hmac_turn(args: Any, username: str, protocol: str, use_tls: bool) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC using short-term HMAC credentials.

    Returns:
        The parsed config tuple, or None when the shared secret, host, or
        port is missing.
    """
    if not (args.turn_shared_secret and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using short-term shared secret HMAC for TURN credentials.")
    hmac_data = generate_rtc_config(
        args.turn_host, args.turn_port, args.turn_shared_secret, username,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(hmac_data)

async def get_rtc_configuration(args: Any) -> Tuple[List[str], List[str], bytes, Dict[str, bool]]:
    """Resolves the RTC configuration from a prioritized sequence of sources.

    Tries, in order: the Cloudflare TURN API, a local RTC config JSON file, a
    custom TURN REST API, long-term TURN credentials (username/password),
    short-term HMAC credentials (shared secret), and finally the built-in
    STUN-only default. The first source that yields a config wins.

    Args:
        args: Parsed CLI/settings namespace carrying the TURN/STUN options.

    Returns:
        A tuple of `(stun_uris, turn_uris, config_bytes, sources_used)`, where
        `sources_used` flags which refreshable source produced the config so
        the caller can start the matching periodic monitor.
    """

    turn_rest_username = args.turn_rest_username.replace(":", "-")
    turn_protocol = 'tcp' if args.turn_protocol.lower() == 'tcp' else 'udp'
    using_turn_tls = args.turn_tls

    monitoring_utilities_used = {
        "using_hmac_turn": False,
        "using_rtc_config_json": False,
        "using_rest_api": False,
        "using_cloudflare_turn": False
    }

    if config := await try_cloudflare(args):
        monitoring_utilities_used["using_cloudflare_turn"] = True
        return *config, monitoring_utilities_used

    if config := await try_json_file(args):
        monitoring_utilities_used["using_rtc_config_json"] = True
        return *config, monitoring_utilities_used

    if config := await try_rest_api(args, turn_rest_username, turn_protocol, using_turn_tls):
        monitoring_utilities_used["using_rest_api"] = True
        return *config, monitoring_utilities_used

    if config := try_legacy_turn(args, turn_protocol, using_turn_tls):
        return *config, monitoring_utilities_used

    if config := try_hmac_turn(args, turn_rest_username, turn_protocol, using_turn_tls):
        monitoring_utilities_used["using_hmac_turn"] = True
        return *config, monitoring_utilities_used

    logger_rtcice.warning("No valid TURN server information found, using default RTC config.")
    return *parse_rtc_config(DEFAULT_RTC_CONFIG), monitoring_utilities_used


logger_metrics = logging.getLogger("metrics")
logger_metrics.setLevel(logging.INFO)

FPS_HIST_BUCKETS = (0, 20, 40, 60)

# Bound the diagnostic stats CSV: field names come from the untrusted client, so cap
# header width and retained rows so it can't grow the file unbounded.
WEBRTC_CSV_MAX_HEADERS = 2048
WEBRTC_CSV_MAX_RETAINED_ROWS = 100000

class Metrics:
    """Prometheus metrics plus optional CSV capture of client WebRTC stats.

    Registers gauges/histograms in the global Prometheus registry at
    construction; `unregister()` must release every one of them or the next
    `Metrics()` raises DuplicateTimeseries. When `using_webrtc_csv` is set,
    client-reported stat dictionaries are also appended to per-connection CSV
    files whose column schema follows the (untrusted) client's field set with
    bounded width and row count.

    Attributes:
        webrtc_pacer_pace_bps: Pacer gauges are per display and exist only
            while a pacer is attached; the event counters are cumulative
            since transport start.
        prev_stats_video_header_names: Header names of the video CSV (and
            `prev_stats_audio_header_names` for audio), tracked alongside the
            lengths so a same-count field swap still triggers a remap.
        stats_video_row_count: On-disk data rows (excluding the header) of the
            video CSV (`stats_audio_row_count` for audio), so the append path
            bounds file growth without re-reading the file each write.
        _csv_lock: Serializes CSV writes, which run in worker threads, so
            concurrent stat messages cannot interleave rows or race the
            `prev_stats_*` state.
        _csv_executor: Single-worker executor for CSV writes, so `unregister`
            can drain them with `shutdown(wait=True)` (the shared default
            executor must not be shut down) and rows keep their order.
        _csv_tasks: Strong references to in-flight write futures so they are
            not collected before completion and their exceptions stay
            observed.
    """

    def __init__(self, using_webrtc_csv: bool = False):
        self.using_webrtc_csv = using_webrtc_csv

        self.fps = Gauge('fps', 'Frames per second observed by client')
        self.fps_hist = Histogram('fps_hist', 'Histogram of FPS observed by client', buckets=FPS_HIST_BUCKETS)
        self.gpu_utilization = Gauge('gpu_utilization', 'Utilization percentage reported by GPU')
        self.latency = Gauge('latency', 'Latency observed by client')
        self.webrtc_statistics = Info('webrtc_statistics', 'WebRTC Statistics from the client')
        self.webrtc_pacer_pace_bps = Gauge(
            'webrtc_pacer_pace_bps', 'Current pacer rate in bits per second', ['display'])
        self.webrtc_pacer_queue_bytes = Gauge(
            'webrtc_pacer_queue_bytes', 'Bytes queued in the pacer', ['display', 'kind'])
        self.webrtc_pacer_idr_floor_bytes = Gauge(
            'webrtc_pacer_idr_floor_bytes', 'IDR floor of the pacer video queue budget in bytes', ['display'])
        self.webrtc_pacer_events = Gauge(
            'webrtc_pacer_events', 'Cumulative pacer event counter', ['display', 'event'])
        self.stats_video_file_path: Optional[str] = None
        self.stats_audio_file_path: Optional[str] = None
        self.prev_stats_video_header_len: Optional[int]  = None
        self.prev_stats_audio_header_len: Optional[int]  = None
        self.prev_stats_video_header_names: Optional[Tuple[str, ...]] = None
        self.prev_stats_audio_header_names: Optional[Tuple[str, ...]] = None
        self.stats_video_row_count: int = 0
        self.stats_audio_row_count: int = 0
        self._csv_lock = threading.Lock()
        self._csv_executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="webrtc-csv")
        self._csv_tasks: set = set()

    def set_fps(self, fps: float) -> None:
        """Records the client-observed FPS in both the gauge and histogram."""
        self.fps.set(fps)
        self.fps_hist.observe(fps)

    def set_pacer_snapshot(self, display: str, snap: Optional[dict]) -> None:
        """Publish one pacer snapshot per display; no-ops when there is no pacer."""
        if snap is None:
            return
        display = display or "primary"
        self.webrtc_pacer_pace_bps.labels(display).set(snap.get("pace_bps", 0))
        self.webrtc_pacer_queue_bytes.labels(display, "total").set(snap.get("queued_bytes", 0))
        self.webrtc_pacer_queue_bytes.labels(display, "video").set(snap.get("video_bytes", 0))
        self.webrtc_pacer_idr_floor_bytes.labels(display).set(snap.get("idr_floor_bytes", 0))
        for event in ("video_dropped", "gop_resets", "keyreqs",
                      "idr_resurrects", "timeout_resurrects", "stale_resets"):
            self.webrtc_pacer_events.labels(display, event).set(snap.get(event, 0))

    def set_gpu_utilization(self, utilization: float) -> None:
        self.gpu_utilization.set(utilization)

    def set_latency(self, latency_ms: float) -> None:
        self.latency.set(latency_ms)

    def unregister(self) -> None:
        """Unregisters all metrics from the global registry and drains CSV writers.

        Not-yet-started CSV futures are cancelled and the executor shut down
        with `wait=True` first, so no writer thread is still running (or about
        to take the lock) after teardown; draining the lock alone would leave
        that window open. Every collector built in `__init__` is then released,
        each independently so an already-released one does not strand the
        rest: any left behind makes the next `Metrics()` raise
        DuplicateTimeseries and a mode switch back into metrics-enabled
        streaming fails to start.
        """
        for fut in list(self._csv_tasks):
            fut.cancel()
        self._csv_tasks.clear()
        self._csv_executor.shutdown(wait=True)
        for collector in (self.fps, self.fps_hist, self.gpu_utilization,
                          self.latency, self.webrtc_statistics,
                          self.webrtc_pacer_pace_bps, self.webrtc_pacer_queue_bytes,
                          self.webrtc_pacer_idr_floor_bytes, self.webrtc_pacer_events):
            try:
                REGISTRY.unregister(collector)
            except KeyError:
                pass

    async def set_webrtc_stats(self, webrtc_stat_type: str, webrtc_stats: str) -> None:
        """Publishes a client stats report to Prometheus and, optionally, CSV.

        The CSV write is submitted to the dedicated executor rather than
        `asyncio.to_thread`, whose shared executor cannot be drained, so
        `unregister` can join it; a write refused by an already shut-down
        executor (teardown in progress) is dropped. The Prometheus Info
        update is a cheap dict copy and stays inline.

        Args:
            webrtc_stat_type: `_stats_audio` for the audio stream; anything
                else is treated as video.
            webrtc_stats: Raw JSON list of RTCStats-shaped objects from the
                client. Parsing/sanitizing runs in a worker thread to keep
                large reports off the event loop.
        """
        sanitized_stats = await asyncio.to_thread(self._parse_and_sanitize_stats, webrtc_stats)
        if self.using_webrtc_csv:
            is_audio = webrtc_stat_type == "_stats_audio"
            csv_path = self.stats_audio_file_path if is_audio else self.stats_video_file_path
            try:
                fut = self._csv_executor.submit(self.write_webrtc_stats_csv, sanitized_stats, csv_path, is_audio)
            except RuntimeError:
                fut = None
            if fut is not None:
                self._csv_tasks.add(fut)
                fut.add_done_callback(self._csv_tasks.discard)
        self.webrtc_statistics.info(sanitized_stats)

    def _parse_and_sanitize_stats(self, webrtc_stats: str) -> OrderedDict:
        return self.sanitize_json_stats(json.loads(webrtc_stats))

    def sanitize_json_stats(self, obj_list: List[Dict[str, Any]]) -> OrderedDict:
        """Flattens a list of RTCStats objects into `reportName.fieldName` keys.

        The first entry of each stat type gets the bare type as its report
        name; later same-type entries get a `-id` suffix, or `-n` (the
        per-type occurrence index) without an id, plus a collision counter.
        Both stay stable across reorders and inserts, unlike a global list
        index, which would shift every column name whenever the browser
        reordered the list and churn the CSV schema (full rewrites, an
        unbounded union header). Entries are sorted by `(type, id)` first for
        the same reason: the browser may emit same-type stats (two
        `inbound-rtp` for distinct SSRCs) in a different order each message,
        and without a fixed order the bare-named first occurrence could be a
        different SSRC on each row; `sorted` is stable, so entries sharing a
        key keep their input order. All values are stringified. Entries that
        are not dicts are skipped and a missing/non-string `type` defaults to
        `unknown`, since the list comes from the untrusted browser client.
        """
        obj_type = set()
        sanitized_stats = OrderedDict()
        type_counts: Dict[str, int] = {}

        def _identity(entry: Any) -> Tuple[str, str]:
            """Stable `(type, id)` sort key; non-dict entries sort first."""
            if not isinstance(entry, dict):
                return ("", "")
            t = entry.get('type')
            t = t if isinstance(t, str) else "unknown"
            i = entry.get('id')
            i = i if isinstance(i, str) else ""
            return (t, i)

        for entry in sorted(obj_list, key=_identity) if isinstance(obj_list, list) else obj_list:
            if not isinstance(entry, dict):
                continue
            base_key = entry.get('type')
            if not isinstance(base_key, str):
                base_key = "unknown"
            occurrence = type_counts.get(base_key, 0)
            type_counts[base_key] = occurrence + 1
            curr_key = base_key
            if curr_key in obj_type:
                entry_id = entry.get('id')
                if isinstance(entry_id, str) and entry_id:
                    suffix = entry_id
                else:
                    suffix = str(occurrence)
                candidate_key = curr_key + "-" + suffix
                collision = 0
                while candidate_key in obj_type:
                    collision += 1
                    candidate_key = curr_key + "-" + suffix + "-" + str(collision)
                curr_key = candidate_key
            obj_type.add(curr_key)

            for key, val in entry.items():
                unique_type = curr_key + "." + str(key)
                if not isinstance(val, str):
                    sanitized_stats[unique_type] = str(val)
                else:
                    sanitized_stats[unique_type] = val

        return sanitized_stats

    def _bump_and_cap_rows(self, file_path: str, is_audio: bool) -> None:
        """Counts one appended data row, trimming the file once over the cap.

        Tracking the count avoids re-reading the file on every write; the
        O(N) trim runs only when the count exceeds
        WEBRTC_CSV_MAX_RETAINED_ROWS. Caller must hold `self._csv_lock`.
        """
        if is_audio:
            self.stats_audio_row_count += 1
            if self.stats_audio_row_count > WEBRTC_CSV_MAX_RETAINED_ROWS:
                self.stats_audio_row_count = self._trim_csv_to_cap(file_path)
        else:
            self.stats_video_row_count += 1
            if self.stats_video_row_count > WEBRTC_CSV_MAX_RETAINED_ROWS:
                self.stats_video_row_count = self._trim_csv_to_cap(file_path)

    def _trim_csv_to_cap(self, file_path: str) -> int:
        """Drops the oldest data rows so the file stays within the row cap.

        Keeps at most WEBRTC_CSV_MAX_RETAINED_ROWS rows plus the header,
        bounding on-disk growth on the steady-state append path. Rewrites via
        a temp file and atomic replace so an interrupted trim cannot corrupt
        the stats. Caller must hold `self._csv_lock`.

        Returns:
            The resulting on-disk data-row count.
        """
        with open(file_path, 'r', newline='') as stats_file:
            rows = list(csv.reader(stats_file, delimiter=','))
        if not rows:
            return 0
        header, data = rows[0], rows[1:]
        if len(data) <= WEBRTC_CSV_MAX_RETAINED_ROWS:
            return len(data)
        data = data[-WEBRTC_CSV_MAX_RETAINED_ROWS:]
        tmp_path = file_path + ".tmp"
        with open(tmp_path, 'w', newline='') as stats_file:
            csv_writer = csv.writer(stats_file)
            csv_writer.writerow(header)
            csv_writer.writerows(data)
        os.replace(tmp_path, file_path)
        return len(data)

    def write_webrtc_stats_csv(self, obj: dict, file_path: str, is_audio: bool = False) -> None:
        """Appends one sanitized stats report to the CSV file.

        Runs on the dedicated CSV executor thread. Handles three schema cases:
        the same field set in a different order (single-row remap, no
        rewrite), a changed field set (full union-schema rewrite via
        `update_webrtc_stats_csv`), and a fresh file (header plus first row).

        Args:
            obj: Flattened `reportName.fieldName` stats mapping from
                `sanitize_json_stats`.
            file_path: Destination CSV path.
            is_audio: Whether this is the audio stream, passed by the caller
                rather than re-derived from the file path; selects which
                header/row-count state to use.
        """

        dt = datetime.now()
        timestamp = dt.strftime("%d/%B/%Y:%H:%M:%S")
        with self._csv_lock:
            try:
                headers = ["timestamp"]
                headers += obj.keys()

                # Reconnecting clients send near-empty reports; too few fields to
                # be a real stats sample.
                if len(headers) < 15:
                    return

                values = [timestamp]
                values.extend(obj.values())

                header_names = tuple(headers)
                prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
                prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names

                if prev_len is not None and prev_names != header_names:
                    if prev_names is not None and frozenset(prev_names) == frozenset(header_names):
                        value_by_name = dict(zip(headers, values))
                        remapped = [value_by_name.get(name, "NaN") for name in prev_names]
                        with open(file_path, 'a+', newline='') as stats_file:
                            csv.writer(stats_file, quotechar='"').writerow(remapped)
                        self._bump_and_cap_rows(file_path, is_audio)
                        return

                    # Outside any open handle: os.replace() onto an open file fails on Windows.
                    new_len, new_names, new_rows = self.update_webrtc_stats_csv(file_path, headers, values, is_audio)
                    if is_audio:
                        self.prev_stats_audio_header_len = new_len
                        self.prev_stats_audio_header_names = new_names
                        self.stats_audio_row_count = new_rows
                    else:
                        self.prev_stats_video_header_len = new_len
                        self.prev_stats_video_header_names = new_names
                        self.stats_video_row_count = new_rows
                    return

                with open(file_path, 'a+', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file, quotechar='"')
                    if prev_len is None:
                        csv_writer.writerow(headers)
                        csv_writer.writerow(values)
                        if is_audio:
                            self.prev_stats_audio_header_len = len(headers)
                            self.prev_stats_audio_header_names = header_names
                            self.stats_audio_row_count = 1
                        else:
                            self.prev_stats_video_header_len = len(headers)
                            self.prev_stats_video_header_names = header_names
                            self.stats_video_row_count = 1
                    else:
                        csv_writer.writerow(values)
                if prev_len is not None:
                    self._bump_and_cap_rows(file_path, is_audio)

            except Exception as e:
                logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))

    def update_webrtc_stats_csv(self, file_path: str, headers: List[str], values: List[Any], is_audio: bool = False) -> Tuple[Optional[int], Optional[Tuple[str, ...]], int]:
        """Rewrites the CSV when the set of stat fields changes.

        The stored rows are aligned by field name onto the union header (prior
        order, new fields appended, width capped at `WEBRTC_CSV_MAX_HEADERS`
        because the names come from the client), gaps filled with "NaN", and
        only the most recent `WEBRTC_CSV_MAX_RETAINED_ROWS` rows are carried
        forward. The rewrite goes through a temp file and an atomic replace so
        an interrupted one cannot truncate the stats; a file deleted since the
        last write, or holding only a header, is recreated with the current
        schema. Caller must hold `self._csv_lock`.

        Returns:
            A tuple of the new header length, the new header-name tuple, and
            the resulting on-disk data-row count — or the previous values on
            failure so the caller's state stays consistent with the file.
        """
        prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
        prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names
        prev_rows = self.stats_audio_row_count if is_audio else self.stats_video_row_count

        try:
            prev_headers = None
            prev_values = []
            try:
                with open(file_path, 'r', newline='') as stats_file:
                    csv_reader = csv.reader(stats_file, delimiter=',')
                    for idx, row in enumerate(csv_reader):
                        if idx == 0:
                            prev_headers = row
                        else:
                            prev_values.append(row)
            except FileNotFoundError:
                pass

            if not prev_headers:
                with open(file_path, 'w', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file)
                    csv_writer.writerow(headers)
                    csv_writer.writerow(values)
                return len(headers), tuple(headers), 1

            merged_headers = list(prev_headers)
            seen_names = set(prev_headers)
            for name in headers:
                if name not in seen_names:
                    if len(merged_headers) >= WEBRTC_CSV_MAX_HEADERS:
                        logger_metrics.warning(
                            "WebRTC Statistics header width capped at %d columns; "
                            "dropping additional fields", WEBRTC_CSV_MAX_HEADERS)
                        break
                    merged_headers.append(name)
                    seen_names.add(name)

            if len(prev_values) > WEBRTC_CSV_MAX_RETAINED_ROWS:
                prev_values = prev_values[-WEBRTC_CSV_MAX_RETAINED_ROWS:]

            prev_index = {name: pos for pos, name in enumerate(prev_headers)}
            new_index = {name: pos for pos, name in enumerate(headers)}

            def remap(row_values: List[Any], src_index: Dict[str, int]) -> List[Any]:
                out = []
                for name in merged_headers:
                    pos = src_index.get(name)
                    if pos is not None and pos < len(row_values):
                        out.append(row_values[pos])
                    else:
                        out.append("NaN")
                return out

            remapped_prev = [remap(row, prev_index) for row in prev_values]
            remapped_new = remap(values, new_index)

            tmp_path = file_path + ".tmp"
            with open(tmp_path, 'w', newline='') as stats_file:
                csv_writer = csv.writer(stats_file)
                csv_writer.writerow(merged_headers)
                csv_writer.writerows(remapped_prev)
                csv_writer.writerow(remapped_new)
            os.replace(tmp_path, file_path)

            logger_metrics.debug("WebRTC Statistics file {} rewritten with updated schema".format(file_path))
            return len(merged_headers), tuple(merged_headers), len(remapped_prev) + 1
        except Exception as e:
            logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))
            return prev_len, prev_names, prev_rows

    async def initialize_webrtc_csv_file(self, webrtc_stats_dir: str = '/tmp') -> None:
        """Points CSV capture at fresh timestamped files for a new connection.

        The header state is reset under `_csv_lock` off the loop thread: a CSV
        rewrite in flight on the executor may hold the lock, so taking it here
        would stall the event loop, and without it a worker could read a torn
        `(len, names)` pair and wrongly rewrite.
        """
        dt = datetime.now()
        timestamp = dt.strftime("%Y-%m-%d:%H:%M:%S")
        self.stats_video_file_path = '{}/selkies-stats-video-{}.csv'.format(webrtc_stats_dir, timestamp)
        self.stats_audio_file_path = '{}/selkies-stats-audio-{}.csv'.format(webrtc_stats_dir, timestamp)
        await asyncio.to_thread(self._reset_csv_header_state)

    def _reset_csv_header_state(self) -> None:
        with self._csv_lock:
            self.prev_stats_video_header_len = None
            self.prev_stats_audio_header_len = None
            self.prev_stats_video_header_names = None
            self.prev_stats_audio_header_names = None
            self.stats_video_row_count = 0
            self.stats_audio_row_count = 0


logger_system = logging.getLogger("system_monitor")
logger_system.setLevel(logging.INFO)

logger_gpu = logging.getLogger("gpu_monitor")
logger_gpu.setLevel(logging.INFO)

class SystemMonitor:
    """Periodically samples CPU and memory usage via psutil.

    The latest sample is exposed on `cpu_percent`, `mem_total`, and
    `mem_used`; the optional async `on_timer` callback fires once per period
    with the current timestamp. psutil calls run in a worker thread so
    sampling never blocks the event loop.
    """

    def __init__(self, period: int = 1, enabled: bool = True):
        self.period = max(1, int(period))
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self.task: Optional[asyncio.Task] = None
        self.cpu_percent: float = 0
        self.mem_total: int = 0
        self.mem_used: int = 0

        self.on_timer: Optional[Callable[[float], Awaitable[None]]] = None

    def start(self) -> None:
        """Starts the sampling task; no-op when disabled."""
        if not self.enabled:
            return
        self.stop_event.clear()
        self.task = asyncio.create_task(self._monitor_loop())
        logger_system.info("System monitor started")

    def _get_system_metrics(self) -> Tuple[float, int, int]:
        """Returns `(cpu_percent, mem_total_bytes, mem_used_bytes)`; blocking."""
        cpu = psutil.cpu_percent()
        mem = psutil.virtual_memory()
        return cpu, mem.total, mem.used

    async def _monitor_loop(self) -> None:
        """Samples until stopped."""
        try:
            while not self.stop_event.is_set():
                self.cpu_percent, self.mem_total, self.mem_used = await asyncio.to_thread(
                    self._get_system_metrics
                )
                if self.on_timer:
                    await self.on_timer(time.time())

                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=self.period)
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_system.error(f"System monitor error: {e}", exc_info=True)
        finally:
            logger_system.debug("System monitor loop exited")

    async def stop(self) -> None:
        """Signals the loop to exit and waits for the task to finish."""
        self.stop_event.set()
        if self.task:
            await self.task
        logger_system.info("System monitor stopped")

class GPUMonitor:
    """Periodically samples GPU load and memory for the pipeline's card.

    Each sample is delivered through the optional async `on_stats` callback
    as `(load, mem_total, mem_used)`. GPU queries run in a worker thread so
    sampling never blocks the event loop. When no GPU is found on the first
    probe, the loop exits instead of polling forever.

    Attributes:
        dri_node: The render node the pipeline captures/encodes on; stats
            must describe the same card, so `get_gpus` filters by it when set.
    """

    def __init__(self, gpu_id: int = 0, period: int = 1, enabled: bool = True, dri_node: str = ""):
        self.period = max(1, int(period))
        self.enabled = enabled
        self.gpu_id = gpu_id
        self.dri_node = dri_node
        self.stop_event = asyncio.Event()
        self.task: Optional[asyncio.Task] = None
        self.on_stats: Optional[Callable[..., Awaitable[None]]] = None

    def start(self) -> None:
        """Starts the sampling task; no-op when disabled."""
        if not self.enabled:
            return
        self.stop_event.clear()
        self.task = asyncio.create_task(self._monitor_loop())
        logger_gpu.info("GPU monitor started")

    def _get_gpu_stats(self) -> Optional[Tuple]:
        """Returns `(load, mem_total, mem_used)` for the target GPU; blocking.

        A `dri_node` match already narrows `get_gpus` to the pipeline's card;
        `gpu_id` indexes only the unfiltered list.

        Returns:
            The stats tuple, or None when the GPU cannot be found or queried.
        """
        try:
            gpus = gpu_stats.get_gpus(self.dri_node)
            idx = 0 if (self.dri_node and len(gpus) == 1) else self.gpu_id
            if not gpus or idx >= len(gpus):
                return None
            gpu = gpus[idx]
            return (gpu.load, gpu.memoryTotal, gpu.memoryUsed)
        except Exception as e:
            logger_gpu.warning(f"Error while fetching GPU stats: {e}")
            return None

    async def _monitor_loop(self) -> None:
        """Samples until stopped; exits at once when the first probe finds no GPU.

        Nothing is substituted for a missing GPU: CPU load and system memory
        are `SystemMonitor`'s, and the GPU gauge contract (fractional load, MiB
        memory) cannot carry them without unit errors.
        """
        try:
            if await asyncio.to_thread(self._get_gpu_stats) is None:
                logger_gpu.info(
                    f"No GPU with ID {self.gpu_id} found; GPU stats disabled "
                    "(CPU and system memory are reported by the system monitor)."
                )
                return
            while not self.stop_event.is_set():
                stats = await asyncio.to_thread(self._get_gpu_stats)
                if stats is not None and self.on_stats:
                    load, mem_total, mem_used = stats
                    await self.on_stats(load, mem_total, mem_used)
                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=self.period)
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_gpu.error(f"GPU monitor error: {e}", exc_info=True)
        finally:
            logger_gpu.debug("GPU monitor loop exited")

    async def stop(self) -> None:
        """Signals the loop to exit and waits for the task to finish."""
        self.stop_event.set()
        if self.task:
            await self.task
        logger_gpu.info("GPU monitor stopped")
