#!/usr/bin/env python3
"""HTTPS comes up without anyone having made a certificate first.

Browsers gate the clipboard, gamepads, pointer lock and the camera on a secure
context, so a session that is not on localhost needs TLS before those work at
all. Requiring a certificate to exist first made turning it on a two-step job,
and the error told the operator to run openssl. Enabling HTTPS now mints a
self-signed pair when none is configured, at the configured path when that is
writable and in the user's state directory when it is not, and reuses whatever
is already there so a browser's trust exception survives a restart.

The pair it mints has to be usable at the address the session is reached by: a
certificate naming only `localhost` fails hostname verification at 127.0.0.1,
which a published container port is what most deployments type. Reuse is what
makes the exception survive, so it is checked at the fallback path too — the
one the configured-path lookup that triggers generation never names — and a
pair from before the addresses were issued is replaced rather than carried
forward.
"""
import ipaddress
import os
import ssl
import stat
import sys
import tempfile
import types

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

import socket  # noqa: E402
import threading  # noqa: E402
from datetime import datetime, timedelta, timezone  # noqa: E402

import helpers as H  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from selkies.stream_server import CentralizedStreamServer  # noqa: E402


def _write_localhost_only_pair(cert_path: str, key_path: str) -> None:
    """Write the pair an install from before the address SANs would be carrying."""
    from cryptography import x509
    from cryptography.hazmat.primitives.asymmetric import ec

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(x509.NameOID.COMMON_NAME, "localhost")])
    now = datetime.now(tz=timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name).issuer_name(name)
        .public_key(key.public_key()).serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(days=1)).not_valid_after(now + timedelta(days=3650))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .sign(key, hashes.SHA256())
    )
    with open(cert_path, "wb") as handle:
        handle.write(cert.public_bytes(serialization.Encoding.PEM))
    with open(key_path, "wb") as handle:
        handle.write(key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption()))


def server(cert: str, key: str):
    """A stand-in carrying only the settings the TLS path reads."""
    settings = types.SimpleNamespace(enable_https=(True, False), https_cert=cert, https_key=key)
    obj = types.SimpleNamespace(settings=settings)
    for name in ("_get_https_certs", "_make_self_signed_cert", "_create_ssl_context",
                 "_get_cert_mtime", "_self_signed_candidates", "_self_signed_names",
                 "_self_signed_pair_usable"):
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

    from cryptography import x509
    parsed = x509.load_pem_x509_certificate(body)
    sans = parsed.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
    res.check("the certificate names localhost",
              "localhost" in sans.get_values_for_type(x509.DNSName),
              sans.get_values_for_type(x509.DNSName))
    addresses = sans.get_values_for_type(x509.IPAddress)
    res.check("the certificate names the loopback addresses it is reached by",
              ipaddress.ip_address("127.0.0.1") in addresses
              and ipaddress.ip_address("::1") in addresses,
              [str(a) for a in addresses])

    # The finding this covers: a certificate that names only `localhost` is
    # rejected for a name mismatch at 127.0.0.1, not merely distrusted, so
    # verification against the certificate itself is what proves the fix.
    verifier = ssl.create_default_context(cafile=cert)
    server_socket = socket.socket()
    server_socket.bind(("127.0.0.1", 0))
    server_socket.listen(1)

    def serve_verified():
        connection, _ = server_socket.accept()
        try:
            with context.wrap_socket(connection, server_side=True) as tls:
                tls.recv(16)
        except OSError:
            pass

    threading.Thread(target=serve_verified, daemon=True).start()
    verified = ""
    try:
        with socket.create_connection(("127.0.0.1", server_socket.getsockname()[1]),
                                      timeout=10) as raw:
            with verifier.wrap_socket(raw, server_hostname="127.0.0.1") as tls:
                tls.send(b"hi")
                verified = "ok"
    except ssl.SSLCertVerificationError as error:
        verified = str(error)
    server_socket.close()
    res.check("it verifies for 127.0.0.1, not only for the name localhost",
              verified == "ok", verified)

    # Regeneration per start invalidates the exception the browser was told to
    # make, and the state pair is not at a path _get_https_certs consults.
    fallback_key = fallback_cert[: -len(".pem")] + ".key"
    fallback_body = open(fallback_cert, "rb").read()
    again_cert, again_key = server(os.path.join(blocked, "s.pem"),
                                  os.path.join(blocked, "s.key"))._make_self_signed_cert()
    res.check("a second start reuses the fallback pair rather than minting one",
              (again_cert, again_key) == (fallback_cert, fallback_key)
              and open(fallback_cert, "rb").read() == fallback_body)
    res.check("the reload watcher sees the pair actually being served",
              server(os.path.join(blocked, "s.pem"),
                     os.path.join(blocked, "s.key"))._get_cert_mtime() > 0)

    # An install carrying the localhost-only pair from before must not keep it.
    stale = os.path.join(work, "stale")
    os.makedirs(stale)
    stale_cert, stale_key = os.path.join(stale, "s.pem"), os.path.join(stale, "s.key")
    _write_localhost_only_pair(stale_cert, stale_key)
    res.check("a localhost-only pair from an earlier install is replaced",
              not server(stale_cert, stale_key)._self_signed_pair_usable(stale_cert, stale_key))
    os.environ["XDG_STATE_HOME"] = os.path.join(work, "state-upgrade")
    server(stale_cert, stale_key)._make_self_signed_cert()
    replaced = x509.load_pem_x509_certificate(open(stale_cert, "rb").read())
    res.check("and what replaces it names the loopback addresses",
              ipaddress.ip_address("127.0.0.1") in replaced.extensions
              .get_extension_for_class(x509.SubjectAlternativeName).value
              .get_values_for_type(x509.IPAddress))

    settings = types.SimpleNamespace(enable_https=(False, False), https_cert=cert, https_key=key)
    off = types.SimpleNamespace(settings=settings)
    off._create_ssl_context = types.MethodType(CentralizedStreamServer._create_ssl_context, off)
    res.check("disabled HTTPS builds no context and writes nothing",
              off._create_ssl_context() is None)

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
