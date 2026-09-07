# Fake Libudev Core (for Virtual Gamepads)

This subproject provides a `libudev` shared library (`libudev.so.1`) designed to be used with `LD_PRELOAD`. It adds a predefined set of virtual gamepads to what the system's `libudev` reports, for applications that use `libudev` to discover and query input devices.

This is particularly useful for running applications in environments where actual gamepad hardware is unavailable or where a full udev daemon setup is not feasible (e.g., certain containerized environments, CI/CD pipelines).

## How it Works

When an application linked against `libudev` is launched with this library preloaded, calls to `libudev` functions are intercepted by this implementation. It:

1.  **Initializes Virtual Device Data:** On the first `udev_new()`, it sets up internal data structures representing a fixed number (`NUM_VIRTUAL_GAMEPADS`, currently 4) of virtual gamepads.
2.  **Loads the Real Library:** At the same time it `dlopen`s the system `libudev` by path (this library carries the `libudev.so.1` SONAME, so the dynamic linker never maps the real one on its own) and forwards everything that is not a virtual gamepad to it: other subsystems (`drm`, `video4linux`, `hidraw`, ...), the host's own input devices, hotplug events, hwdb and queue queries. `SELKIES_REAL_LIBUDEV` names the real library when it lives outside the platform library directories and `LD_LIBRARY_PATH`; `SELKIES_REAL_LIBUDEV=none` disables passthrough. Without a real `libudev`, only the virtual gamepads are visible.
3.  **Simulates Device Hierarchy:** Each virtual gamepad is represented with a typical udev device hierarchy:
    *   A **USB Parent Device** (e.g., `/sys/devices/virtual/usb/selkies_usb_ctrl0_dev`) with USB-specific attributes like `idVendor`, `idProduct`.
    *   An **Input Parent Device** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10`) which is a child of the USB device, providing common input attributes like `name`, `phys`, `uniq`.
    *   A **JS (Joystick) Device Node** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10/js0`) representing the traditional joystick interface (`/dev/input/jsX`).
    *   An **Event Device Node** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10/event1000`) representing the evdev interface (`/dev/input/eventX`).
4.  **Responds to Queries:** It answers `libudev` API calls (like `udev_enumerate_scan_devices`, `udev_device_get_property_value`, `udev_device_get_sysattr_value`, `udev_device_get_parent_with_subsystem_devtype`) for these virtual devices from the hardcoded data, and for every other device from the real library.
    *   The virtual gamepads are designed to mimic "Microsoft X-Box 360 pad" devices.
    *   Enumeration lists a js or event node only while the interposer's socket for it (`$SELKIES_JS_SOCKET_PATH/selkies_<node>.sock`) is bound; a pad served later is announced through the monitor as a hotplug add.
    *   A real input node that shares a virtual node's name (`js0`–`js3`, `event1000`–`event1003`) is hidden, since its `/dev/input` path is the one the joystick interposer serves.

## Key Features

*   **LD_PRELOADable:** Designed to intercept `libudev` calls without modifying the target application.
*   **Transparent Elsewhere:** Anything outside the virtual gamepads passes through to the real `libudev`, so a preloaded application (a nested KWin discovering its GPU, a browser probing webcams) behaves as without the preload.
*   **Virtual Gamepads:** Simulates `NUM_VIRTUAL_GAMEPADS` (default: 4) gamepads.
*   **Standard Hierarchy:** Presents devices with a plausible sysfs path and parent/child relationships.
*   **Common Properties/Attributes:** Provides essential udev properties (`DEVNAME`, `ID_INPUT_JOYSTICK`, etc.) and sysfs attributes (`idVendor`, `idProduct`, `name`, etc.).
*   **Debug Logging:** `JS_LOG=1` in the environment enables `stderr` logging (prefixed with `[fake_udev_dbg:]`, `[fake_udev_info:]`, etc.) to trace `libudev` calls and the library's responses.

## Limitations

*   **Static Data:** All virtual device information is hardcoded in `fake-libudev-core.c`.
*   **No Real Hardware Interaction:** These are purely virtual constructs. No actual `/dev/input/jsX` or `/dev/input/eventX` device nodes are created in the kernel. The library only makes applications *believe* they exist via `libudev`.
*   **Hotplug via inotify:** For the virtual gamepads, `udev_monitor_*` is backed by an inotify watch on the socket directory; creating/removing the interposer's device sockets (`selkies_js*.sock`, `selkies_event*.sock`) surfaces as `input`-subsystem `add`/`remove` events. Real hotplug events come from the real library's monitor over the same fd.
*   **Filters the Pads Cannot Honour:** sysattr, tag and is-initialized matches are forwarded to the real library but do not restrict the virtual gamepads.
*   **Fixed Number of Devices:** The number of virtual gamepads is determined at compile time by `NUM_VIRTUAL_GAMEPADS`.

## Build Instructions

1.  Ensure you have `gcc` and `make` installed.
2.  Navigate to this directory in your terminal.
3.  Run `make`:
```
make
```
    This will compile `fake-libudev-core.c` and produce:
    *   `libudev.so.1.0.0-fake` (the actual shared library file)
    *   `libudev.so.1` (symlink to `libudev.so.1.0.0-fake`, typically used for `soname`)
    *   `libudev.so` (symlink to `libudev.so.1`, typically used for linking)

    `make all32` builds the 32-bit variant (`libudev_x86.so.1.0.0-fake`) with `gcc-multilib`.

## Usage

To use this library, preload it when running your target application:

```
LD_PRELOAD=./libudev.so.1 /path/to/your/application [application_args]
```

*   Replace `./libudev.so.1` with the correct path to the compiled shared library if it's not in the current directory.
*   Replace `/path/to/your/application` with the actual application you want to run.

The application should then discover and interact with the virtual gamepads as if they were reported by the system's `libudev`, alongside everything the system's `libudev` reports itself.

## Creating Device Nodes (Manual Step)

This fake `libudev` library informs applications that device nodes like `/dev/input/js0` or `/dev/input/event1000` exist. However, **it does not create these nodes in the filesystem.** If your application attempts to `open()` these device nodes, they must actually exist.

You can create these dummy device nodes manually using `mknod` and set appropriate permissions. These nodes will not be backed by real hardware drivers but will allow `open()` calls to succeed.

**Important:** These commands typically require root privileges (e.g., run with `sudo`).

The major number for input devices is generally 13.
*   Minor numbers for `/dev/input/jsX` are `0` through `X`.
*   Minor numbers for `/dev/input/eventX` are `64 + X`.

```
# Create /dev/input directory if it doesn't exist
sudo mkdir -p /dev/input

# Create js device nodes (Major 13, Minor 0-3)
sudo mknod /dev/input/js0 c 13 0
sudo mknod /dev/input/js1 c 13 1
sudo mknod /dev/input/js2 c 13 2
sudo mknod /dev/input/js3 c 13 3

# Create event device nodes (Major 13, Minor 64 + event_id)
# event1000 (minor 64+1000 = 1064)
sudo mknod /dev/input/event1000 c 13 1064
# event1001 (minor 64+1001 = 1065)
sudo mknod /dev/input/event1001 c 13 1065
# event1002 (minor 64+1002 = 1066)
sudo mknod /dev/input/event1002 c 13 1066
# event1003 (minor 64+1003 = 1067)
sudo mknod /dev/input/event1003 c 13 1067

# Set permissions (e.g., world-readable/writable for simplicity in testing)
sudo chmod 0666 /dev/input/js{0,1,2,3}
sudo chmod 0666 /dev/input/event100{0,1,2,3}

# Verify (optional)
ls -l /dev/input/js* /dev/input/event100*
```

## Debugging

With `JS_LOG=1` in the environment the library outputs logging to `stderr`. This can be very helpful for:
*   Understanding which `libudev` functions your application is calling.
*   Seeing how the fake library is responding to these calls.
*   Troubleshooting why an application might not be "seeing" the virtual devices as expected.

Log messages are prefixed like:
*   `[fake_udev_dbg:]` for detailed debug messages.
*   `[fake_udev_info:]` for general informational messages.
*   `[fake_udev_warn:]` for warnings.
*   `[fake_udev_err:]` for errors.
