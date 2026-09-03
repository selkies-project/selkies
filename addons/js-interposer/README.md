# Selkies Joystick (Gamepad) Interposer

An `LD_PRELOAD` library for interposing application calls to open a Linux joystick/gamepad device and pass data through a unix domain socket.

This allows the Selkies remote desktop application interface to pass gamepad events over the WebRTC `RTCDataChannel` or WebSockets, and translate them to joystick/gamepad events to emulate devices without requiring access to /dev/input/js0 or depending on kernel modules including `uinput`.

## Compiling

```bash
gcc -shared -fPIC -ldl -o selkies_joystick_interposer.so joystick_interposer.c
```

To compile the `i386` library for Wine and other 32-bit packages, add `-m32` with the `gcc-multilib` package installed.

## Installing

1. Install to your library path (may be `/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so` and `/usr/lib/i386-linux-gnu/selkies_joystick_interposer.so` for Ubuntu), also available as a tarball or `.deb` installer.

If using Wine with `x86_64`, both `/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so` and `/usr/lib/i386-linux-gnu/selkies_joystick_interposer.so` are likely required.

2. The `/dev/input` directory has to exist for the interposer to augment it:

```bash
sudo mkdir -pm1777 /dev/input
```

Each of the four gamepad slots is interposed as both a joydev node (`js0`-`js3`) and an evdev node (`event1000`-`event1003`). An application opening one of those paths by name is intercepted whether or not the file exists, so no placeholder files are needed for that. An application that instead scans `/dev/input` is served too: the interposer adds the evdev node of every bound slot to `opendir`/`readdir` and `scandir`, so a directory scan lists them the way it lists a real device (character device, major 13), and an unbound slot is left out so a scan never trips over a node with no server behind it. A scanner that then watches `/dev/input` with inotify is told of a slot bound or withdrawn later as that node appearing or vanishing, the way it would learn of a plugged device.

3. Use the below command before running your target application, so the interposer library intercepts its joystick/gamepad calls (the single quotes are required in the first line).

```bash
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
```

Do **not** preload this into the Selkies backend process itself. It hooks
`read`, `close`, `ioctl` and `epoll_ctl` for every file descriptor in the
process, and its blocking device reads are exactly what an asyncio event loop
must never do: a hook that blocks there stops the server answering anything.
The backend is the other end of these sockets and needs no preload;
`SELKIES_INTERPOSER` alone tells it that applications have one.

Otherwise, if you only need one architecture, the below is an equivalent command.

```bash
export LD_PRELOAD="/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so${LD_PRELOAD:+:${LD_PRELOAD}}"
```

You can replace `/usr/$LIB/selkies_joystick_interposer.so` with any non-root path of your choice if using the `.tar.gz` tarball. Make sure the correct `selkies_joystick_interposer.so` is installed in that path.

SDL2 applications find the four pads through [fake-udev](https://github.com/selkies-project/selkies/tree/main/addons/fake-udev/README.md), which is preloaded alongside the interposer in the container images. Where device discovery through `libudev` is unavailable — `SDL_JOYSTICK_DISABLE_UDEV=1`, an SDL sandbox build, or an SDL built without udev — SDL scans `/dev/input`, and the interposer's evdev nodes appear in that scan, so the pads are found with nothing further to set.

SDL with udev disabled also probes `/dev/input/js0` directly, and since the interposer answers that path it enumerates the pad as both its joydev and evdev node — the same double a udev-less SDL shows for a real controller, and the reason the container images run fake-udev. To pin SDL to the evdev node alone (and skip the joydev probe), name it:

```bash
export SDL_JOYSTICK_DEVICE=/dev/input/event1000:/dev/input/event1001:/dev/input/event1002:/dev/input/event1003
```

## Testing

1. Start the gamepad server on a private socket directory. It serves slot 0 on both of that slot's sockets as the standard Xbox 360 pad, then toggles button 0 and moves axis 1 for 30 seconds:

```bash
mkdir -p /tmp/selkies-js-test
SELKIES_JS_SOCKET_PATH=/tmp/selkies-js-test python3 tests/tools/gamepad/gpserver.py
```

2. Build the readers (`make -C tests/tools gamepad`, needs SDL2 and libudev) and, in another shell, read the joydev node through the interposer. The interposer resolves its sockets under `SELKIES_JS_SOCKET_PATH` too, so the client needs the same value:

```bash
export SELKIES_JS_SOCKET_PATH=/tmp/selkies-js-test
LD_PRELOAD='/usr/$LIB/selkies_joystick_interposer.so' tests/tools/gamepad/jsread /dev/input/js0
```

`jsread` prints the device name from `JSIOCGNAME` and the first few `js_event` records; any joydev client (`jstest /dev/input/js0`, for instance) works the same way.

3. Read the evdev node of the same slot through SDL2:

```bash
export SELKIES_JS_SOCKET_PATH=/tmp/selkies-js-test
export SDL_JOYSTICK_DEVICE=/dev/input/event1000
LD_PRELOAD='/usr/$LIB/selkies_joystick_interposer.so' timeout 10 tests/tools/gamepad/sdlread
```

`sdlread` prints the name, GUID, vendor/product and axis/button/hat counts SDL read out of the interposer, then one line per event. `tests/tools/gamepad/sdlenum` lists what SDL enumerates without opening anything.

## Unix domain socket protocol

Selkies is the server (`SelkiesGamepad` in `selkies.input_handler`) and this library is the client. The sockets are `AF_UNIX`/`SOCK_STREAM` and live in `$SELKIES_JS_SOCKET_PATH` (default `/tmp`, set from `--js_socket_path`): `selkies_js<0-3>.sock` backs `/dev/input/js<0-3>` and `selkies_event100<0-3>.sock` backs `/dev/input/event100<0-3>`.

Every `open()` of an interposed device makes its own connection (up to 16 per device), so each handle gets the full event stream. A connect is retried for 250 ms; if nothing is listening, `open()` fails with `EIO`.

### Handshake

1. **Server → client: `js_config_t`, 1360 bytes**, written as soon as the connection is accepted. The client reads exactly that many bytes; the layout is below.

2. **Client → server: 1 byte**, the client's `sizeof(long)`: `8` from a 64-bit process, `4` from a 32-bit one (Wine, 32-bit Steam titles). It sets the `timeval` width of the evdev records below; joydev records do not depend on it.

3. **Server → client on the joydev socket: the init burst**, sent the moment the architecture byte arrives, before any live event. It is one `js_event` per button and per axis of the mapping — 19 events for the standard pad — carrying the current state with `JS_EVENT_INIT` (`0x80`) OR'd into the type, which is what joydev itself sends on open. The evdev socket has no in-band equivalent: those clients read the initial state from the interposer's `EVIOCG*` ioctl emulation.

### The `js_config_t` payload

| offset | field | C type | bytes |
|---|---|---|---|
| 0 | `name` | `char[255]` | 255 |
| 255 | (alignment pad) | — | 1 |
| 256 | `vendor` | `uint16_t` | 2 |
| 258 | `product` | `uint16_t` | 2 |
| 260 | `version` | `uint16_t` | 2 |
| 262 | `num_btns` | `uint16_t` | 2 |
| 264 | `num_axes` | `uint16_t` | 2 |
| 266 | `btn_map` | `uint16_t[512]` | 1024 |
| 1290 | `axes_map` | `uint8_t[64]` | 64 |
| 1354 | `final_alignment_padding` | `uint8_t[6]` | 6 |

`name` is a NUL-terminated UTF-8 string, truncated to fit. `btn_map` and `axes_map` are the evdev `BTN_*`/`ABS_*` codes of the mapping, zero-padded to their array length; `num_btns` and `num_axes` are the counts actually in use and never exceed those lengths. The last 6 bytes pad the 1354 bytes of fields up to the 1360 the C struct occupies, which is why the server packs the payload as `=255sxHHHHH512H64B` plus `6x` (`_make_interposer_config_payload` in `selkies.input_handler`) rather than as the fields alone.

### Event records

The joydev socket carries `struct js_event`, 8 bytes on every architecture, packed `=IhBB`:

```c
struct js_event { __u32 time; __s16 value; __u8 type; __u8 number; };
```

`time` is a millisecond timestamp, `type` is `JS_EVENT_BUTTON` (`0x01`) or `JS_EVENT_AXIS` (`0x02`), OR'd with `JS_EVENT_INIT` for the burst above.

The evdev socket carries `struct input_event`, **24 bytes for a 64-bit client and 16 for a 32-bit one** (`=qqHHi` / `=llHHi`), the difference being the two `long`-width `timeval` members:

```c
struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; };
```

Each event is written together with a `SYN_REPORT` (`type` `EV_SYN` = 0, `code` `SYN_REPORT` = 0, `value` 0) immediately after it, so one button press or axis motion is always two records.

### Closing

Closing a handle is per handle: the interposer retires the fd that was closed, leaves this device's other handles alone, and clears the cached `js_config_t` only when the last one goes. The server drops the writer from its fan-out list when the connection ends, and unlinks the socket files when the gamepad is shut down.
