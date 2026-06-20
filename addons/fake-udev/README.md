# Fake Libudev Core (for Virtual Gamepads)

This subproject provides a fake `libudev` shared library (`libudev.so.1`) designed to be used with `LD_PRELOAD`. Its primary purpose is to simulate the presence of a predefined set of virtual gamepads for applications that use `libudev` to discover and query input devices.

This is particularly useful for testing applications or running them in environments where actual gamepad hardware is unavailable or where a full udev daemon setup is not feasible (e.g., certain containerized environments, CI/CD pipelines).

## How it Works

When an application linked against `libudev` is launched with this library preloaded, calls to `libudev` functions are intercepted by this fake implementation. Instead of querying the system's actual udev database, this library:

1.  **Initializes Virtual Device Data:** On the first relevant `libudev` call (e.g., `udev_new()`), it sets up internal data structures representing a fixed number (`NUM_VIRTUAL_GAMEPADS`, currently 4) of virtual gamepads.
2.  **Simulates Device Hierarchy:** Each virtual gamepad is represented with a typical udev device hierarchy:
    *   A **USB Parent Device** (e.g., `/sys/devices/virtual/usb/selkies_usb_ctrl0_dev`) with USB-specific attributes like `idVendor`, `idProduct`.
    *   An **Input Parent Device** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10`) which is a child of the USB device, providing common input attributes like `name`, `phys`, `uniq`.
    *   A **JS (Joystick) Device Node** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10/js0`) representing the traditional joystick interface (`/dev/input/jsX`).
    *   An **Event Device Node** (e.g., `/sys/devices/virtual/selkies_pad0/input/input10/event1000`) representing the evdev interface (`/dev/input/eventX`).
3.  **Responds to Queries:** It responds to `libudev` API calls (like `udev_enumerate_scan_devices`, `udev_device_get_property_value`, `udev_device_get_sysattr_value`, `udev_device_get_parent_with_subsystem_devtype`) using the hardcoded data for these virtual devices.
    *   The virtual gamepads are designed to mimic "Microsoft X-Box 360 pad" devices.

## Key Features

*   **LD_PRELOADable:** Designed to intercept `libudev` calls without modifying the target application.
*   **Virtual Gamepads:** Simulates `NUM_VIRTUAL_GAMEPADS` (default: 4) gamepads.
*   **Standard Hierarchy:** Presents devices with a plausible sysfs path and parent/child relationships.
*   **Common Properties/Attributes:** Provides essential udev properties (`DEVNAME`, `ID_INPUT_JOYSTICK`, etc.) and sysfs attributes (`idVendor`, `idProduct`, `name`, etc.).
*   **Targeted Implementation:** Implements core `libudev` functions relevant for device enumeration and property querying.
*   **Debug Logging:** Includes extensive `fprintf(stderr, ...)` logging (prefixed with `[fake_udev_dbg:]`, `[fake_udev_info:]`, etc.) to trace `libudev` calls and the library's responses.

## Limitations

*   **Static Data:** All device information is hardcoded in `fake-libudev-core.c`. It does not interact with the system's actual udev daemon or sysfs beyond what's necessary for the simulation.
*   **No Real Hardware Interaction:** These are purely virtual constructs. No actual `/dev/input/jsX` or `/dev/input/eventX` device nodes are created in the kernel. The library only makes applications *believe* they exist via `libudev`.
*   **Hotplug via inotify:** `udev_monitor_*` is backed by an inotify watch on `/tmp`; creating/removing the interposer's device sockets (`/tmp/selkies_js*.sock`, `/tmp/selkies_event*.sock`) surfaces as `input`-subsystem `add`/`remove` events. Remaining `udev_monitor_*` calls are minimal.
*   **HWDB Stubs:** `udev_hwdb_*` functions are stubs.
*   **Limited Scope:** Primarily focused on the "input" subsystem and devices that look like gamepads. Other udev functionalities or device types are not implemented or are minimally stubbed.
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

## Usage

To use this fake library, preload it when running your target application:

```
LD_PRELOAD=./libudev.so.1 /path/to/your/application [application_args]
```

*   Replace `./libudev.so.1` with the correct path to the compiled shared library if it's not in the current directory.
*   Replace `/path/to/your/application` with the actual application you want to run.

The application should then discover and interact with the virtual gamepads as if they were reported by the system's `libudev`.

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

The library outputs detailed logging to `stderr`. This can be very helpful for:
*   Understanding which `libudev` functions your application is calling.
*   Seeing how the fake library is responding to these calls.
*   Troubleshooting why an application might not be "seeing" the virtual devices as expected.

Log messages are prefixed like:
*   `[fake_udev_dbg:]` for detailed debug messages.
*   `[fake_udev_info:]` for general informational messages.
*   `[fake_udev_warn:]` for warnings.
*   `[fake_udev_err:]` for errors.
