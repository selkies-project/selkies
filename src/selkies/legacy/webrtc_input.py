# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

import Xlib
from Xlib import display
from Xlib.ext import xfixes, xtest
import asyncio
import base64
import pynput
import io
import msgpack
import re
import os
import subprocess
import socket
import time
import ctypes
import ctypes.util
from PIL import Image
from gamepad import SelkiesGamepad

import logging
logger = logging.getLogger("webrtc_input")
logger.setLevel(logging.INFO)


# ---------------------------------------------------------------------------
# X11 cursor name via ctypes (XFixesGetCursorImageAndName, opcode 25)
#
# python-xlib's xfixes_get_cursor_image() returns pixel data but no cursor
# name. The name is available via XFixesGetCursorImageAndName in libXfixes,
# which we call directly via ctypes.
# ---------------------------------------------------------------------------

class _XFixesCursorImage(ctypes.Structure):
    """XFixesCursorImage struct layout on x86_64 (includes name fields)."""
    _fields_ = [
        ("x", ctypes.c_short), ("y", ctypes.c_short),
        ("width", ctypes.c_ushort), ("height", ctypes.c_ushort),
        ("xhot", ctypes.c_ushort), ("yhot", ctypes.c_ushort),
        ("cursor_serial", ctypes.c_ulong), ("pixels", ctypes.c_void_p),
        ("atom", ctypes.c_ulong), ("name", ctypes.c_char_p),
    ]

_cn_display = None
_cn_libs = None
_cn_init_done = False

def _init_cursor_name():
    global _cn_display, _cn_libs, _cn_init_done
    if _cn_init_done:
        return _cn_display is not None
    _cn_init_done = True
    try:
        x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
        xfixes = ctypes.CDLL(ctypes.util.find_library("Xfixes"))
        x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x11.XOpenDisplay.restype = ctypes.c_void_p
        x11.XFree.argtypes = [ctypes.c_void_p]
        x11.XFree.restype = ctypes.c_int
        xfixes.XFixesQueryExtension.argtypes = [
            ctypes.c_void_p, ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int)]
        xfixes.XFixesQueryExtension.restype = ctypes.c_int
        xfixes.XFixesGetCursorImageAndName.argtypes = [ctypes.c_void_p]
        xfixes.XFixesGetCursorImageAndName.restype = ctypes.POINTER(_XFixesCursorImage)
        dpy = x11.XOpenDisplay(os.environ.get("DISPLAY", ":99").encode())
        if not dpy:
            return False
        eb, er = ctypes.c_int(), ctypes.c_int()
        if not xfixes.XFixesQueryExtension(dpy, ctypes.byref(eb), ctypes.byref(er)):
            return False
        _cn_display = dpy
        _cn_libs = (x11, xfixes)
        logger.info("cursor name support initialized (XFixesGetCursorImageAndName)")
        return True
    except Exception as e:
        logger.warning("cursor name support unavailable: %s", e)
        return False

def _get_cursor_name():
    """Return the current X11 cursor name (e.g. 'left_ptr', 'hand2'), or None."""
    if not _init_cursor_name():
        return None
    try:
        x11, xfixes = _cn_libs
        result = xfixes.XFixesGetCursorImageAndName(_cn_display)
        if not result:
            return None
        name = result.contents.name
        cursor_name = name.decode("utf-8", errors="replace") if name else None
        x11.XFree(result)
        return cursor_name
    except Exception:
        return None

# Local enumerations for mouse actions.
MOUSE_POSITION = 10
MOUSE_MOVE = 11
MOUSE_SCROLL_UP = 20
MOUSE_SCROLL_DOWN = 21
MOUSE_BUTTON_PRESS = 30
MOUSE_BUTTON_RELEASE = 31
MOUSE_BUTTON = 40
MOUSE_BUTTON_LEFT = 41
MOUSE_BUTTON_MIDDLE = 42
MOUSE_BUTTON_RIGHT = 43

UINPUT_BTN_LEFT = (0x01, 0x110)
UINPUT_BTN_MIDDLE = (0x01, 0x112)
UINPUT_BTN_RIGHT = (0x01, 0x111)
UINPUT_REL_X = (0x02, 0x00)
UINPUT_REL_Y = (0x02, 0x01)
UINPUT_REL_WHEEL = (0x02, 0x08)

# Local map for uinput and pynput buttons
MOUSE_BUTTON_MAP = {
    MOUSE_BUTTON_LEFT: {
        "uinput": UINPUT_BTN_LEFT,
        "pynput": pynput.mouse.Button.left,
    },
    MOUSE_BUTTON_MIDDLE: {
        "uinput": UINPUT_BTN_MIDDLE,
        "pynput": pynput.mouse.Button.middle,
    },
    MOUSE_BUTTON_RIGHT: {
        "uinput": UINPUT_BTN_RIGHT,
        "pynput": pynput.mouse.Button.right,
    },
}


class WebRTCInputError(Exception):
    pass


class WebRTCInput:
    def __init__(self, uinput_mouse_socket_path="", js_socket_path="", enable_clipboard="", enable_cursors=True, cursor_size=16, cursor_scale=1.0, cursor_debug=False):
        """Initializes WebRTC input instance
        """

        self.clipboard_running = False
        self.uinput_mouse_socket_path = uinput_mouse_socket_path
        self.uinput_mouse_socket = None

        # Map of gamepad numbers to socket paths
        self.js_socket_path_map = {i: os.path.join(js_socket_path, "selkies_js%d.sock" % i) for i in range(4)}

        # Map of gamepad number to SelkiesGamepad objects
        self.js_map = {}

        self.enable_clipboard = enable_clipboard

        self.enable_cursors = enable_cursors
        self.cursors_running = False
        self.cursor_cache = {}
        self.cursor_scale = cursor_scale
        self.cursor_size = cursor_size
        self.cursor_debug = cursor_debug

        self.keyboard = None
        self.mouse = None
        self.joystick = None
        self.xdisplay = None
        self.button_mask = 0

        self.ping_start = None

        self.on_video_encoder_bit_rate = lambda bitrate: logger.warning(
            'unhandled on_video_encoder_bit_rate')
        self.on_audio_encoder_bit_rate = lambda bitrate: logger.warning(
            'unhandled on_audio_encoder_bit_rate')
        self.on_mouse_pointer_visible = lambda visible: logger.warning(
            'unhandled on_mouse_pointer_visible')
        self.on_clipboard_read = lambda data: logger.warning(
            'unhandled on_clipboard_read')
        self.on_set_fps = lambda fps: logger.warning(
            'unhandled on_set_fps')
        self.on_set_enable_resize = lambda enable_resize, res: logger.warning(
            'unhandled on_set_enable_resize')
        self.on_client_fps = lambda fps: logger.warning(
            'unhandled on_client_fps')
        self.on_client_latency = lambda latency: logger.warning(
            'unhandled on_client_latency')
        self.on_resize = lambda res: logger.warning(
            'unhandled on_resize')
        self.on_scaling_ratio = lambda res: logger.warning(
            'unhandled on_scaling_ratio')
        self.on_ping_response = lambda latency: logger.warning(
            'unhandled on_ping_response')
        self.on_cursor_change = lambda msg: logger.warning(
            'unhandled on_cursor_change')
        self.on_client_webrtc_stats = lambda webrtc_stat_type, webrtc_stats: logger.warning(
            'unhandled on_client_webrtc_stats')

    def __keyboard_connect(self):
        self.keyboard = pynput.keyboard.Controller()

    def __mouse_connect(self):
        if self.uinput_mouse_socket_path:
            # Proxy uinput mouse commands through unix domain socket.
            logger.info("Connecting to uinput mouse socket: %s" %
                        self.uinput_mouse_socket_path)
            self.uinput_mouse_socket = socket.socket(
                socket.AF_UNIX, socket.SOCK_DGRAM)

        self.mouse = pynput.mouse.Controller()

    def __mouse_disconnect(self):
        if self.mouse:
            del self.mouse
            self.mouse = None

    def __mouse_emit(self, *args, **kwargs):
        if self.uinput_mouse_socket_path:
            cmd = {"args": args, "kwargs": kwargs}
            data = msgpack.packb(cmd, use_bin_type=True)
            self.uinput_mouse_socket.sendto(
                data, self.uinput_mouse_socket_path)

    def __js_connect(self, js_num, name, num_btns, num_axes):
        """Connect virtual joystick using Selkies Joystick Interposer
        """

        logger.info("creating selkies gamepad for js%d, name: '%s', buttons: %d, axes: %d" % (js_num, name, num_btns, num_axes))

        socket_path = self.js_socket_path_map.get(js_num, None)
        if socket_path is None:
            logger.error("failed to connect js%d because socket_path was not found" % js_num)
            return

        # Create the gamepad and button config.
        js = SelkiesGamepad(socket_path)
        js.set_config(name, num_btns, num_axes)

        asyncio.create_task(js.run_server())

        self.js_map[js_num] = js

    async def __js_disconnect(self, js_num=None):
        if js_num is None:
            # stop all gamepads.
            for js in self.js_map.values():
                await asyncio.to_thread(js.stop_server)
            self.js_map = {}
            return
        
        js = await asyncio.to_thread(self.js_map.get, js_num, None)
        if js is not None:
            logger.info("stopping gamepad %d" % js_num)
            await asyncio.to_thread(js.stop_server)
            del self.js_map[js_num]

    def __js_emit_btn(self, js_num, btn_num, btn_val):
        js = self.js_map.get(js_num, None)
        if js is None:
            logger.error("cannot send button because js%d is not connected" % js_num)
            return

        logger.debug("sending js%d button num %d with val %d" % (js_num, btn_num, btn_val))

        js.send_btn(btn_num, btn_val)

    def __js_emit_axis(self, js_num, axis_num, axis_val):
        js = self.js_map.get(js_num, None)
        if js is None:
            logger.error("cannot send axis because js%d is not connected" % js_num)
            return

        logger.debug("sending js%d axis num %d with val %d" % (js_num, axis_num, axis_val))

        js.send_axis(axis_num, axis_val)

    async def connect(self):
        # Create connection to the X11 server provided by the DISPLAY env var.
        self.xdisplay = display.Display()

        self.__keyboard_connect()

        # Clear any stuck modifier keys
        self.reset_keyboard()

        self.__mouse_connect()

    async def disconnect(self):
        await self.__js_disconnect()
        self.__mouse_disconnect()

    def reset_keyboard(self):
        """Resets any stuck modifier keys
        """

        logger.info("Resetting keyboard modifiers.")

        lctrl = 65507
        lshift = 65505
        lalt = 65513

        rctrl = 65508
        rshift = 65506
        ralt = 65027

        lmeta = 65511
        rmeta = 65512

        keyf = 102
        keyF = 70

        keym = 109
        keyM = 77

        escape = 65307

        for k in [lctrl, lshift, lalt, rctrl, rshift, ralt, lmeta, rmeta, keyf, keyF, keym, keyM, escape]:
            self.send_x11_keypress(k, down=False)

    def send_mouse(self, action, data):
        if action == MOUSE_POSITION:
            # data is a tuple of (x, y)
            # using X11 mouse even when virtual mouse is enabled for non-relative actions.
            if self.mouse:
                self.mouse.position = data
        elif action == MOUSE_MOVE:
            # data is a tuple of (x, y)
            x, y = data
            if self.uinput_mouse_socket_path:
                # Send relative motion to uinput device.
                # syn=False delays the sync until the second command.
                self.__mouse_emit(UINPUT_REL_X, x, syn=False)
                self.__mouse_emit(UINPUT_REL_Y, y)
            else:
                # NOTE: the pynput mouse.move method moves the mouse relative to the current position using its internal tracked position.
                #       this does not work for relative motion where the input should just be a delta value.
                #       instead, send the X fake input directly.
                xtest.fake_input(self.xdisplay, Xlib.X.MotionNotify, detail=True, root=Xlib.X.NONE, x=x, y=y)
                self.xdisplay.sync()
        elif action == MOUSE_SCROLL_UP:
            # Scroll up
            if self.uinput_mouse_socket_path:
                self.__mouse_emit(UINPUT_REL_WHEEL, 1)
            else:
                self.mouse.scroll(0, -1)
        elif action == MOUSE_SCROLL_DOWN:
            # Scroll down
            if self.uinput_mouse_socket_path:
                self.__mouse_emit(UINPUT_REL_WHEEL, -1)
            else:
                self.mouse.scroll(0, 1)
        elif action == MOUSE_BUTTON:
            # Button press/release, data is a tuple of (MOUSE_BUTTON_PRESS|MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_enum)
            if self.uinput_mouse_socket_path:
                btn = MOUSE_BUTTON_MAP[data[1]]["uinput"]
            else:
                btn = MOUSE_BUTTON_MAP[data[1]]["pynput"]

            if data[0] == MOUSE_BUTTON_PRESS:
                if self.uinput_mouse_socket_path:
                    self.__mouse_emit(btn, 1)
                else:
                    self.mouse.press(btn)
            else:
                if self.uinput_mouse_socket_path:
                    self.__mouse_emit(btn, 0)
                else:
                    self.mouse.release(btn)

    def send_x11_keypress(self, keysym, down=True):
        """Sends keypress to X server

        The key sym is converted to a keycode using the X server library.

        Arguments:
            keysym {integer} -- the key symbol to send

        Keyword Arguments:
            down {bool} -- toggle key down or up (default: {True})
        """

        try:
            # With the Generic 105-key PC layout (default in Linux without a real keyboard), the key '<' is redirected to keycode 94
            # Because keycode 94 with Shift pressed is instead the key '>', the keysym for '<' should instead be redirected to ','
            # Although prevented in most cases, this fix may present issues in some keyboard layouts
            if keysym == 60 and self.keyboard._display.keysym_to_keycode(keysym) == 94:
                keysym = 44
            keycode = pynput.keyboard.KeyCode(keysym)
            if down:
                self.keyboard.press(keycode)
            else:
                self.keyboard.release(keycode)
        except Exception as e:
            logger.error('failed to send keypress: {}'.format(e))

    def send_x11_mouse(self, x, y, button_mask, scroll_magnitude, relative=False):
        """Sends mouse events to the X server.

        The mouse button mask is stored locally to keep track of press/release state.

        Relative motion is sent to the uninput device for improved tracking.

        Arguments:
            x {integer} -- mouse position X
            y {integer} -- mouse position Y
            scroll_magnitude {integer} -- 
            button_mask {integer} -- mask of 5 mouse buttons, button 1 is at the LSB.
        """

        # Mouse motion
        if relative:
            self.send_mouse(MOUSE_MOVE, (x, y))
        else:
            self.send_mouse(MOUSE_POSITION, (x, y))

        # Button press and release
        if button_mask != self.button_mask:
            max_buttons = 5
            for i in range(0, max_buttons):
                if (button_mask ^ self.button_mask) & (1 << i):

                    action = MOUSE_BUTTON
                    btn_action = MOUSE_BUTTON_PRESS
                    btn_num = MOUSE_BUTTON_LEFT

                    if (button_mask & (1 << i)):
                        btn_action = MOUSE_BUTTON_PRESS
                    else:
                        btn_action = MOUSE_BUTTON_RELEASE

                    # Remap middle and right buttons in relative mode.
                    if i == 1:
                        btn_num = MOUSE_BUTTON_MIDDLE
                    elif i == 2:
                        btn_num = MOUSE_BUTTON_RIGHT
                    elif i == 3 and button_mask != 0:
                        # Wheel up
                        action = MOUSE_SCROLL_UP
                    elif i == 4 and button_mask != 0:
                        # Wheel down
                        action = MOUSE_SCROLL_DOWN

                    data = (btn_action, btn_num)

                    # if event is scroll up/down then send the event multiple times
                    # based on the received scroll magnitue for smoother scroll experience
                    if i == 3 or i == 4:
                        for i in range(1, scroll_magnitude):
                            self.send_mouse(action, data)

                    self.send_mouse(action, data)

            # Update the button mask to remember positions.
            self.button_mask = button_mask

        if not relative:
            self.xdisplay.sync()

    def read_clipboard(self):
        try:
            result = subprocess.run(('xsel', '--clipboard', '--output'), check=True, text=True, capture_output=True, timeout=3)
            return result.stdout
        except subprocess.SubprocessError as e:
            logger.warning(f"Error while capturing clipboard: {e}")

    def write_clipboard(self, data):
        try:
            subprocess.run(('xsel', '--clipboard', '--input'), input=data.encode(), check=True, timeout=3)
            return True
        except subprocess.SubprocessError as e:
            logger.warning(f"Error while writing to clipboard: {e}")
            return False

    async def start_clipboard(self):
        if self.enable_clipboard in ["true", "out"]:
            logger.info("starting clipboard monitor")
            self.clipboard_running = True
            last_data = ""
            while self.clipboard_running:
                curr_data = self.read_clipboard()
                if curr_data and curr_data != last_data:
                    logger.info(
                        "sending clipboard content, length: %d" % len(curr_data))
                    self.on_clipboard_read(curr_data)
                    last_data = curr_data
                await asyncio.sleep(0.5)
            logger.info("clipboard monitor stopped")
        else:
            logger.info("skipping outbound clipboard service.")

    def stop_clipboard(self):
        logger.info("stopping clipboard monitor")
        self.clipboard_running = False

    async def start_cursor_monitor(self):
        if not self.xdisplay.has_extension('XFIXES'):
            if self.xdisplay.query_extension('XFIXES') is None:
                logger.error(
                    'XFIXES extension not supported, cannot watch cursor changes')
                return

        xfixes_version = self.xdisplay.xfixes_query_version()
        logger.info('Found XFIXES version %s.%s' % (
            xfixes_version.major_version,
            xfixes_version.minor_version,
        ))

        logger.info("starting cursor monitor")
        self.cursor_cache = {}
        self.cursors_running = True
        screen = self.xdisplay.screen()
        self.xdisplay.xfixes_select_cursor_input(
            screen.root, xfixes.XFixesDisplayCursorNotifyMask)
        logger.info("watching for cursor changes")

        # Fetch initial cursor
        try:
            image = self.xdisplay.xfixes_get_cursor_image(screen.root)
            self.cursor_cache[image.cursor_serial] = self.cursor_to_msg(
                image, self.cursor_scale, self.cursor_size)
            self.on_cursor_change(self.cursor_cache[image.cursor_serial])
        except Exception as e:
            logger.warning("exception from fetching cursor image: %s" % e)

        while self.cursors_running:
            if self.xdisplay.pending_events() == 0:
                await asyncio.sleep(0.1)
                continue
            event = self.xdisplay.next_event()
            if (event.type, 0) == self.xdisplay.extension_event.DisplayCursorNotify:
                cache_key = event.cursor_serial
                if cache_key in self.cursor_cache:
                    if self.cursor_debug:
                        logger.warning(
                            "cursor changed to cached serial: {}".format(cache_key))
                else:
                    try:
                        # Request the cursor image.
                        cursor = self.xdisplay.xfixes_get_cursor_image(
                            screen.root)

                        # Convert cursor image and cache.
                        self.cursor_cache[cache_key] = self.cursor_to_msg(
                            cursor, self.cursor_scale, self.cursor_size)

                        if self.cursor_debug:
                            logger.warning("New cursor: position={},{}, size={}x{}, length={}, xyhot={},{}, cursor_serial={}".format(
                                cursor.x, cursor.y, cursor.width, cursor.height, len(cursor.cursor_image), cursor.xhot, cursor.yhot, cursor.cursor_serial))
                    except Exception as e:
                        logger.warning(
                            "exception from fetching cursor image: %s" % e)

                self.on_cursor_change(self.cursor_cache.get(cache_key))

        logger.info("cursor monitor stopped")

    def stop_cursor_monitor(self):
        logger.info("stopping cursor monitor")
        self.cursors_running = False

    def cursor_to_msg(self, cursor, scale=1.0, cursor_size=-1):
        if cursor_size > -1:
            target_width = cursor_size
            target_height = cursor_size
            xhot_scaled = int(cursor_size/cursor.width * cursor.xhot)
            yhot_scaled = int(cursor_size/cursor.height * cursor.yhot)
        else:
            target_width = int(cursor.width * scale)
            target_height = int(cursor.height * scale)
            xhot_scaled = int(cursor.xhot * scale)
            yhot_scaled = int(cursor.yhot * scale)

        png_data_b64 = base64.b64encode(
            self.cursor_to_png(cursor, target_width, target_height))

        override = None
        if sum(cursor.cursor_image) == 0:
            override = "none"

        msg = {
            "curdata": png_data_b64.decode(),
            "handle": cursor.cursor_serial,
            "override": override,
            "hotspot": {
                "x": xhot_scaled,
                "y": yhot_scaled,
            },
        }
        cursor_name = _get_cursor_name()
        if cursor_name:
            msg["cursor_name"] = cursor_name
        return msg

    def cursor_to_png(self, cursor, resize_width, resize_height):
        with io.BytesIO() as f:
            # Extract each component to RGBA bytes.
            s = [((i >> b) & 0xFF)
                 for i in cursor.cursor_image for b in [16, 8, 0, 24]]

            # Create raw image from pixel bytes
            im = Image.frombytes(
                'RGBA', (cursor.width, cursor.height), bytes(s), 'raw')

            if cursor.width != resize_width or cursor.height != resize_height:
                # Resize cursor to target size
                im = im.resize((resize_width, resize_height))

            # Save image as PNG
            im.save(f, "PNG")
            data = f.getvalue()

            if self.cursor_debug:
                with open("/tmp/cursor_%d.png" % cursor.cursor_serial, 'wb') as debugf:
                    debugf.write(data)
            return data

    async def stop_js_server(self):
        await self.__js_disconnect()

    async def on_message(self, msg):
        """Handles incoming input messages

        Bound to a data channel, handles input messages.

        Message format: <command>,<data>

        Supported message commands:
          kd: key down event, data is keysym
          ku: key up event, data is keysym
          m: mouse event, data is csv of: x,y,button mask
          b: bitrate event, data is the desired encoder bitrate in bps.
          js: joystick connect/disconnect/button/axis event

        Arguments:
            msg {string} -- the raw data channel message packed in the <command>,<data> format.
        """

        toks = msg.split(",")
        if toks[0] == "pong":
            if self.ping_start is None:
                logger.warning('received pong before ping')
                return

            roundtrip = time.time() - self.ping_start
            latency = (roundtrip / 2) * 1000
            latency = float("%.3f" % latency)
            self.on_ping_response(latency)
        elif toks[0] == "kd":
            # Key down
            self.send_x11_keypress(int(toks[1]), down=True)
        elif toks[0] == "ku":
            # Key up
            self.send_x11_keypress(int(toks[1]), down=False)
        elif toks[0] == "kr":
            # Keyboard reset
            self.reset_keyboard()
        elif toks[0] in ["m", "m2"]:
            # Mouse action
            # x,y,button_mask
            relative = False
            if toks[0] == "m2":
                relative = True
            try:
                x, y, button_mask, scroll_magnitude = [int(i) for i in toks[1:]]
            except:
                x, y, button_mask, scroll_magnitude = 0, 0, self.button_mask, 0
                relative = False
            try:
                self.send_x11_mouse(x, y, button_mask, scroll_magnitude, relative)
            except Exception as e:
                logger.warning('failed to set mouse cursor: {}'.format(e))
        elif toks[0] == "p":
            # toggle mouse pointer visibility
            visible = bool(int(toks[1]))
            logger.info("Setting pointer visibility to: %s" % str(visible))
            self.on_mouse_pointer_visible(visible)
        elif toks[0] == "vb":
            # Set video bitrate
            bitrate = int(toks[1])
            logger.info("Setting video bitrate to: %d" % bitrate)
            self.on_video_encoder_bit_rate(bitrate)
        elif toks[0] == "ab":
            # Set audio bitrate
            bitrate = int(toks[1])
            logger.info("Setting audio bitrate to: %d" % bitrate)
            self.on_audio_encoder_bit_rate(bitrate)
        elif toks[0] == "js":
            # Joystick
            # button: b,<btn_num>,<value>
            # axis: a,<axis_num>,<value>
            if toks[1] == 'c':
                js_num = int(toks[2])
                name = base64.b64decode(toks[3]).decode()[:255]
                num_axes = int(toks[4])
                num_btns = int(toks[5])
                self.__js_connect(js_num, name, num_btns, num_axes)
            elif toks[1] == 'd':
                js_num = int(toks[2])
                await self.__js_disconnect(js_num)
            elif toks[1] == 'b':
                js_num = int(toks[2])
                btn_num = int(toks[3])
                btn_val = float(toks[4])
                self.__js_emit_btn(js_num, btn_num, btn_val)
            elif toks[1] == 'a':
                js_num = int(toks[2])
                axis_num = int(toks[3])
                axis_val = float(toks[4])
                self.__js_emit_axis(js_num, axis_num, axis_val)
            else:
                logger.warning('unhandled joystick command: %s' % toks[1])
        elif toks[0] == "cr":
            # Clipboard read
            if self.enable_clipboard in ["true", "out"]:
                data = self.read_clipboard()
                if data:
                    logger.info("read clipboard content, length: %d" %
                                len(data))
                    self.on_clipboard_read(data)
                else:
                    logger.warning("no clipboard content to send")
            else:
                logger.warning(
                    "rejecting clipboard read because outbound clipboard is disabled.")
        elif toks[0] == "cw":
            # Clipboard write
            if self.enable_clipboard in ["true", "in"]:
                data = base64.b64decode(toks[1]).decode("utf-8")
                self.write_clipboard(data)
                logger.info("set clipboard content, length: %d" % len(data))
            else:
                logger.warning(
                    "rejecting clipboard write because inbound clipboard is disabled.")
        elif toks[0] == "r":
            # resize event
            res = toks[1]
            if re.match(re.compile(r'^\d+x\d+$'), res):
                # Make sure resolution is divisible by 2
                w, h = [int(i) + int(i) % 2 for i in res.split("x")]
                self.on_resize("%dx%d" % (w, h))
            else:
                logger.warning(
                    "rejecting resolution change, invalid WxH resolution: %s" % res)
        elif toks[0] == "s":
            # scaling info
            scale = toks[1]
            if re.match(re.compile(r'^\d+(\.\d+)?$'), scale):
                self.on_scaling_ratio(float(scale))
            else:
                logger.warning(
                    "rejecting scaling change, invalid scale ratio: %s" % scale)
        elif toks[0] == "_arg_fps":
            # Set framerate
            fps = int(toks[1])
            logger.info("Setting framerate to: %d" % fps)
            self.on_set_fps(fps)
        elif toks[0] == "_arg_resize":
            if len(toks) != 3:
                logger.error("invalid _arg_resize command, expected 2 arguments <enabled>,<resolution>")
            else:
                # Set resizing enabled
                enabled = toks[1].lower() == "true"
                logger.info("Setting enable_resize to : %s" % str(enabled))

                res = toks[2]
                if re.match(re.compile(r'^\d+x\d+$'), res):
                    # Make sure resolution is divisible by 2
                    w, h = [int(i) + int(i) % 2 for i in res.split("x")]
                    enable_res = "%dx%d" % (w, h)
                else:
                    logger.warning(
                        "rejecting enable resize with resolution change to invalid resolution: %s" % res)
                    enable_res = None

                self.on_set_enable_resize(enabled, enable_res)
        elif toks[0] == "_f":
            # Reported FPS from client.
            try:
                fps = int(toks[1])
                self.on_client_fps(fps)
            except:
                logger.error("failed to parse fps from client: " + str(toks))
        elif toks[0] == "_l":
            # Reported latency from client.
            try:
                latency_ms = int(toks[1])
                self.on_client_latency(latency_ms)
            except:
                logger.error(
                    "failed to parse latency report from client" + str(toks))
        elif toks[0] == "_stats_video" or toks[0] == "_stats_audio":
            # WebRTC Statistics API data from client
            try:
                await self.on_client_webrtc_stats(toks[0], ",".join(toks[1:]))
            except:
                logger.error("failed to parse WebRTC Statistics JSON object")
        else:
            logger.info('unknown data channel message: %s' % msg)
