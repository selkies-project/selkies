# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""WebRTC ICE configuration: builders and parsers for RTCPeerConnection-style
ICE server JSON, the prioritized `get_rtc_configuration` resolver, and refresh
monitors (HMAC shared-secret, TURN REST, Cloudflare, and a config-file watcher)
that push updated credentials through an `on_rtc_config` callback. The
periodic loops share one shutdown idiom: an `asyncio.Event` waited on with a
timeout, so `stop()` interrupts the sleep immediately instead of waiting out
the period.
"""

import json
import time
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
from typing import Callable, Tuple, List, Dict, Any, Optional, Union
from watchdog.events import FileClosedEvent, FileSystemEventHandler

import os
import stat



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
