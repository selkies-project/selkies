#!/usr/bin/env python3
"""HTTPS comes up without anyone having made a certificate first.

Browsers gate the clipboard, gamepads, pointer lock and the camera on a secure
context, so a session that is not on localhost needs TLS before those work at
all. Requiring a certificate to exist first made turning it on a two-step job,
and the error told the operator to run openssl. Enabling HTTPS now mints a
self-signed pair when none is configured, at the configured path when that is
writable and in the user's state directory when it is not, and reuses whatever
is already there so a browser's trust exception survives a restart.
"""
import os
import ssl
import stat
import sys
import tempfile
import types

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

import helpers as H  # noqa: E402
from selkies.stream_server import CentralizedStreamServer  # noqa: E402


def server(cert: str, key: str):
    """A stand-in carrying only the settings the TLS path reads."""
    settings = types.SimpleNamespace(enable_https=(True, False), https_cert=cert, https_key=key)
    obj = types.SimpleNamespace(settings=settings)
    for name in ("_get_https_certs", "_make_self_signed_cert", "_create_ssl_context"):
        setattr(obj, name, types.MethodType(getattr(CentralizedStreamServer, name), obj))
    return obj


def main() -> int:
    res = H.Results("https-selfsigned")
    work = tempfile.mkdtemp(prefix="selkies-https-")
    os.environ["XDG_STATE_HOME"] = os.path.join(work, "state")

    configured = os.path.join(work, "configured")
    os.makedirs(configured)
    cert, key = os.path.join(configured, "s.pem"), os.path.join(configured, "s.key")
    made_cert, made_key = server(cert, key)._make_self_signed_cert()
    res.check("a writable configured path is where the pair lands",
              made_cert == cert and made_key == key, made_cert)
    res.check("the private key is readable by its owner alone",
              stat.S_IMODE(os.stat(key).st_mode) == 0o600,
              oct(stat.S_IMODE(os.stat(key).st_mode)))

    blocked = os.path.join(work, "blocked")
    os.makedirs(blocked)
    os.chmod(blocked, 0o500)
    fallback_cert, _ = server(os.path.join(blocked, "s.pem"),
                              os.path.join(blocked, "s.key"))._make_self_signed_cert()
    res.check("an unwritable one falls back to the state directory",
              fallback_cert.startswith(os.environ["XDG_STATE_HOME"]), fallback_cert)

    body = open(cert, "rb").read()
    context = server(cert, key)._create_ssl_context()
    res.check("a context is built from the pair already on disk",
              isinstance(context, ssl.SSLContext), type(context).__name__)
    res.check("that pair is reused rather than replaced",
              open(cert, "rb").read() == body)

    # A context that cannot complete a handshake is no use, so run one.
    import socket
    import threading
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)

    def serve():
        connection, _ = listener.accept()
        with context.wrap_socket(connection, server_side=True) as tls:
            tls.recv(16)
            tls.send(b"ok")

    threading.Thread(target=serve, daemon=True).start()
    client = ssl.create_default_context()
    client.check_hostname = False
    client.verify_mode = ssl.CERT_NONE
    with socket.create_connection(("127.0.0.1", listener.getsockname()[1]), timeout=10) as raw:
        with client.wrap_socket(raw) as tls:
            tls.send(b"hi")
            res.check("the certificate completes a TLS handshake", tls.recv(2) == b"ok")
    listener.close()

    settings = types.SimpleNamespace(enable_https=(False, False), https_cert=cert, https_key=key)
    off = types.SimpleNamespace(settings=settings)
    off._create_ssl_context = types.MethodType(CentralizedStreamServer._create_ssl_context, off)
    res.check("disabled HTTPS builds no context and writes nothing",
              off._create_ssl_context() is None)

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
