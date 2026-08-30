#!/usr/bin/env python3
"""The address the embedded TURN server hands out.

A TURN server is only useful at the address a browser outside the container can
reach it on, and nothing inside the container can see that address -- it has to
be asked of a public resolver. Every way that can go wrong ends in a session
that negotiates and then carries no media: a resolver diagnostic mistaken for an
answer, an IPv6 literal left unbracketed for a URL, or no answer at all taken as
an empty address rather than the container's own.

Runs the entrypoint's own helpers against stubbed lookups, so what is checked is
the shipped code rather than a restatement of it.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "base", "container-entrypoint.sh")
# The end marker stops before the assignment, so the value's quoting is
# not part of what this suite pins.
BEGIN, END = "public_address() {", "export SELKIES_ENABLE_INTERNAL_TURN"

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [turn-address] {label}  {detail}", flush=True)


def helpers():
    """The address helpers as the entrypoint defines them.

    Returns:
        A (source, missing) pair: the slice between the markers and an empty
        string, or None and the marker the entrypoint is missing.
    """
    body = open(ENTRYPOINT).read()
    for marker in (BEGIN, END):
        if marker not in body:
            return None, marker
    return body[body.index(BEGIN):body.index(END)], ""


def ask(call: str, dig: str = "exit 1", hostname: str = "exit 1",
        getent: str = "exit 1") -> str:
    """Run one helper with the lookups it depends on arranged.

    Args:
        call: The shell invocation of the helper under test.
        dig: Stub body for `dig`; the default answers nothing.
        hostname: Stub body for `hostname`.
        getent: Stub body for `getent`.

    Returns:
        The helper's stdout, stripped.
    """
    with tempfile.TemporaryDirectory() as tmp:
        stub_dir = os.path.join(tmp, "bin")
        os.mkdir(stub_dir)
        for name, script in (("dig", dig), ("hostname", hostname), ("getent", getent)):
            path = os.path.join(stub_dir, name)
            with open(path, "w") as f:
                f.write("#!/bin/sh\n" + script + "\n")
            os.chmod(path, 0o755)
        script = os.path.join(tmp, "lib.sh")
        with open(script, "w") as f:
            f.write(LIB + "\n" + call + "\n")
        env = dict(os.environ, PATH=stub_dir + os.pathsep + os.environ["PATH"])
        return subprocess.run(["bash", script], capture_output=True, text=True,
                              env=env, timeout=60).stdout.strip()


if not shutil.which("bash"):
    print("SKIP bash not found, so the entrypoint helpers cannot be run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

LIB, missing = helpers()
if LIB is None:
    print(f"FAIL  [turn-address] entrypoint defines the address helpers  missing: {missing}")
    sys.exit(1)

V4 = 'case "$1" in -4) echo \'"203.0.113.7"\';; *) exit 1;; esac'
V6 = 'case "$1" in -6) echo \'"2001:db8::5"\';; *) exit 1;; esac'
LOCAL = 'echo "10.1.2.3 fd00::1"'

check("an IPv4 answer is used as given",
      ask("public_address", dig=V4, hostname=LOCAL) == "203.0.113.7")
# Bracketed because it goes into a URL, where a bare IPv6 literal's colons are
# indistinguishable from the port separator.
check("an IPv6 answer is bracketed",
      ask("public_address", dig=V6, hostname=LOCAL) == "[2001:db8::5]")
check("IPv4 is preferred when both answer",
      ask("public_address", dig='echo \'"203.0.113.7"\'', hostname=LOCAL) == "203.0.113.7")
# A resolver that cannot answer still prints, and its diagnostic is not an address.
check("a resolver diagnostic is not mistaken for an answer",
      ask("public_address", dig='echo ";; connection timed out"',
          hostname=LOCAL) == "10.1.2.3")
check("no resolver falls back to this container's address",
      ask("public_address", hostname=LOCAL) == "10.1.2.3")
# Never empty: an empty external address makes coTURN advertise nothing at all.
check("with nothing to go on, a routable address is still returned",
      ask("public_address") == "127.0.0.1")

check("a hostname is resolved for coTURN, which binds addresses",
      ask('resolved_address "turn.example.com"',
          getent='echo "198.51.100.9 STREAM turn.example.com"') == "198.51.100.9")
check("an IPv6-only host resolves bracketed",
      ask('resolved_address "turn.example.com"',
          getent='case "$1" in ahostsv6) echo "2001:db8::9 STREAM turn.example.com";;'
                 ' *) exit 1;; esac') == "[2001:db8::9]")
# public_address may have bracketed it already; getent takes the literal.
check("an already-bracketed literal is unwrapped before lookup",
      ask('resolved_address "[2001:db8::5]"',
          getent='case "$1" in ahostsv6) echo "$2 STREAM $2";; *) exit 1;; esac')
      == "[2001:db8::5]")


# Whether the container starts its own TURN server. Getting this wrong is silent
# either way: a needless coTURN, or a WebRTC session with no relay when the one
# that was configured cannot actually be used.
TURN_VARS = ("SELKIES_TURN_REST_URI", "SELKIES_TURN_USERNAME", "SELKIES_TURN_PASSWORD",
             "SELKIES_TURN_SHARED_SECRET", "SELKIES_TURN_HOST", "SELKIES_TURN_PORT")


def internal_turn(**env: str) -> bool:
    """Whether the entrypoint would start its own TURN server under `env`.

    Every variable is set explicitly, so a stray one in the environment running
    the tests cannot decide the answer.
    """
    setup = "".join(f'export {k}="{env.get(k, "")}"\n' for k in TURN_VARS)
    with tempfile.TemporaryDirectory() as tmp:
        script = os.path.join(tmp, "lib.sh")
        with open(script, "w") as f:
            f.write(setup + LIB +
                    "\nif ! external_turn_configured; then echo internal; else echo external; fi\n")
        return subprocess.run(["bash", script], capture_output=True, text=True,
                              timeout=60).stdout.strip() == "internal"


check("nothing configured runs the internal server", internal_turn())
check("a REST service is enough on its own",
      not internal_turn(SELKIES_TURN_REST_URI="https://turn.example.com/creds"))
check("host, port and a password are enough",
      not internal_turn(SELKIES_TURN_HOST="turn.example.com", SELKIES_TURN_PORT="3478",
                        SELKIES_TURN_USERNAME="u", SELKIES_TURN_PASSWORD="p"))
check("host, port and a shared secret are enough",
      not internal_turn(SELKIES_TURN_HOST="turn.example.com", SELKIES_TURN_PORT="3478",
                        SELKIES_TURN_SHARED_SECRET="s"))
check("credentials without a host are not enough",
      internal_turn(SELKIES_TURN_USERNAME="u", SELKIES_TURN_PASSWORD="p"))
check("a host without a port is not enough",
      internal_turn(SELKIES_TURN_HOST="turn.example.com", SELKIES_TURN_SHARED_SECRET="s"))
check("a username without its password is not enough",
      internal_turn(SELKIES_TURN_HOST="turn.example.com", SELKIES_TURN_PORT="3478",
                    SELKIES_TURN_USERNAME="u"))

print(f"[turn-address] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
