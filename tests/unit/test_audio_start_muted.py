#!/usr/bin/env python3
"""The audio_start_muted setting exists and both cores carry its policy.

Start-muted is a client-boot decision, so three places must agree: the
setting definition (bool, false default, present in the client payload),
the websockets core, and the WebRTC core. This suite is a STRUCTURAL
guard, not a behavioural one: it parses the setting definition and then
asserts, by pattern, that each core carries the guarded policy block
(server value, user-toggle latch, shared-mode and display gates) and
that the websockets core gates its initial audio request on the settings
payload instead of sending it unconditionally. It does not execute
either core; runtime behaviour lives with the e2e dashboard suites.
"""
import ast
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SETTINGS_PY = os.path.join(ROOT, "src", "selkies", "settings.py")
WS_CORE = os.path.join(ROOT, "addons", "selkies-web-core", "selkies-ws-core.js")
WR_CORE = os.path.join(ROOT, "addons", "selkies-web-core", "selkies-wr-core.js")

failed = 0


def check(name, ok, detail=""):
    global failed
    if not ok:
        failed += 1
    print(("PASS" if ok else "FAIL") + "  [audio-start-muted] {}  {}".format(name, detail),
          flush=True)


def setting_definition():
    """The audio_start_muted dict literal from SETTING_DEFINITIONS, or None."""
    with open(SETTINGS_PY, encoding="utf-8") as fh:
        tree = ast.parse(fh.read())
    for node in ast.walk(tree):
        if not isinstance(node, ast.Dict):
            continue
        keys = [k.value for k in node.keys if isinstance(k, ast.Constant)]
        if "name" not in keys:
            continue
        try:
            spec = ast.literal_eval(node)
        except ValueError:
            continue
        if isinstance(spec, dict) and spec.get("name") == "audio_start_muted":
            return spec
    return None


def main():
    spec = setting_definition()
    check("setting is defined", spec is not None)
    if spec is None:
        return False
    check("type is bool", spec.get("type") == "bool", repr(spec.get("type")))
    check("default is false", str(spec.get("default")).lower() == "false",
          repr(spec.get("default")))

    with open(SETTINGS_PY, encoding="utf-8") as fh:
        settings_text = fh.read()
    excluded = re.search(r"CLIENT_PAYLOAD_EXCLUDED\s*=\s*[{\[](.*?)[}\]]",
                         settings_text, re.S)
    check("CLIENT_PAYLOAD_EXCLUDED located", excluded is not None)
    check("not excluded from the client payload",
          excluded is not None and "audio_start_muted" not in excluded.group(1))
    check("not marked sensitive", not spec.get("sensitive", False))

    # The full guard, in source order: server value true, no user toggle yet,
    # not a shared viewer, primary display. A missing gate reintroduces the
    # shared-viewer STOP_AUDIO bug this block was reviewed for.
    guard = (r"asm\.value === true\s*&&(?:\s*!audioDisabledByServer\s*&&)?"
             r"\s*!audioToggledByUser\s*&&\s*!isSharedMode\s*&&"
             r"\s*\n?\s*\t*displayId === ['\"]primary['\"]")
    for label, path in (("ws-core", WS_CORE), ("wr-core", WR_CORE)):
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        check("{} reads the server value from the payload".format(label),
              re.search(r"obj\.settings\.audio_start_muted", text) is not None)
        check("{} guards on user toggle, shared mode and display".format(label),
              re.search(guard, text) is not None)
        check("{} latches the user toggle in the audio control branch".format(label),
              re.search(r"pipeline === ['\"]audio['\"][\s\S]{0,800}?audioToggledByUser = true",
                        text) is not None)

    with open(WS_CORE, encoding="utf-8") as fh:
        ws_text = fh.read()
    check("ws-core holds the initial audio request for the settings payload",
          re.search(r"if \(serverSettingsReceived\) websocket\.send\('START_AUDIO'\);"
                    r"\s*\n\s*else pendingInitialAudioStart = true;", ws_text) is not None)
    check("ws-core resolves the pending request inside the settings handler",
          re.search(r"if \(pendingInitialAudioStart\) \{[\s\S]{0,300}?START_AUDIO", ws_text)
          is not None)
    check("ws-core sends no unconditional initial START_AUDIO",
          re.search(r"if \(isAudioPipelineActive\) websocket\.send\('START_AUDIO'\);", ws_text)
          is None)

    print("ALL PASS" if failed == 0 else "{} FAILED".format(failed), flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
