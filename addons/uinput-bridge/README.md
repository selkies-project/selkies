# Optional Selkies uinput gamepad bridge

This addon documents an **optional** path that mirrors Selkies browser gamepad
events to a real kernel Xbox-like controller via `/dev/uinput`.

The default Selkies path remains the [Joystick Interposer](../js-interposer/)
(`LD_PRELOAD`). Use this bridge only when applications need a real
`/dev/input/event*` / `js*` node — notably **Steam / Proton** and browsers
inside the remote desktop (for example [html5gamepad.com](https://html5gamepad.com/)).

## Why optional?

Selkies intentionally prefers `LD_PRELOAD` so gamepads work in unprivileged
containers without `/dev/uinput` (see historical discussion in
[#95](https://github.com/selkies-project/selkies/pull/95) and
[#168](https://github.com/selkies-project/selkies/issues/168)).
uinput needs device access and is therefore off by default.

## Enable (integrated)

1. Install `python-evdev` in the Selkies Python environment:
   ```bash
   pip install evdev
   ```
2. Ensure `/dev/uinput` exists and is writable by the Selkies process user
   (often: add the user to the `input` group, or `chmod 666 /dev/uinput` in
   trusted single-user VMs).
3. Enable the bridge:
   ```bash
   export SELKIES_ENABLE_UINPUT_BRIDGE=true
   ```
   Or pass `--enable_uinput_bridge=true` if using CLI settings.
4. Restart Selkies. On start, each virtual pad slot may create a kernel device
   named like `Microsoft X-Box 360 pad` / `Selkies Controller`.
5. Connect the Selkies web client over **HTTPS** (or `localhost`), press a
   button on the local pad, then **fully quit and restart Steam** so it
   enumerates the hot-plugged uinput device.

Verify:

```bash
grep -A5 -i 'x-box\|selkies\|microsoft' /proc/bus/input/devices
```

## Standalone helper

[`selkies_uinput_bridge.py`](selkies_uinput_bridge.py) can also be run as a
separate process that attaches to existing `/tmp/selkies_js{0-3}.sock` files
(legacy / external deployments). Prefer the integrated setting above on
current Selkies `main`.

```bash
pip install evdev
python3 addons/uinput-bridge/selkies_uinput_bridge.py
```

Environment:

- `SELKIES_JS_SOCKET_PATH` — socket directory (default `/tmp`)

## Notes

- Does **not** replace `LD_PRELOAD` / `SELKIES_INTERPOSER`.
- Browser Gamepad API requires a [secure context](https://developer.mozilla.org/en-US/docs/Web/API/Gamepad_API)
  (HTTPS or localhost).
- Placeholder regular files at `/dev/input/js*` (created for the interposer)
  can block real kernel `js` nodes; remove non-character placeholders if you
  rely on uinput `js*` devices.
