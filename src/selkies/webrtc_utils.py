
import json
import time
import psutil
import GPUtil
import asyncio
import aiohttp
import aiofiles
import logging
import urllib.parse
import hashlib
import hmac
import base64
from aiohttp import web
from watchdog.observers import Observer
from typing import Tuple, List, Dict, Any, Optional, Union
from watchdog.events import FileClosedEvent, FileSystemEventHandler

import os
import csv
import stat
import threading
from datetime import datetime
from collections import OrderedDict
from prometheus_client import generate_latest, REGISTRY
from prometheus_client import Gauge, Histogram, Info


# ---------------- RTC ICE config utilities ----------------

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
    if host and ":" in host and not (host.startswith("[") and host.endswith("]")):
        return f"[{host}]"
    return host


def _extract_host_port(url: str, scheme: str, default_port: int) -> Tuple[Optional[str], int]:
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


async def _dispatch_rtc_callback(callback, stun_servers: List[str], turn_servers: List[str], rtc_config: bytes) -> None:
    if asyncio.iscoroutinefunction(callback):
        await callback(stun_servers, turn_servers, rtc_config)
        return
    await asyncio.to_thread(callback, stun_servers, turn_servers, rtc_config)


def _log_asyncio_task_error(task: asyncio.Task) -> None:
    try:
        task.result()
    except asyncio.CancelledError:
        # Expected when pending callback tasks are cancelled at shutdown.
        pass
    except Exception as e:
        logger_rtcice.warning(f"Error in on_rtc_config callback task: {e}")


def _schedule_rtc_callback(loop: asyncio.AbstractEventLoop, callback, stun_servers: List[str], turn_servers: List[str], rtc_config: bytes) -> None:
    task = loop.create_task(_dispatch_rtc_callback(callback, stun_servers, turn_servers, rtc_config))
    task.add_done_callback(_log_asyncio_task_error)


def generate_rtc_config(turn_host, turn_port, shared_secret, user, protocol='udp', turn_tls=False, stun_host=None, stun_port=None):
    # Use shared secret to generate HMAC credential

    # Sanitize user for credential compatibility
    user = user.replace(":", "-")

    # Credential expires in 24 hours
    expiry_hour = 24

    exp = int(time.time()) + expiry_hour * 3600
    username = "{}:{}".format(exp, user)

    # Generate HMAC credential
    hashed = hmac.new(bytes(shared_secret, "utf-8"), bytes(username, "utf-8"), hashlib.sha1).digest()
    password = base64.b64encode(hashed).decode()

    # Configure STUN servers
    stun_list = []
    seen_stun = set()
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
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self):
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("HMAC RTC monitor started")

    async def _monitor_loop(self):
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

    async def stop(self):
        self.stop_event.set()
        if self._task:
            await self._task

class RESTRTCMonitor:
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
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self):
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("TURN REST RTC monitor started")

    async def _monitor_loop(self):
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

    async def stop(self):
        self.stop_event.set()
        if self._task:
            await self._task

class CloudflareRTCMonitor:
    """Periodically refreshes Cloudflare TURN credentials before they expire.

    Cloudflare credentials are minted with a finite TTL (default 24h); without a
    refresh, a server running longer than the TTL would hand out expired TURN
    credentials to newly connecting clients until restart.
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
        # Refresh well within the credential lifetime (default: half the TTL).
        self.period = period if period is not None else max(60, ttl // 2)
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self._task: Optional[asyncio.Task] = None
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    def start(self):
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())
        logger_rtcice.info("Cloudflare TURN RTC monitor started")

    async def _monitor_loop(self):
        try:
            while not self.stop_event.is_set():
                # Wait first: the initial fetch already happened at startup.
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

    async def stop(self):
        self.stop_event.set()
        if self._task:
            await self._task

class RTCConfigFileMonitor(FileSystemEventHandler):
    def __init__(self, rtc_file: str, enabled: bool = True):
        self.enabled = enabled
        self.rtc_file = os.path.abspath(rtc_file)
        self.watch_dir = os.path.dirname(self.rtc_file) or "."
        self._loop = asyncio.get_running_loop()
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

        self.observer = Observer()
        self.observer.schedule(self, self.watch_dir, recursive=False)

    async def start(self):
        if not self.enabled:
            return

        await asyncio.to_thread(self.observer.start)
        logger_rtcice.info(f"RTC config file monitor started for: {self.rtc_file}")

    def _shutdown_observer(self):
        if self.observer.is_alive():
            self.observer.stop()
            self.observer.join()  # Wait for the thread to terminate

    async def stop(self):
        if not self.enabled:
            return

        await asyncio.to_thread(self._shutdown_observer)
        logger_rtcice.info("RTC config file monitor stopped")

    def _reload_config(self, src_path: str):
        """Read, parse, and dispatch the updated RTC config. Runs on the watchdog thread."""
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

    # These methods override FileSystemEventHandler. React to in-place writes
    # (on_closed) and to the atomic write-temp-then-rename pattern, which
    # surfaces as a move (or create) of the target rather than a close.
    def on_closed(self, event):
        if not isinstance(event, FileClosedEvent):
            return
        if os.path.abspath(event.src_path) != self.rtc_file:
            return
        self._reload_config(event.src_path)

    def on_moved(self, event):
        dest = getattr(event, "dest_path", None)
        if dest and os.path.abspath(dest) == self.rtc_file:
            self._reload_config(dest)

    def on_created(self, event):
        if os.path.abspath(event.src_path) == self.rtc_file:
            self._reload_config(event.src_path)

def make_turn_rtc_config_json_legacy(
    turn_host: str,
    turn_port: int,
    username: str,
    password: str,
    protocol: str = 'udp',
    turn_tls: bool = False,
    stun_host: str = None,
    stun_port: int = None
) -> str:
    """COnverts given rtc details to json format for legacy components"""
    stun_list: List[str] = []
    seen_stun: set = set()
    if stun_host is not None and stun_port is not None:
        _append_stun_url(stun_list, seen_stun, str(stun_host), stun_port)
    _append_stun_url(stun_list, seen_stun, str(turn_host), turn_port)
    for default_host, default_port in DEFAULT_STUN_SERVERS:
        _append_stun_url(stun_list, seen_stun, default_host, default_port)

    rtc_config = {}
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

        # Convert 'uris' to 'urls' for compatibility with RTCPeerConnection spec
        if "uris" in ice_server and "urls" not in ice_server:
            ice_server["urls"] = ice_server.pop("uris")
            normalized_config = True

        # Convert TURN REST-style password field to RTCPeerConnection-style credential field
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
    """
    Asynchronously fetches TURN config from a REST API
    Returns a tuple containing STUN URIs, TURN URIs, and the raw/normalized RTC config JSON bytes.
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
    """
    Asynchronously obtains TURN credentials from the Cloudflare Calls API using aiohttp.
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
    """Attempts to configure RTC using Cloudflare TURN."""
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
    """Return True only if `path` is safe to trust as an RTC config source.

    The RTC config file overrides all STUN/TURN settings, and its default
    location (e.g. /tmp/rtc.json) lives in a world-writable directory where a
    different local user could plant or alter it. Require the file to be owned by
    root or the current user and to be writable by neither group nor others.
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
    """Attempts to configure RTC from a local JSON file."""
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
    """Attempts to configure RTC from a custom TURN REST API."""
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
    """Attempts to configure RTC using long-term TURN credentials."""
    if not (args.turn_username and args.turn_password and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using long-term username/password for TURN credentials.")
    config_json = make_turn_rtc_config_json_legacy(
        args.turn_host, args.turn_port, args.turn_username, args.turn_password,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(config_json)

def try_hmac_turn(args: Any, username: str, protocol: str, use_tls: bool) -> Optional[Tuple[List[str], List[str], bytes]]:
    """Attempts to configure RTC using short-term HMAC credentials."""
    if not (args.turn_shared_secret and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using short-term shared secret HMAC for TURN credentials.")
    hmac_data = generate_rtc_config(
        args.turn_host, args.turn_port, args.turn_shared_secret, username,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(hmac_data)

async def get_rtc_configuration(args: Any) -> Tuple[List[str], List[str], bytes, Dict[str, bool]]:
    """
    Determines and fetches the RTC configuration based on a prioritized sequence of methods.

    Priority Order:
    1. Cloudflare TURN API
    2. Local RTC Config JSON file
    3. Custom TURN REST API
    4. Long-term TURN credentials (username/password)
    5. Short-term TURN credentials (shared secret HMAC)
    6. Default built-in configuration
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

    # Try each method in order of priority, returning on the first success
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

    # Fallback to default if all other methods fail
    logger_rtcice.warning("No valid TURN server information found, using default RTC config.")
    return *parse_rtc_config(DEFAULT_RTC_CONFIG), monitoring_utilities_used


# ---------------- Metrics utilities ----------------

logger_metrics = logging.getLogger("metrics")
logger_metrics.setLevel(logging.INFO)

FPS_HIST_BUCKETS = (0, 20, 40, 60)

# Bounds for the diagnostic WebRTC stats CSV. Field names come from the
# (untrusted) browser client; cap the union header width and the number of
# retained rows carried across a schema-change rewrite so a churning/hostile
# client cannot grow the file unbounded.
WEBRTC_CSV_MAX_HEADERS = 2048
WEBRTC_CSV_MAX_RETAINED_ROWS = 100000

class Metrics:
    def __init__(self, using_webrtc_csv: bool = False):
        self.using_webrtc_csv = using_webrtc_csv

        self.fps = Gauge('fps', 'Frames per second observed by client')
        self.fps_hist = Histogram('fps_hist', 'Histogram of FPS observed by client', buckets=FPS_HIST_BUCKETS)
        self.gpu_utilization = Gauge('gpu_utilization', 'Utilization percentage reported by GPU')
        self.latency = Gauge('latency', 'Latency observed by client')
        self.webrtc_statistics = Info('webrtc_statistics', 'WebRTC Statistics from the client')
        self.stats_video_file_path: Optional[str] = None
        self.stats_audio_file_path: Optional[str] = None
        self.prev_stats_video_header_len: Optional[int]  = None
        self.prev_stats_audio_header_len: Optional[int]  = None
        # Track header names so a same-count field swap still triggers a remap.
        self.prev_stats_video_header_names: Optional[Tuple[str, ...]] = None
        self.prev_stats_audio_header_names: Optional[Tuple[str, ...]] = None
        # Serializes CSV writes (which run in worker threads) so concurrent stat
        # messages cannot interleave rows or race the shared prev_stats state.
        self._csv_lock = threading.Lock()
        # Hold strong references to in-flight write tasks so they are not garbage
        # collected before completion (and their exceptions stay observed).
        self._csv_tasks: set = set()

    def set_fps(self, fps):
        self.fps.set(fps)
        self.fps_hist.observe(fps)

    def set_gpu_utilization(self, utilization):
        self.gpu_utilization.set(utilization)

    def set_latency(self, latency_ms):
        self.latency.set(latency_ms)
    
    def unregister(self):
        """Unregister all metrics from the global registry."""
        # Cancel any in-flight CSV write tasks so they do not outlive teardown.
        # Iterate a copy: the done-callback mutates the set as tasks finish.
        for task in list(self._csv_tasks):
            task.cancel()
        try:
            REGISTRY.unregister(self.fps)
            REGISTRY.unregister(self.fps_hist)
            REGISTRY.unregister(self.gpu_utilization)
            REGISTRY.unregister(self.latency)
            REGISTRY.unregister(self.webrtc_statistics)
        except KeyError:
            # Metrics might have already been unregistered
            pass

    async def handle_metrics_request(self, request: web.Request):
        """Async handler for metrics endpoint"""
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

    async def set_webrtc_stats(self, webrtc_stat_type: str, webrtc_stats: str) -> None:
        sanitized_stats = await asyncio.to_thread(self._parse_and_sanitize_stats, webrtc_stats)
        if self.using_webrtc_csv:
            is_audio = webrtc_stat_type == "_stats_audio"
            csv_path = self.stats_audio_file_path if is_audio else self.stats_video_file_path
            task = asyncio.create_task(asyncio.to_thread(self.write_webrtc_stats_csv, sanitized_stats, csv_path, is_audio))
            self._csv_tasks.add(task)
            task.add_done_callback(self._csv_tasks.discard)
        # Cheap inline dict copy; not worth a thread dispatch.
        self.webrtc_statistics.info(sanitized_stats)

    def _parse_and_sanitize_stats(self, webrtc_stats: str) -> OrderedDict:
        return self.sanitize_json_stats(json.loads(webrtc_stats))

    def sanitize_json_stats(self, obj_list: List[Dict[str, Any]]) -> OrderedDict:
        """A helper function to process data to a structure
           For example: reportName.fieldName:value
        """
        obj_type = set()
        sanitized_stats = OrderedDict()
        # Per-type occurrence counter for a content-stable dedup suffix. Keying
        # the suffix on the global enumerate() index made every column name
        # shift whenever the stats list reordered/inserted, which churned the
        # CSV schema (full rewrites) and grew the union header unbounded. A
        # per-type counter (and the entry 'id' when present) keeps a given
        # logical stat mapped to the same column across messages.
        type_counts: Dict[str, int] = {}
        for entry in obj_list:
            # Stats come from the (untrusted) browser client; skip entries that
            # are not dicts and default a missing/non-string 'type'.
            if not isinstance(entry, dict):
                continue
            base_key = entry.get('type')
            if not isinstance(base_key, str):
                base_key = "unknown"
            # Per-base-type occurrence index, stable across reorders/inserts.
            occurrence = type_counts.get(base_key, 0)
            type_counts[base_key] = occurrence + 1
            curr_key = base_key
            if curr_key in obj_type:
                # Prefer the entry's stable 'id'; fall back to the per-type
                # occurrence index. Both are stable across reorders/inserts,
                # unlike the prior global loop index.
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

    def write_webrtc_stats_csv(self, obj: dict, file_path: str, is_audio: bool = False) -> None:
        """Writes the WebRTC statistics to a CSV file.

        Arguments:
            obj_list {[list of object]} -- list of Python objects/dictionary
            is_audio {bool} -- whether this is the audio stream (passed by the
                caller rather than re-derived from the file path).
        """

        dt = datetime.now()
        timestamp = dt.strftime("%d/%B/%Y:%H:%M:%S")
        # Writes run in worker threads; serialize them.
        with self._csv_lock:
            try:
                # Prepare the data
                headers = ["timestamp"]
                headers += obj.keys()

                # Upon reconnections the client could send a redundant objs just discard them
                if len(headers) < 15:
                    return

                # Pass raw values to csv.writer; pre-quoting would be double-quoted.
                values = [timestamp]
                values.extend(obj.values())

                header_names = tuple(headers)
                prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
                prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names

                if prev_len is not None and prev_names != header_names:
                    if prev_names is not None and frozenset(prev_names) == frozenset(header_names):
                        # Same field SET, different order only: no schema change.
                        # Avoid an O(N) file rewrite (which previously fired on
                        # every reorder, e.g. ICE churn reshuffling the stats
                        # list). Append a single row remapped into the stored
                        # column order so the CSV stays consistent.
                        value_by_name = dict(zip(headers, values))
                        remapped = [value_by_name.get(name, "NaN") for name in prev_names]
                        with open(file_path, 'a+', newline='') as stats_file:
                            csv.writer(stats_file, quotechar='"').writerow(remapped)
                        return

                    # The field set changed: rewrite the CSV with the merged
                    # schema. Must run with no open handle on file_path, as
                    # os.replace() onto an open file fails on Windows.
                    new_len, new_names = self.update_webrtc_stats_csv(file_path, headers, values, is_audio)
                    if is_audio:
                        self.prev_stats_audio_header_len = new_len
                        self.prev_stats_audio_header_names = new_names
                    else:
                        self.prev_stats_video_header_len = new_len
                        self.prev_stats_video_header_names = new_names
                    return

                with open(file_path, 'a+', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file, quotechar='"')
                    if prev_len is None:
                        csv_writer.writerow(headers)
                        csv_writer.writerow(values)
                        if is_audio:
                            self.prev_stats_audio_header_len = len(headers)
                            self.prev_stats_audio_header_names = header_names
                        else:
                            self.prev_stats_video_header_len = len(headers)
                            self.prev_stats_video_header_names = header_names
                    else:
                        csv_writer.writerow(values)

            except Exception as e:
                logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))

    def update_webrtc_stats_csv(self, file_path: str, headers: List[str], values: List[Any], is_audio: bool = False):
        """Rewrites the CSV when the set of stat fields changes, aligning the
           previously-stored rows onto the new (union) header layout by field
           name. Missing fields are filled with "NaN". Returns a tuple of the
           new header length and the new header-name tuple, or the previous
           length/names on failure.
        """
        prev_len = self.prev_stats_audio_header_len if is_audio else self.prev_stats_video_header_len
        prev_names = self.prev_stats_audio_header_names if is_audio else self.prev_stats_video_header_names

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
                # File deleted externally since the last write; recreate it
                # below with the current schema.
                pass

            # No usable prior content (empty or header-only file): (re)write the
            # current schema plus this row.
            if not prev_headers:
                with open(file_path, 'w', newline='') as stats_file:
                    csv_writer = csv.writer(stats_file)
                    csv_writer.writerow(headers)
                    csv_writer.writerow(values)
                return len(headers), tuple(headers)

            # Union header: keep the previous column order, then append any
            # genuinely new fields at the end. Remap every previous row and the
            # new row onto that union by field name, filling gaps with "NaN".
            # The union width is capped so untrusted/churning field names cannot
            # widen every future row without bound.
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

            # Bound retained history: carry only the most recent rows forward.
            if len(prev_values) > WEBRTC_CSV_MAX_RETAINED_ROWS:
                prev_values = prev_values[-WEBRTC_CSV_MAX_RETAINED_ROWS:]

            prev_index = {name: pos for pos, name in enumerate(prev_headers)}
            new_index = {name: pos for pos, name in enumerate(headers)}

            def remap(row_values, src_index):
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

            # Rewrite via a temp file + atomic replace so an interrupted rewrite
            # cannot truncate or corrupt the existing stats.
            tmp_path = file_path + ".tmp"
            with open(tmp_path, 'w', newline='') as stats_file:
                csv_writer = csv.writer(stats_file)
                csv_writer.writerow(merged_headers)
                csv_writer.writerows(remapped_prev)
                csv_writer.writerow(remapped_new)
            os.replace(tmp_path, file_path)

            logger_metrics.debug("WebRTC Statistics file {} rewritten with updated schema".format(file_path))
            return len(merged_headers), tuple(merged_headers)
        except Exception as e:
            logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))
            return prev_len, prev_names

    def initialize_webrtc_csv_file(self, webrtc_stats_dir: str ='/tmp'):
        """Initializes the WebRTC Statistics file upon every new WebRTC connection
        """
        dt = datetime.now()
        timestamp = dt.strftime("%Y-%m-%d:%H:%M:%S")
        self.stats_video_file_path = '{}/selkies-stats-video-{}.csv'.format(webrtc_stats_dir, timestamp)
        self.stats_audio_file_path = '{}/selkies-stats-audio-{}.csv'.format(webrtc_stats_dir, timestamp)
        # Reset under _csv_lock so a worker thread cannot observe a torn
        # (len, names) pair (e.g. prev_len=old while prev_names=None) and
        # spuriously enter the rewrite branch.
        with self._csv_lock:
            self.prev_stats_video_header_len = None
            self.prev_stats_audio_header_len = None
            self.prev_stats_video_header_names = None
            self.prev_stats_audio_header_names = None


# ---------------- Monitoring utilities ----------------

logger_system = logging.getLogger("system_monitor")
logger_system.setLevel(logging.INFO)

logger_gpu = logging.getLogger("gpu_monitor")
logger_gpu.setLevel(logging.INFO)

class SystemMonitor:
    def __init__(self, period: int = 1, enabled: bool = True):
        self.period = max(1, int(period))
        self.enabled = enabled
        self.stop_event = asyncio.Event()
        self.task: Optional[asyncio.Task] = None
        self.cpu_percent = 0
        self.mem_total = 0
        self.mem_used = 0

        self.on_timer = None

    def start(self):
        if not self.enabled:
            return
        self.stop_event.clear()
        self.task = asyncio.create_task(self._monitor_loop())
        logger_system.info("System monitor started")

    def _get_system_metrics(self):
        cpu = psutil.cpu_percent()
        mem = psutil.virtual_memory()
        return cpu, mem.total, mem.used

    async def _monitor_loop(self):
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

    async def stop(self):
        self.stop_event.set()
        if self.task:
            await self.task
        logger_system.info("System monitor stopped")

class GPUMonitor:
    def __init__(self, gpu_id: int = 0, period: int = 1, enabled: bool = True):
        self.period = max(1, int(period))
        self.enabled = enabled
        self.gpu_id = gpu_id
        self.stop_event = asyncio.Event()
        self.task: Optional[asyncio.Task] = None
        self.on_stats = None

    def start(self) -> None:
        if not self.enabled:
            return
        self.stop_event.clear()
        self.task = asyncio.create_task(self._monitor_loop())
        logger_gpu.info("GPU monitor started")

    def _get_gpu_stats(self) -> Optional[Tuple]:
        try:
            gpus = GPUtil.getGPUs()
            if not gpus or self.gpu_id >= len(gpus):
                return None
            gpu = gpus[self.gpu_id]
            return (gpu.load, gpu.memoryTotal, gpu.memoryUsed)
        except Exception as e:
            # GPUtil can sometimes raise unexpected errors
            logger_gpu.warning(f"Error while fetching GPU stats: {e}")
            return None

    async def _monitor_loop(self):
        try:
            while not self.stop_event.is_set():
                stats = await asyncio.to_thread(self._get_gpu_stats)

                if stats is None:
                    logger_gpu.warning(f"Could not find GPU with ID {self.gpu_id}. Retrying in {self.period}s...")
                elif self.on_stats:
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

    async def stop(self):
        self.stop_event.set()
        if self.task:
            await self.task
        logger_gpu.info("GPU monitor stopped")
