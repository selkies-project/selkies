# Test-only helpers.
#
#   uinput_shim.so    emulator for /dev/uinput: records the setup ioctls and
#                     diverts the event writes to a file, so the kernel gamepad
#                     path runs on hosts (CI included) with no uinput node
#   uinput_abi_truth  prints the ioctl encodings and struct sizes the kernel
#                     headers define, which tests/unit compares against the
#                     constants in selkies.input_handler
#   v4l2probe         libc-only V4L2 capture client (enumerate, MMAP or read()
#                     stream, pixel samples), run under the V4L2 interposer or
#                     against a kernel device by the webcam tests
#
# The gamepad/ tools inspect the interposer backend by hand and need SDL2 and
# libudev, so `make gamepad` builds them on request rather than `make all`.
CC = gcc
CFLAGS ?= -O2 -Wall

all: uinput_shim.so uinput_abi_truth v4l2probe

uinput_shim.so: uinput_shim.c
	$(CC) $(CFLAGS) -shared -fPIC -o $@ $< -ldl

uinput_abi_truth: uinput_abi_truth.c
	$(CC) $(CFLAGS) -o $@ $<

v4l2probe: v4l2probe.c
	$(CC) $(CFLAGS) -o $@ $<

gamepad: gamepad/jsread gamepad/sdlenum gamepad/sdlread gamepad/sdlhotplug gamepad/udevscan

gamepad/jsread: gamepad/jsread.c
	$(CC) $(CFLAGS) -o $@ $<

gamepad/sdlenum: gamepad/sdlenum.c
	$(CC) $(CFLAGS) -o $@ $< $(shell pkg-config --cflags --libs sdl2)

gamepad/sdlread: gamepad/sdlread.c
	$(CC) $(CFLAGS) -o $@ $< $(shell pkg-config --cflags --libs sdl2)

gamepad/sdlhotplug: gamepad/sdlhotplug.c
	$(CC) $(CFLAGS) -o $@ $< $(shell pkg-config --cflags --libs sdl2)

gamepad/udevscan: gamepad/udevscan.c
	$(CC) $(CFLAGS) -o $@ $< -ludev

clean:
	rm -f uinput_shim.so uinput_abi_truth v4l2probe gamepad/jsread gamepad/sdlenum gamepad/sdlread gamepad/sdlhotplug gamepad/udevscan

.PHONY: all gamepad clean
