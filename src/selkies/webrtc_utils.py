
import json
import time
import psutil
import GPUtil
import asyncio
import aiohttp
import aiofiles
import logging
import urllib.parse
from watchdog.observers import Observer
from .signaling_server import generate_rtc_config
from typing import Tuple, List, Dict, Any, Optional
from watchdog.events import FileClosedEvent, FileSystemEventHandler

import os
import csv
import json
import random
import asyncio
import logging
import argparse
from datetime import datetime
from http.server import HTTPServer
from collections import OrderedDict
from prometheus_client import MetricsHandler
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
        self.period = min(int(period), 60)
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
                    await asyncio.to_thread(self.on_rtc_config, stun_servers, turn_servers, rtc_config)
                except Exception as e:
                    logger_rtcice.warning(f"could not fetch TURN HMAC config in periodic monitor: {e}")

                _, pending = await asyncio.wait([self.stop_event.wait()], timeout=self.period)
                for task in pending:
                    task.cancel()
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
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

    async def start(self):
        if not self.enabled:
            return
        self.stop_event.clear()
        self._task = asyncio.create_task(self._monitor_loop())

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
                        self.turn_rest_tls_header
                    )
                    await asyncio.to_thread(self.on_rtc_config, stun_servers, turn_servers, rtc_config)
                except Exception as e:
                    logger_rtcice.warning(f"could not fetch TURN REST config in periodic monitor: {e}")

                _, pending = await asyncio.wait([self.stop_event.wait()], timeout=self.period)
                for task in pending:
                    task.cancel()
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

class RTCConfigFileMonitor(FileSystemEventHandler):
    def __init__(self, rtc_file: str, enabled: bool = True):
        self.enabled = enabled
        self.rtc_file = rtc_file
        self._loop = asyncio.get_running_loop()
        self.on_rtc_config = lambda stun_servers, turn_servers, rtc_config: logger_rtcice.warning("unhandled on_rtc_config")

        self.observer = Observer()
        self.observer.schedule(self, self.rtc_file, recursive=False)

    async def start(self):
        if not self.enabled:
            return

        # Schedule this class itself to handle events for the specified file
        self.observer.schedule(self, self.rtc_file, recursive=False)
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

     # This method overrides the one in FileSystemEventHandler
    def on_closed(self, event):
        """
        Called by the watchdog thread when a file write is closed.
        This is a synchronous method.
        """
        if not isinstance(event, FileClosedEvent):
            return
        try:
            logger_rtcice.info(f"Detected RTC JSON file change: {event.src_path}")
            with open(self.rtc_file, 'rb') as f:
                data = f.read()

            stun_servers, turn_servers, rtc_config = parse_rtc_config(data)
            asyncio.run_coroutine_threadsafe(
                self.on_rtc_config(stun_servers, turn_servers, rtc_config),
                self._loop
            )
        except Exception as e:
            logger_rtcice.warning(f"Could not read or parse RTC JSON file: {self.rtc_file}: {e}")

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
    stun_list = ["stun:{}:{}".format(turn_host, turn_port)]
    if stun_host is not None and stun_port is not None and (stun_host != turn_host or str(stun_port) != str(turn_port)):
        stun_list.insert(0, "stun:{}:{}".format(stun_host, stun_port))
    if stun_host != "stun.l.google.com" or (str(stun_port) != "19302"):
        stun_list.append("stun:stun.l.google.com:19302")

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
            "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', turn_host, turn_port, protocol)
        ],
        "username": username,
        "credential": password
    })
    return json.dumps(rtc_config, indent=2)

def parse_rtc_config(data: bytes) -> Tuple[List[str], List[str], bytes]:
    ice_servers = json.loads(data)['iceServers']
    stun_uris = []
    turn_uris = []
    for ice_server in ice_servers:
        for url in ice_server.get("urls", []):
            if url.startswith("stun:"):
                stun_host = url.split(":")[1]
                stun_port = url.split(":")[2].split("?")[0]
                stun_uri = "stun://%s:%s" % (
                    stun_host,
                    stun_port
                )
                stun_uris.append(stun_uri)
            elif url.startswith("turn:"):
                turn_host = url.split(':')[1]
                turn_port = url.split(':')[2].split('?')[0]
                turn_user = ice_server['username']
                turn_password = ice_server['credential']
                turn_uri = "turn://%s:%s@%s:%s" % (
                    urllib.parse.quote(turn_user, safe=""),
                    urllib.parse.quote(turn_password, safe=""),
                    turn_host,
                    turn_port
                )
                turn_uris.append(turn_uri)
            elif url.startswith("turns:"):
                turn_host = url.split(':')[1]
                turn_port = url.split(':')[2].split('?')[0]
                turn_user = ice_server['username']
                turn_password = ice_server['credential']
                turn_uri = "turns://%s:%s@%s:%s" % (
                    urllib.parse.quote(turn_user, safe=""),
                    urllib.parse.quote(turn_password, safe=""),
                    turn_host,
                    turn_port
                )
                turn_uris.append(turn_uri)
    return stun_uris, turn_uris, data

async def fetch_turn_rest(
    uri: str,
    user: str,
    auth_header_username: str = 'x-auth-user',
    protocol: str = 'udp',
    header_protocol: str = 'x-turn-protocol',
    turn_tls: bool = False,
    header_tls: str = 'x-turn-tls'
) -> Tuple[List, List, Dict]:
    """
    Asynchronously fetches TURN config from a REST API
    """
    auth_headers = {
        auth_header_username: user,
        header_protocol: protocol,
        header_tls: 'true' if turn_tls else 'false'
    }

    async with aiohttp.ClientSession() as session:
        try:
            async with session.get(uri, headers=auth_headers) as response:
                # Raise an exception for 4xx or 5xx status codes
                response.raise_for_status()

                content = await response.read()
                if not content:
                    raise Exception("Data from REST API service was empty")
                return parse_rtc_config(content)
        except aiohttp.ClientResponseError as e:
            body = await e.response.text() if hasattr(e, 'response') else ''
            raise Exception(f"Error fetching REST API config: {e.status} {e.message}. Body: {body}") from e
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

    async with aiohttp.ClientSession() as session:
        try:
            async with session.post(uri, headers=auth_headers, json=data_payload) as response:
                response.raise_for_status()
                return await response.json()
        except aiohttp.ClientResponseError as e:
            body = await e.response.text() if hasattr(e, 'response') else ''
            raise Exception(f"Could not obtain Cloudflare TURN credentials: {e.status} {e.message}. Body: {body}") from e
        except aiohttp.ClientError as e:
            raise Exception(f"Network error while fetching Cloudflare credentials: {e}") from e

async def try_cloudflare(args: Any) -> Optional[Tuple[List, List, Dict]]:
    """Attempts to configure RTC using Cloudflare TURN."""
    if not args.enable_cloudflare_turn:
        return None

    if not (args.cloudflare_turn_token_id and args.cloudflare_turn_api_token):
        logger_rtcice.error("Cloudflare TURN is enabled but token ID and/or API token are missing.")
        return None

    try:
        json_config = await fetch_cloudflare_turn(args.cloudflare_turn_token_id, args.cloudflare_turn_api_token)
        logger_rtcice.info(f"Successfully fetched RTC configuration from Cloudflare: {json_config}")
        wrapped_config = json.dumps({"iceServers": [json_config["iceServers"]]})
        return parse_rtc_config(wrapped_config)
    except Exception as e:
        logger_rtcice.warning(f"Failed to fetch TURN config from Cloudflare: {e}")
        return None

async def try_json_file(args: Any) -> Optional[Tuple[List, List, Dict]]:
    """Attempts to configure RTC from a local JSON file."""
    if not os.path.exists(args.rtc_config_json):
        return None

    logger_rtcice.warning(f"Using JSON file '{args.rtc_config_json}' for RTC config, overrides all other STUN/TURN settings.")
    try:
        async with aiofiles.open(args.rtc_config_json, 'r') as f:
            content = await f.read()
            return parse_rtc_config(content)
    except Exception as e:
        logger_rtcice.error(f"Failed to read or parse RTC config file '{args.rtc_config_json}': {e}")
        return None

async def try_rest_api(args: Any, username: str, protocol: str, use_tls: bool) -> Optional[Tuple[List, List, Dict]]:
    """Attempts to configure RTC from a custom TURN REST API."""
    if not args.turn_rest_uri:
        return None

    try:
        config = await fetch_turn_rest(
            args.turn_rest_uri, username, args.turn_rest_username_auth_header,
            protocol, args.turn_rest_protocol_header, use_tls, args.turn_rest_tls_header
        )
        logger_rtcice.info("Using TURN REST API for RTC configuration.")
        return config
    except Exception as e:
        logger_rtcice.warning(f"Error fetching from TURN REST API, falling back to other methods: {e}")
        return None

def try_legacy_turn(args: Any, protocol: str, use_tls: bool) -> Optional[Tuple[List, List, Dict]]:
    """Attempts to configure RTC using long-term TURN credentials."""
    if not (args.turn_username and args.turn_password and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using long-term username/password for TURN credentials.")
    config_json = make_turn_rtc_config_json_legacy(
        args.turn_host, args.turn_port, args.turn_username, args.turn_password,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(config_json)

def try_hmac_turn(args: Any, username: str, protocol: str, use_tls: bool) -> Optional[Tuple[List, List, Dict]]:
    """Attempts to configure RTC using short-term HMAC credentials."""
    if not (args.turn_shared_secret and args.turn_host and args.turn_port):
        return None

    logger_rtcice.info("Using short-term shared secret HMAC for TURN credentials.")
    hmac_data = generate_rtc_config(
        args.turn_host, args.turn_port, args.turn_shared_secret, username,
        protocol, use_tls, args.stun_host, args.stun_port
    )
    return parse_rtc_config(hmac_data)

async def get_rtc_configuration(args: Any) -> Tuple[List, List, Dict]:
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

    # Try each method in order of priority, returning on the first success
    if config := await try_cloudflare(args):
        return config

    if config := await try_json_file(args):
        return config

    if config := await try_rest_api(args, turn_rest_username, turn_protocol, using_turn_tls):
        return config

    if config := try_legacy_turn(args, turn_protocol, using_turn_tls):
        return config

    if config := try_hmac_turn(args, turn_rest_username, turn_protocol, using_turn_tls):
        return config

    # Fallback to default if all other methods fail
    logger_rtcice.warning("No valid TURN server information found, using default RTC config.")
    return parse_rtc_config(DEFAULT_RTC_CONFIG)


# ---------------- Metrics utilities ----------------

logger_metrics = logging.getLogger("metrics")
logger_metrics.setLevel(logging.INFO)

FPS_HIST_BUCKETS = (0, 20, 40, 60)

class Metrics:
    def __init__(self, port: int = 8000, using_webrtc_csv: bool = False):
        self.port = port
        self.server = HTTPServer(('localhost', self.port), MetricsHandler)
        self._task: Optional[asyncio.Task] = None

        self.fps = Gauge('fps', 'Frames per second observed by client')
        self.fps_hist = Histogram('fps_hist', 'Histogram of FPS observed by client', buckets=FPS_HIST_BUCKETS)
        self.gpu_utilization = Gauge('gpu_utilization', 'Utilization percentage reported by GPU')
        self.latency = Gauge('latency', 'Latency observed by client')
        self.webrtc_statistics = Info('webrtc_statistics', 'WebRTC Statistics from the client')
        self.using_webrtc_csv = using_webrtc_csv
        self.stats_video_file_path: Optional[str] = None
        self.stats_audio_file_path: Optional[str] = None
        self.prev_stats_video_header_len: Optional[str]  = None
        self.prev_stats_audio_header_len: Optional[str]  = None

    def set_fps(self, fps):
        self.fps.set(fps)
        self.fps_hist.observe(fps)

    def set_gpu_utilization(self, utilization):
        self.gpu_utilization.set(utilization)

    def set_latency(self, latency_ms):
        self.latency.set(latency_ms)

    def start_http(self):
        if self._task is not None:
            logger_metrics.warning("Metrics server is already runningg")
            return
        self._task = asyncio.create_task(asyncio.to_thread(self.server.serve_forever))
        logger_metrics.info(f"Metrics server started on port {self.port}")

    async def stop_http(self):
        if self._task is None:
            logger_metrics.warning("Metrics server is not running, might have already been stopped")
            return
        if self.server:
            self.server.shutdown()
            self.server.server_close()
        if self._task and not self._task.done():
            await self._task
        self._task = None
        logger_metrics.info(f"Metrics server stopped")

    async def set_webrtc_stats(self, webrtc_stat_type: str, webrtc_stats: str) -> None:
        webrtc_stats_obj = await asyncio.to_thread(json.loads, webrtc_stats)
        sanitized_stats = await asyncio.to_thread(self.sanitize_json_stats, webrtc_stats_obj)
        if self.using_webrtc_csv:
            if webrtc_stat_type == "_stats_audio":
                asyncio.create_task(asyncio.to_thread(self.write_webrtc_stats_csv, sanitized_stats, self.stats_audio_file_path))
            else:
                asyncio.create_task(asyncio.to_thread(self.write_webrtc_stats_csv, sanitized_stats, self.stats_video_file_path))
        await asyncio.to_thread(self.webrtc_statistics.info, sanitized_stats)

    def sanitize_json_stats(self, obj_list: List[Dict[str, Any]]) -> OrderedDict:
        """A helper function to process data to a structure
           For example: reportName.fieldName:value
        """
        obj_type = []
        sanitized_stats = OrderedDict()
        for i in range(len(obj_list)):
            curr_key = obj_list[i].get('type')
            if  curr_key in obj_type:
                # Append id at suffix to eliminate duplicate types
                curr_key = curr_key + str("-") + obj_list[i].get('id')
                obj_type.append(curr_key)
            else:
                obj_type.append(curr_key)

            for key, val in obj_list[i].items():
                unique_type = curr_key + str(".")  + key
                if not isinstance(val, str):
                    sanitized_stats[unique_type] =  str(val)
                else:
                    sanitized_stats[unique_type] = val

        return sanitized_stats

    def write_webrtc_stats_csv(self, obj: dict, file_path: str) -> None:
        """Writes the WebRTC statistics to a CSV file.

        Arguments:
            obj_list {[list of object]} -- list of Python objects/dictionary
        """

        dt = datetime.now()
        timestamp = dt.strftime("%d/%B/%Y:%H:%M:%S")
        try:
            with open(file_path, 'a+') as stats_file:
                csv_writer = csv.writer(stats_file, quotechar='"')

                # Prepare the data
                headers = ["timestamp"]
                headers += obj.keys()

                # Upon reconnections the client could send a redundant objs just discard them
                if len(headers) < 15:
                    return

                values = [timestamp]
                for val in obj.values():
                    values.extend(['"{}"'.format(val) if isinstance(val, str) and ';' in val else val])

                if 'audio' in file_path:
                    # Audio stats
                    if self.prev_stats_audio_header_len is None:
                        csv_writer.writerow(headers)
                        csv_writer.writerow(values)
                        self.prev_stats_audio_header_len = len(headers)
                    elif self.prev_stats_audio_header_len == len(headers):
                        csv_writer.writerow(values)
                    else:
                        # Update the data after obtaining new fields
                        self.prev_stats_audio_header_len = self.update_webrtc_stats_csv(file_path, headers, values)
                else:
                    # Video stats
                    if self.prev_stats_video_header_len is None:
                        csv_writer.writerow(headers)
                        csv_writer.writerow(values)
                        self.prev_stats_video_header_len = len(headers)
                    elif self.prev_stats_video_header_len == len(headers):
                        csv_writer.writerow(values)
                    else:
                        # Update the data after obtaining new fields
                        self.prev_stats_video_header_len = self.update_webrtc_stats_csv(file_path, headers, values)

        except Exception as e:
            logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))

    def update_webrtc_stats_csv(self, file_path: str, headers: List[str], values: List[Any]):
        """Copies data from one CSV file to another to facilite dynamic updates to the data structure
           by handling empty values and appending new data.
        """
        prev_headers = None
        prev_values = []

        try:
            with open(file_path, 'r') as stats_file:
                csv_reader = csv.reader(stats_file, delimiter=',')

                # Fetch all existing data
                header_indicator = 0
                for row in csv_reader:
                    if header_indicator == 0:
                        prev_headers = row
                        header_indicator += 1
                    else:
                        prev_values.append(row)

                # Sometimes columns might not exist in new data
                if len(headers) < len(prev_headers):
                    for i in prev_headers:
                        if i not in headers:
                            values.insert(prev_headers.index(i), "NaN")
                else:
                    i, j, k = 0, 0, 0
                    while i < len(headers):
                        if headers[i] != prev_headers[j]:
                            # If there is a mismatch, update all previous rows with a placeholder to represent an empty value, using `NaN` here
                            for row in prev_values:
                                row.insert(i, "NaN")
                            i += 1
                            k += 1  # track number of values added
                        else:
                            i += 1
                            j += 1

                    j += k
                    # When new fields are at the end
                    while j < i:
                        for row in prev_values:
                            row.insert(j, "NaN")
                        j += 1

                # Validation check to confirm modified rows are of same length
                if len(prev_values[0]) != len(values):
                    logger_metrics.warning("There's a mismatch; columns could be misaligned with headers")

            # Purge existing file
            if os.path.exists(file_path):
                os.remove(file_path)
            else:
                logger_metrics.warning("File {} doesn't exist to purge".format(file_path))

            # Create a new file with updated data
            with open(file_path, "a") as stats_file:
                csv_writer = csv.writer(stats_file)

                if len(headers) > len(prev_headers):
                    csv_writer.writerow(headers)
                else:
                    csv_writer.writerow(prev_headers)
                csv_writer.writerows(prev_values)
                csv_writer.writerow(values)

                logger_metrics.debug("WebRTC Statistics file {} created with updated data".format(file_path))
            return len(headers) if len(headers) > len(prev_headers) else len(prev_headers)
        except Exception as e:
            logger_metrics.error("writing WebRTC Statistics to CSV file: " + str(e))

    def initialize_webrtc_csv_file(self, webrtc_stats_dir: str ='/tmp'):
        """Initializes the WebRTC Statistics file upon every new WebRTC connection
        """
        dt = datetime.now()
        timestamp = dt.strftime("%Y-%m-%d:%H:%M:%S")
        self.stats_video_file_path = '{}/selkies-stats-video-{}.csv'.format(webrtc_stats_dir, timestamp)
        self.stats_audio_file_path = '{}/selkies-stats-audio-{}.csv'.format(webrtc_stats_dir, timestamp)
        self.prev_stats_video_header_len = None
        self.prev_stats_audio_header_len = None


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

                _, pending = await asyncio.wait([self.stop_event.wait()], timeout=self.period)
                for task in pending:
                    task.cancel()
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_system.error(f"System monitor error: {e}")
        finally:
            logger_system.info("System monitor stopped")

    async def stop(self):
        self.stop_event.set()
        if self.task:
            await self.task

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
                _, pending = await asyncio.wait([self.stop_event.wait()], timeout=self.period)
                for task in pending:
                    task.cancel()
        except asyncio.CancelledError:
            pass
        except Exception as e:
            logger_gpu.error(f"GPU monitor error: {e}")
        finally:
            logger_gpu.info("GPU monitor stopped")

    async def stop(self):
        self.stop_event.set()
        if self.task:
            await self.task
