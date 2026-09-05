# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""TURN REST credential service.

A minimal Flask app implementing the TURN REST API: it mints time-limited
HMAC-SHA1 credentials (coturn's ``use-auth-secret`` scheme) from a shared
secret, optionally gated behind an API key, and returns the TURN URI list a
WebRTC client feeds into its RTCPeerConnection configuration. All deployment
knobs come from `TURN_*` environment variables read at import time. Run
directly, the development server listens on `TURN_REST_ADDR` (host names or
addresses, comma-separated, the loopback addresses by default) at
`TURN_REST_PORT` (8008); the container's gunicorn command is what a deployment
runs.
"""

from flask import Flask, request, jsonify
import os
import time
import hmac
import hashlib
import base64
import secrets
import string

from typing import Any, Optional

shared_secret = os.environ.get('TURN_SHARED_SECRET', 'openrelayprojectsecret')
turn_api_key = os.environ.get('TURN_API_KEY', '')
turn_host = os.environ.get('TURN_HOST', 'staticauth.openrelay.metered.ca')
turn_port = os.environ.get('TURN_PORT', '443')
turn_protocol_default = os.environ.get('TURN_PROTOCOL', 'udp')
turn_tls_default = os.environ.get('TURN_TLS', 'false')
turn_ttl_default = os.environ.get('TURN_TTL', '86400')

app = Flask(__name__)


def parse_port(value, fallback: int) -> int:
    """Parse a TCP/UDP port number, falling back on anything out of range."""
    try:
        port = int(value)
        if 1 <= port <= 65535:
            return port
    except (TypeError, ValueError):
        pass
    return fallback


def parse_ttl(value, fallback: int) -> int:
    """Parse a positive credential TTL in seconds, falling back otherwise."""
    try:
        ttl = int(value)
        if ttl > 0:
            return ttl
    except (TypeError, ValueError):
        pass
    return fallback


def parse_bool(value, fallback: bool = False) -> bool:
    """Parse a permissive boolean string; unrecognized values take the fallback."""
    if value is None:
        return fallback
    value = str(value).strip().lower()
    if value in ('1', 'true', 'yes', 'on'):
        return True
    if value in ('0', 'false', 'no', 'off'):
        return False
    return fallback


def parse_protocol(value, fallback: str = 'udp') -> str:
    """Normalize a transport protocol string to ``tcp`` or ``udp``."""
    candidate = (value or fallback or 'udp').strip().lower()
    return 'tcp' if candidate == 'tcp' else 'udp'


def format_ice_host(host: str) -> str:
    """Bracket a bare IPv6 literal so it is valid inside a TURN URI."""
    if host and ":" in host and not (host.startswith("[") and host.endswith("]")):
        return f"[{host}]"
    return host


def random_username(length: int = 16) -> str:
    """A random lowercase-alphanumeric username for anonymous requests."""
    alphabet = string.ascii_lowercase + string.digits
    return ''.join(secrets.choice(alphabet) for _ in range(length))


def get_param(name: str, json_payload: Any) -> Optional[Any]:
    """Read a request parameter from query/form values, then the JSON body."""
    value = request.values.get(name)
    if value is not None:
        return value
    if isinstance(json_payload, dict):
        return json_payload.get(name)
    return None


@app.route('/', methods=['GET', 'POST'])
def turn_rest():
    """Mint a time-limited TURN credential per the TURN REST API.

    Returns:
        JSON with ``username`` (`expiry:user`), the HMAC-SHA1 ``password``,
        ``ttl``, and the ``uris`` list, or a plain-text error with a 4xx
        status for a bad service or API key.
    """
    json_payload = request.get_json(silent=True)

    service_input = str(get_param('service', json_payload) or 'turn').strip().lower()
    if service_input not in ('', 'turn'):
        return "Invalid service sent. Only 'turn' is supported.\n", 400

    if turn_api_key:
        api_key_input = get_param('key', json_payload) or get_param('api', json_payload)
        if not api_key_input:
            return "Invalid service and/or key sent.\n", 400
        if not hmac.compare_digest(api_key_input, turn_api_key):
            return "Not allowed to access this service.\n", 403

    username_input = get_param('username', json_payload) or request.headers.get('x-auth-user') or request.headers.get('x-turn-username')
    username_input = str(username_input).strip() if username_input is not None else ''
    if not username_input:
        username_input = random_username()

    protocol = parse_protocol(get_param('protocol', json_payload) or request.headers.get('x-turn-protocol'), turn_protocol_default)
    turn_tls = parse_bool(get_param('tls', json_payload) or request.headers.get('x-turn-tls'), parse_bool(turn_tls_default, False))
    ttl = parse_ttl(turn_ttl_default, 86400)
    host = str(turn_host).strip()
    if not host:
        host = 'staticauth.openrelay.metered.ca'
    port = parse_port(turn_port, 3478)

    # The colon separates expiry from user in the credential, so a user-supplied
    # colon would corrupt the format.
    user = username_input.replace(":", "-")

    exp = int(time.time()) + ttl
    username = "{}:{}".format(exp, user)

    # coturn's use-auth-secret scheme: password = b64(HMAC-SHA1(secret, username)).
    hashed = hmac.new(bytes(shared_secret, "utf-8"), bytes(username, "utf-8"), hashlib.sha1).digest()
    password = base64.b64encode(hashed).decode()

    turn_uri = "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', format_ice_host(host), port, protocol)

    rtc_config = {}
    rtc_config["username"] = username
    rtc_config["password"] = password
    rtc_config["ttl"] = ttl
    rtc_config["uris"] = [turn_uri]

    return jsonify(rtc_config)

def _development_servers(addr: str, port: int) -> list:
    """One werkzeug server per address `addr` names.

    `addr` is a host name or address, or a comma-separated list of them, each
    bound on every address it resolves to. The sockets are bound here rather
    than by werkzeug, which serves one address per server and leaves an IPv6
    socket dual-stack, so `0.0.0.0,::` would collide on the port. An address
    the host lacks is skipped while another binds; anything else, or nothing
    binding, raises.

    Raises:
        OSError: When no address of `addr` can be bound.
    """
    import errno
    import socket
    from werkzeug.serving import make_server

    candidates = []
    for host in (h.strip() for h in addr.split(',')):
        if not host:
            continue
        for family, _, _, _, sockaddr in socket.getaddrinfo(
                host, port, type=socket.SOCK_STREAM, flags=socket.AI_PASSIVE):
            if (family, sockaddr) not in candidates:
                candidates.append((family, sockaddr))
    servers = []
    skipped = []
    for family, sockaddr in candidates:
        sock = None
        try:
            sock = socket.socket(family, socket.SOCK_STREAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            if family == socket.AF_INET6:
                sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            sock.bind(sockaddr)
            sock.listen(128)
        except OSError as exc:
            if sock is not None:
                sock.close()
            if exc.errno not in (errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT):
                raise
            skipped.append(f"{sockaddr[0]} ({exc.strerror})")
            continue
        # werkzeug duplicates the descriptor, so this socket object can go.
        with sock:
            servers.append(make_server(sockaddr[0], sockaddr[1], app, threaded=True, fd=sock.fileno()))
    if not servers:
        raise OSError(f"no address of '{addr}' is available to listen on: {', '.join(skipped)}")
    for note in skipped:
        print(f"Not listening on {note}", flush=True)
    return servers


if __name__ == "__main__":
    import threading

    listen_port = parse_port(os.environ.get('TURN_REST_PORT'), 8008)
    for server in _development_servers(os.environ.get('TURN_REST_ADDR') or 'localhost', listen_port):
        host = server.host if ':' not in server.host else f"[{server.host}]"
        print(f"TURN REST server listening on http://{host}:{server.port}", flush=True)
        threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Event().wait()
