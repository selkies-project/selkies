# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""TURN REST credential service.

A minimal Flask app implementing the TURN REST API: it mints time-limited
HMAC-SHA1 credentials (coturn's ``use-auth-secret`` scheme) from a shared
secret, optionally gated behind an API key, and returns the TURN URI list a
WebRTC client feeds into its RTCPeerConnection configuration. All deployment
knobs come from `TURN_*` environment variables read at import time.
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

if __name__ == "__main__":
    app.run(host="0.0.0.0", port="8008")
