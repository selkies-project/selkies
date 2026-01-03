# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
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

import ctypes
import logging
import struct
import time
import asyncio
from asyncio import subprocess
import socket
import os
import base64
import io
import re
import json
from PIL import Image 

try:
    from xkbcommon import xkb
except ImportError:
    xkb = None

try:
    import pynput
    import Xlib
    from Xlib import display
    from Xlib import X
    from Xlib import XK
    from Xlib.ext import xfixes, xtest
    X11_LIBS_AVAILABLE = True
except ImportError:
    X11_LIBS_AVAILABLE = False
    pynput = None
    Xlib = None
    display = None
    X = None
    XK = None
    xfixes = None
    xtest = None
import msgpack
import distro

logger_webrtc_input = logging.getLogger("webrtc_input")
logger_selkies_gamepad = logging.getLogger("selkies_gamepad")

# EVDEV Event Codes (from linux/input-event-codes.h)
EV_SYN = 0x00
EV_KEY = 0x01
EV_REL = 0x02
EV_ABS = 0x03
EV_MSC = 0x04
SYN_REPORT = 0

# Mouse Button Codes (from linux/input-event-codes.h)
BTN_MOUSE = 0x110
BTN_LEFT = 0x110
BTN_RIGHT = 0x111
BTN_MIDDLE = 0x112
BTN_SIDE = 0x113
BTN_EXTRA = 0x114

# Gamepad Button Codes
BTN_A = 0x130       # Or BTN_SOUTH
BTN_B = 0x131       # Or BTN_EAST
BTN_C = 0x132       # Typically BTN_C in evdev, for matching XBox360 bitmask
BTN_X = 0x133       # Or BTN_NORTH
BTN_Y = 0x134       # Or BTN_WEST
BTN_Z = 0x135       # Typically BTN_Z in evdev, for matching XBox360 bitmask
BTN_TL = 0x136      # Left Bumper
BTN_TR = 0x137      # Right Bumper
BTN_SELECT = 0x13a  # Back button
BTN_START = 0x13b   # Start button
BTN_MODE = 0x13c    # Xbox/Guide button
BTN_THUMBL = 0x13d  # Left Thumbstick click
BTN_THUMBR = 0x13e  # Right Thumbstick click


# Absolute Axis Codes
ABS_X = 0x00
ABS_Y = 0x01
ABS_Z = 0x02      # Often Left Trigger
ABS_RX = 0x03
ABS_RY = 0x04
ABS_RZ = 0x05     # Often Right Trigger
ABS_HAT0X = 0x10
ABS_HAT0Y = 0x11

# JS Event types (from linux/joystick.h, used by the JS-like interface)
JS_EVENT_BUTTON = 0x01
JS_EVENT_AXIS = 0x02
JS_EVENT_INIT = 0x80

# For js_config_t struct packing for the C interposer
# These are the max sizes in the C struct js_config_t
INTERPOSER_MAX_BTNS = 512
INTERPOSER_MAX_AXES = 64
CONTROLLER_NAME_MAX_LEN = 255 
C_INTERPOSER_STRUCT_SIZE = 1360

# Max clipboard chunk size
CLIPBOARD_CHUNK_SIZE = 750 * 1024 

# For mouse input to send fake back and forward events
KEYSYM_ALT_L = 0xFFE9     # Left Alt keysym
KEYSYM_LEFT_ARROW = 0xFF51 # Left Arrow keysym
KEYSYM_RIGHT_ARROW = 0xFF53# Right Arrow keysym

# Import keysyms
try:
    from .server_keysym_map import X11_KEYSYM_MAP
except ImportError:
    logger_webrtc_input = logging.getLogger("webrtc_input_fallback_map_import")
    logger_webrtc_input.warning(
        "server_keysym_map.py not found or X11_KEYSYM_MAP not defined. "
        "Keysym mapping will rely entirely on fallback."
    )
    X11_KEYSYM_MAP = {}

class JsConfigCtypes(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_char * CONTROLLER_NAME_MAX_LEN),
        ("vendor", ctypes.c_uint16),
        ("product", ctypes.c_uint16),
        ("version", ctypes.c_uint16),
        ("num_btns", ctypes.c_uint16),
        ("num_axes", ctypes.c_uint16),
        ("btn_map", ctypes.c_uint16 * INTERPOSER_MAX_BTNS),
        ("axes_map", ctypes.c_uint8 * INTERPOSER_MAX_AXES)
    ]

    def pack_to_bytes(self):
        # This format string MUST exactly match the order and types in _fields_
        # and the C struct, assuming standard C packing ('=').
        # '=' means standard C types, native byte order, and proper padding.
        # 's' for char array (name) - ctypes.c_char * X is like char[X]
        # 'H' for uint16_t
        # 'B' for uint8_t
        # 'x' can be used for explicit padding if needed, but '=' should handle most.

        # Construct the format string dynamically based on constants
        # This is robust if INTERPOSER_MAX_BTNS or AXES changes.
        f"={CONTROLLER_NAME_MAX_LEN}sHHHHH{INTERPOSER_MAX_BTNS}H{INTERPOSER_MAX_AXES}B"
        
        # Ensure name is bytes and correctly truncated/padded for fixed-size char array
        name_bytes = self.name.encode('utf-8')[:CONTROLLER_NAME_MAX_LEN]
        # Pad with nulls if shorter than CONTROLLER_NAME_MAX_LEN
        name_bytes = name_bytes.ljust(CONTROLLER_NAME_MAX_LEN, b'\0')

        return struct.pack(
            pack_format,
            name_bytes, # Must be bytes
            self.vendor,
            self.product,
            self.version,
            self.num_btns,
            self.num_axes,
            *self.btn_map,  # Unpack the array
            *self.axes_map   # Unpack the array
        )

# Get the size of the C-compatible struct
EXPECTED_C_STRUCT_SIZE = ctypes.sizeof(JsConfigCtypes)
logging.info(f"Expected C js_config_t size (from ctypes): {EXPECTED_C_STRUCT_SIZE} bytes")


ABS_MIN_VAL = -32767
ABS_MAX_VAL = 32767
ABS_TRIGGER_MIN_VAL = 0 # Triggers often 0-255 or 0-1023 for EVDEV
ABS_TRIGGER_MAX_VAL = 255 # Or 1023, or ABS_MAX_VAL depending on driver expectation
ABS_HAT_MIN_VAL = -1
ABS_HAT_MAX_VAL = 1

STANDARD_XPAD_CONFIG = {
    "name": "Microsoft X-Box 360 pad",
    "vendor_id": 0x045e,
    "product_id": 0x028e,
    "version": 0x0114,

    # EVDEV codes. The order here defines our internal abstract button indices.
    # This list is now cleaned up to match a standard controller layout.
    "btn_map": [
        BTN_A,      # Internal abstract button 0
        BTN_B,      # Internal abstract button 1
        BTN_X,      # Internal abstract button 2
        BTN_Y,      # Internal abstract button 3
        BTN_TL,     # Internal abstract button 4 (Left Bumper)
        BTN_TR,     # Internal abstract button 5 (Right Bumper)
        BTN_SELECT, # Internal abstract button 6 (Back)
        BTN_START,  # Internal abstract button 7 (Start)
        BTN_MODE,   # Internal abstract button 8 (Xbox Guide)
        BTN_THUMBL, # Internal abstract button 9 (Left Stick Click)
        BTN_THUMBR, # Internal abstract button 10 (Right Stick Click)
    ],

    # EVDEV codes for axes. The order defines internal abstract axis indices.
    "axes_map": [
        ABS_X,     # Internal abstract axis 0 (Left Stick X)
        ABS_Y,     # Internal abstract axis 1 (Left Stick Y)
        ABS_Z,     # Internal abstract axis 2 (Left Trigger)
        ABS_RX,    # Internal abstract axis 3 (Right Stick X)
        ABS_RY,    # Internal abstract axis 4 (Right Stick Y)
        ABS_RZ,    # Internal abstract axis 5 (Right Trigger)
        ABS_HAT0X, # Internal abstract axis 6 (D-Pad X)
        ABS_HAT0Y  # Internal abstract axis 7 (D-Pad Y)
    ],

    "mapping": {
        # Maps client button numbers to our internal abstract button *indices*.
        # With the cleaned up btn_map, this is now much simpler.
        "btns": { # client_btn_idx -> internal_abstract_btn_idx
            0: 0,  # Client A -> internal index 0 (BTN_A)
            1: 1,  # Client B -> internal index 1 (BTN_B)
            2: 2,  # Client X -> internal index 2 (BTN_X)
            3: 3,  # Client Y -> internal index 3 (BTN_Y)
            4: 4,  # Client LB -> internal index 4 (BTN_TL)
            5: 5,  # Client RB -> internal index 5 (BTN_TR)
            8: 6,  # Client Select/Back -> internal index 6 (BTN_SELECT)
            9: 7,  # Client Start -> internal index 7 (BTN_START)
            10: 9, # Client Left Stick Press -> internal index 9 (BTN_THUMBL)
            11: 10,# Client Right Stick Press -> internal index 10 (BTN_THUMBR)
            16: 8, # Client Xbox/Home -> internal index 8 (BTN_MODE)
        },
        "axes": { # client_axis_idx -> internal_abstract_axis_idx
            0: 0, # Client Left Stick X  -> internal index 0 (ABS_X)
            1: 1, # Client Left Stick Y  -> internal index 1 (ABS_Y)
            2: 3, # Client Right Stick X -> internal index 3 (ABS_RX)
            3: 4, # Client Right Stick Y -> internal index 4 (ABS_RY)
        },
        # Client buttons that map to an internal abstract axis
        "client_btns_to_internal_axes": {
            6: 2, # Client Btn 6 (LT) -> internal axis 2 (ABS_Z)
            7: 5, # Client Btn 7 (RT) -> internal axis 5 (ABS_RZ)
        },
        # Client DPad buttons map to internal abstract HAT axes
        "dpad_to_hat": {
            # client_btn_idx -> (internal_abstract_axis_idx_for_HAT, hat_direction_value)
            12: (7, -1), # Up    -> internal axis 7 (ABS_HAT0Y), value -1
            13: (7, 1),  # Down  -> internal axis 7 (ABS_HAT0Y), value 1
            14: (6, -1), # Left  -> internal axis 6 (ABS_HAT0X), value -1
            15: (6, 1),  # Right -> internal axis 6 (ABS_HAT0X), value 1
        },
        "trigger_internal_abstract_axis_indices": [2, 5],
        "hat_internal_abstract_axis_indices": [6, 7],
    }
}

# --- Event Packing Functions ---
def get_js_event_packed(ev_type, number, value):
    """Packs a js_event struct."""
    # struct js_event { __u32 time; __s16 value; __u8 type; __u8 number; };
    ts_ms = int(time.time() * 1000) & 0xFFFFFFFF # Ensure it fits in u32
    return struct.pack("=IhbB", ts_ms, int(value), ev_type, number)

def get_evdev_events_packed(ev_type, ev_code, ev_value, client_arch_bits):
    """Packs an input_event struct and a SYN_REPORT, using client architecture for timeval."""
    # struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; };
    # struct timeval { time_t tv_sec; suseconds_t tv_usec; };
    # time_t and suseconds_t are 'long' on 32-bit, 'long long' (usually) on 64-bit for tv_sec,
    # and 'long' for tv_usec. The C interposer sends sizeof(unsigned long).
    
    now = time.time()
    ts_sec = int(now)
    ts_usec = int((now - ts_sec) * 1_000_000)

    if client_arch_bits == 64: # Assuming 'long' is 8 bytes for timeval members on 64-bit client
        timeval_fmt = "qq" # tv_sec (long long), tv_usec (long long)
    else: # Assuming 'long' is 4 bytes for timeval members on 32-bit client
        timeval_fmt = "ll" # tv_sec (long), tv_usec (long)
    
    event_fmt = f"={timeval_fmt}HHi" # Native byte order, timeval, type, code, value

    event_data = struct.pack(event_fmt, ts_sec, ts_usec, ev_type, ev_code, int(ev_value))
    syn_event_data = struct.pack(event_fmt, ts_sec, ts_usec, EV_SYN, SYN_REPORT, 0)
    return event_data + syn_event_data

def normalize_axis_value(client_value, is_trigger, is_hat, for_js_event=False):
    """
    Normalizes client axis value.
    If for_js_event is True and is_hat is True, it scales to the full axis range.
    """
    if is_hat:
        hat_val = int(max(ABS_HAT_MIN_VAL, min(ABS_HAT_MAX_VAL, round(client_value))))
        if for_js_event:
            # For JS, D-pad axes need to be full range, not -1/0/1
            return hat_val * ABS_MAX_VAL
        else:
            # For EVDEV, HAT values are -1, 0, or 1
            return hat_val
    if is_trigger: # Client sends 0.0 to 1.0
        # For JS and EVDEV, triggers are often treated as regular axes.
        # Map 0..1 to -32k..+32k for consistency, some drivers map to 0..255.
        # This mapping ensures it works like an analog input.
        return int(ABS_MIN_VAL + client_value * (ABS_MAX_VAL - ABS_MIN_VAL))
    # Regular axis: client sends -1.0 to 1.0
    return int(ABS_MIN_VAL + ((client_value + 1) / 2) * (ABS_MAX_VAL - ABS_MIN_VAL))


class GamepadMapper:
    def __init__(self, config_template, client_input_name, client_num_btns, client_num_axes):
        self.config = config_template
        self.client_input_name = client_input_name

    def get_mapped_events(self, client_event_idx, client_value, is_button_event):
        internal_abstract_idx = -1
        is_trigger_axis = False
        is_hat_axis = False
        target_evdev_type = None
        final_value = 0 # This will be the raw value from the client or dpad direction

        if is_button_event:
            if client_event_idx in self.config["mapping"]["dpad_to_hat"]:
                internal_abstract_idx, hat_direction_value = self.config["mapping"]["dpad_to_hat"][client_event_idx]
                is_hat_axis = True
                target_evdev_type = EV_ABS
                final_value = hat_direction_value * int(client_value)
            elif client_event_idx in self.config["mapping"]["client_btns_to_internal_axes"]:
                internal_abstract_idx = self.config["mapping"]["client_btns_to_internal_axes"][client_event_idx]
                is_trigger_axis = internal_abstract_idx in self.config["mapping"]["trigger_internal_abstract_axis_indices"]
                target_evdev_type = EV_ABS
                final_value = client_value
            else:
                internal_abstract_idx = self.config["mapping"]["btns"].get(client_event_idx)
                target_evdev_type = EV_KEY
                final_value = int(client_value)
        else: # Axis event
            internal_abstract_idx = self.config["mapping"]["axes"].get(client_event_idx)
            is_trigger_axis = internal_abstract_idx in self.config["mapping"]["trigger_internal_abstract_axis_indices"]
            is_hat_axis = internal_abstract_idx in self.config["mapping"]["hat_internal_abstract_axis_indices"]
            target_evdev_type = EV_ABS
            final_value = client_value

        if internal_abstract_idx is None or internal_abstract_idx < 0:
            return None

        # 2. Get EVDEV code and normalized values for both JS and EVDEV
        evdev_code = -1
        js_event_value = 0
        evdev_event_value = 0

        if target_evdev_type == EV_KEY:
            if 0 <= internal_abstract_idx < len(self.config["btn_map"]):
                evdev_code = self.config["btn_map"][internal_abstract_idx]
                js_event_value = evdev_event_value = final_value # 0 or 1
            else: return None
        elif target_evdev_type == EV_ABS:
            if 0 <= internal_abstract_idx < len(self.config["axes_map"]):
                evdev_code = self.config["axes_map"][internal_abstract_idx]
                # Calculate values separately for JS and EVDEV
                js_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=True)
                evdev_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=False)
            else: return None
        else:
            return None

        # 3. Create event data/templates
        if evdev_code != -1:
            js_event_type = JS_EVENT_BUTTON if target_evdev_type == EV_KEY else JS_EVENT_AXIS
            js_event_data = get_js_event_packed(js_event_type, internal_abstract_idx, js_event_value)
            
            evdev_event_template = (target_evdev_type, evdev_code, evdev_event_value)
            
            return {'js_event_data': js_event_data, 'evdev_event_template': evdev_event_template}
        
        return None

class SelkiesGamepad:
    def __init__(self, js_interposer_socket_path, evdev_interposer_socket_path, loop=None):
        self.js_sock_path = js_interposer_socket_path
        self.evdev_sock_path = evdev_interposer_socket_path
        self.loop = loop or asyncio.get_event_loop()
        
        self.mapper = None # Set by set_config
        self.config_payload_cache = None # Cache for js_config_t

        self.js_server = None
        self.evdev_server = None
        self.js_clients = {} # {writer: {'arch_bits': bits}}
        self.evdev_clients = {} # {writer: {'arch_bits': bits}}
        
        self.events_queue = asyncio.Queue()
        self.running = False
        self._event_processor_task = None

    def set_config(self, client_input_name, client_num_btns, client_num_axes):
        self.mapper = GamepadMapper(STANDARD_XPAD_CONFIG, client_input_name, client_num_btns, client_num_axes)
        
        js_idx = 0 
        match = re.search(r"selkies_js(\d+)\.sock$", self.js_sock_path)
        if match:
            js_idx = int(match.group(1))
        else:
            logger_selkies_gamepad.warning(
                f"Failed to parse js_index from {self.js_sock_path}, "
                f"defaulting to 0 for payload name generation if needed."
            )

        payload_controller_config = {
            "name": STANDARD_XPAD_CONFIG.get("name", f"Selkies Virtual JS{js_idx}"),
            "vendor_id": STANDARD_XPAD_CONFIG.get("vendor_id", 0x0000),
            "product_id": STANDARD_XPAD_CONFIG.get("product_id", 0x0000),
            "version": STANDARD_XPAD_CONFIG.get("version_id", 0x0100), 
            "buttons": STANDARD_XPAD_CONFIG.get("btn_map", []), 
            "axes": STANDARD_XPAD_CONFIG.get("axes_map", [])
        }
        
        self.config_payload_cache = self._make_interposer_config_payload(js_idx, payload_controller_config)
        
        logger_selkies_gamepad.info(
            f"Gamepad configured. JS socket: {self.js_sock_path}, EVDEV socket: {self.evdev_sock_path}. "
            f"Using fixed config: {STANDARD_XPAD_CONFIG['name']}"
        )

    def _make_interposer_config_payload(self, js_index: int, controller_config: dict) -> bytes:
        """
        Creates the configuration payload (js_config_t) to be sent to the C interposer.
        Ensures the payload is exactly C_INTERPOSER_STRUCT_SIZE (1360 bytes).
        """
        try:
            name_str = controller_config.get("name", f"Selkies Virtual JS{js_index}")
            name_bytes_utf8 = name_str.encode('utf-8')
            if len(name_bytes_utf8) >= CONTROLLER_NAME_MAX_LEN:
                name_bytes_for_pack = name_bytes_utf8[:CONTROLLER_NAME_MAX_LEN - 1] + b'\0'
            else:
                name_bytes_for_pack = name_bytes_utf8.ljust(CONTROLLER_NAME_MAX_LEN, b'\0')

            if len(name_bytes_for_pack) != CONTROLLER_NAME_MAX_LEN:
                 logging.error(f"CRITICAL: name_bytes_for_pack is not {CONTROLLER_NAME_MAX_LEN} bytes long! Got {len(name_bytes_for_pack)}")
                 return b'\0' * C_INTERPOSER_STRUCT_SIZE

            raw_vendor = controller_config.get("vendor_id")
            if isinstance(raw_vendor, str):
                vendor_id = int(raw_vendor, 16)
            elif isinstance(raw_vendor, int):
                vendor_id = raw_vendor
            else: # Default if key missing or type is wrong
                vendor_id = 0x045e # Default Xbox vendor
            raw_product = controller_config.get("product_id")
            if isinstance(raw_product, str):
                product_id = int(raw_product, 16)
            elif isinstance(raw_product, int):
                product_id = raw_product
            else: # Default
                product_id = 0x028e # Default Xbox product
            raw_version = controller_config.get("version") # Using "version" as the key
            if isinstance(raw_version, str):
                version_id = int(raw_version, 16)
            elif isinstance(raw_version, int):
                version_id = raw_version
            else: # Default
                version_id = 0x0114 # Default Xbox version

            buttons_evdev_codes = controller_config.get("buttons", [])
            axes_evdev_codes = controller_config.get("axes", [])

            num_actual_btns = len(buttons_evdev_codes)
            num_actual_axes = len(axes_evdev_codes)

            padded_btn_map_for_pack = list(buttons_evdev_codes)
            if len(padded_btn_map_for_pack) > INTERPOSER_MAX_BTNS:
                logging.warning(f"Controller '{name_str}' has {len(padded_btn_map_for_pack)} buttons, truncating to {INTERPOSER_MAX_BTNS} for config.")
                padded_btn_map_for_pack = padded_btn_map_for_pack[:INTERPOSER_MAX_BTNS]
                # num_actual_btns is already set correctly to the original length before potential truncation for the array
            else:
                padded_btn_map_for_pack.extend([0] * (INTERPOSER_MAX_BTNS - len(padded_btn_map_for_pack)))

            padded_axes_map_for_pack = list(axes_evdev_codes)
            if len(padded_axes_map_for_pack) > INTERPOSER_MAX_AXES:
                logging.warning(f"Controller '{name_str}' has {len(padded_axes_map_for_pack)} axes, truncating to {INTERPOSER_MAX_AXES} for config.")
                padded_axes_map_for_pack = padded_axes_map_for_pack[:INTERPOSER_MAX_AXES]
                # num_actual_axes is already set
            else:
                padded_axes_map_for_pack.extend([0] * (INTERPOSER_MAX_AXES - len(padded_axes_map_for_pack)))

            # Base format string for the actual data fields
            base_struct_fmt = f"={CONTROLLER_NAME_MAX_LEN}sxHHHHH{INTERPOSER_MAX_BTNS}H{INTERPOSER_MAX_AXES}B"
            
            # Calculate size of the base structure without any explicit end padding
            size_without_explicit_end_padding = struct.calcsize(base_struct_fmt) # Should be 1353

            # Calculate how much padding is needed to reach the C struct's total size
            padding_needed = C_INTERPOSER_STRUCT_SIZE - size_without_explicit_end_padding

            if padding_needed < 0:
                logging.error(
                    f"CRITICAL STRUCT SIZE ERROR: Python base packed size ({size_without_explicit_end_padding}) "
                    f"is larger than C interposer expected size ({C_INTERPOSER_STRUCT_SIZE}). "
                    f"This means constants (MAX_BTNS, MAX_AXES, NAME_LEN) or field types/order "
                    f"differ between Python 'base_struct_fmt' and C 'js_config_t'."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            # Final format string including the calculated padding at the end
            struct_fmt = f"{base_struct_fmt}{padding_needed}x"
            
            # Verify the final Python packed size matches the C expectation
            python_final_packed_size = struct.calcsize(struct_fmt)
            if python_final_packed_size != C_INTERPOSER_STRUCT_SIZE:
                # This should ideally not be hit if padding_needed was calculated correctly
                logging.error(
                    f"CRITICAL FINAL PYTHON PACKED SIZE MISMATCH for js_config_t! "
                    f"C interposer expects: {C_INTERPOSER_STRUCT_SIZE}, "
                    f"Python struct.pack calculated final size: {python_final_packed_size} using format '{struct_fmt}'. "
                    f"This indicates an issue with padding calculation logic or the base_struct_fmt."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            logging.debug(f"Using final struct_fmt: '{struct_fmt}' for js_config, packing to size {python_final_packed_size}")

            payload_args = [
                name_bytes_for_pack,    # char name[CONTROLLER_NAME_MAX_LEN]
                vendor_id,              # uint16_t vendor
                product_id,             # uint16_t product
                version_id,             # uint16_t version
                num_actual_btns,        # uint16_t num_btns (actual count)
                num_actual_axes,        # uint16_t num_axes (actual count)
            ]
            # Add elements of the padded button map array
            payload_args.extend(padded_btn_map_for_pack) # uint16_t btn_map[INTERPOSER_MAX_BTNS]
            # Add elements of the padded axes map array
            payload_args.extend(padded_axes_map_for_pack)  # uint8_t axes_map[INTERPOSER_MAX_AXES]
            # The 'x' padding specifier in struct_fmt does not take arguments in payload_args

            payload = struct.pack(struct_fmt, *payload_args)

            log_display_name = name_bytes_for_pack.split(b'\0',1)[0].decode('utf-8', errors='replace')
            logging.info(f"Packed js_config payload for '{name_str}' (js{js_index}): "
                         f"len={len(payload)} bytes. "
                         f"Name='{log_display_name}', "
                         f"Vendor=0x{vendor_id:04x}, Product=0x{product_id:04x}, Version=0x{version_id:04x}, "
                         f"Reported Buttons={num_actual_btns} (Array capacity: {INTERPOSER_MAX_BTNS}), "
                         f"Reported Axes={num_actual_axes} (Array capacity: {INTERPOSER_MAX_AXES})")
            
            if len(payload) != C_INTERPOSER_STRUCT_SIZE:
                logging.error(f"FINAL PAYLOAD SIZE MISMATCH AFTER PACKING! Expected {C_INTERPOSER_STRUCT_SIZE}, got {len(payload)}. This is very bad.")
                return b'\0' * C_INTERPOSER_STRUCT_SIZE
            return payload

        except struct.error as e:
            # Ensure struct_fmt is defined for the error message if an error occurs before its assignment
            current_struct_fmt = struct_fmt if 'struct_fmt' in locals() else (base_struct_fmt if 'base_struct_fmt' in locals() else "undefined")
            logging.error(f"Error packing joystick config for js{js_index} with format '{current_struct_fmt}': {e}")
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.error(f"Controller config was: {config_to_log}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE
        except Exception as e:
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.exception(f"Unexpected error creating interposer config payload for js{js_index} with config {config_to_log}: {e}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE

    async def _handle_interposer_client(self, reader, writer, is_evdev_socket):
        peername = writer.get_extra_info('peername') 
        socket_type_str = "EVDEV" if is_evdev_socket else "JS"
        clients_dict = self.evdev_clients if is_evdev_socket else self.js_clients
        sock_path = self.evdev_sock_path if is_evdev_socket else self.js_sock_path
        log_prefix = f"Gamepad {sock_path} Client {peername} ({socket_type_str}):"
        logger_selkies_gamepad.info(f"{log_prefix} Handler started.")

        try:
            # 1. Send config payload
            if not self.config_payload_cache:
                logger_selkies_gamepad.error(f"{log_prefix} Config payload not ready. Aborting handler.")
                return
            logger_selkies_gamepad.info(f"{log_prefix} Preparing to send config payload. Length: {len(self.config_payload_cache)}, Expected C size: {EXPECTED_C_STRUCT_SIZE}, First 16 bytes: {self.config_payload_cache[:16].hex()}")
            writer.write(self.config_payload_cache)
            await writer.drain()
            await asyncio.sleep(1)
            logger_selkies_gamepad.debug(f"{log_prefix} Sent config payload.")

            # 2. Read 1-byte architecture specifier
            arch_byte = await reader.readexactly(1)
            client_sizeof_long = struct.unpack("=B", arch_byte)[0]
            client_arch_bits = client_sizeof_long * 8
            logger_selkies_gamepad.info(f"{log_prefix} Received arch specifier: {client_sizeof_long} bytes ({client_arch_bits}-bit).")
            
            clients_dict[writer] = {'arch_bits': client_arch_bits}
            logger_selkies_gamepad.info(f"{log_prefix} Added to active list. Total {socket_type_str} clients: {len(clients_dict)}.")

            # Keep connection alive
            while self.running and not writer.is_closing():
                await asyncio.sleep(0.1) 
            
            if not self.running:
                logger_selkies_gamepad.info(f"{log_prefix} Exiting handler normally because self.running is False.")
            if writer.is_closing():
                logger_selkies_gamepad.info(f"{log_prefix} Exiting handler normally because writer.is_closing() is True (client likely closed connection).")

        except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError) as e:
            logger_selkies_gamepad.info(f"{log_prefix} Disconnected (expected error): {type(e).__name__} - {e}")
        except Exception as e:
            logger_selkies_gamepad.error(f"{log_prefix} Unhandled error in handler: {e}", exc_info=True)
        finally:
            logger_selkies_gamepad.info(f"{log_prefix} Entering finally block.")
            if writer in clients_dict:
                del clients_dict[writer]
                logger_selkies_gamepad.info(f"{log_prefix} Removed from active list. Total {socket_type_str} clients now: {len(clients_dict)}.")
            else:
                logger_selkies_gamepad.warning(f"{log_prefix} Writer not found in active list during finally block.")

            if not writer.is_closing():
                logger_selkies_gamepad.info(f"{log_prefix} Explicitly closing writer in finally block.")
                writer.close()
                try: await writer.wait_closed() 
                except AttributeError: pass # wait_closed might not exist on all stream types or states
            logger_selkies_gamepad.info(f"{log_prefix} Handler finished.")

    async def _run_single_server(self, interposer_socket_path, is_evdev_socket):
        sock_dir = os.path.dirname(interposer_socket_path)
        if sock_dir and not os.path.exists(sock_dir):
            try: os.makedirs(sock_dir, exist_ok=True)
            except OSError as e:
                logger_selkies_gamepad.error(f"Failed to create directory {sock_dir} for socket: {e}")
                return None
        
        if os.path.exists(interposer_socket_path):
            try:
                os.unlink(interposer_socket_path)
                logger_selkies_gamepad.debug(f"Removed existing socket file: {interposer_socket_path}")
            except OSError as e:
                logger_selkies_gamepad.warning(f"Could not remove existing file at {interposer_socket_path}: {e}. Bind might fail.")

        try:
            server = await asyncio.start_unix_server(
                lambda r, w: self._handle_interposer_client(r, w, is_evdev_socket),
                path=interposer_socket_path
            )
            addr = server.sockets[0].getsockname() if server.sockets else interposer_socket_path
            logger_selkies_gamepad.info(f"{'EVDEV' if is_evdev_socket else 'JS'} interposer server listening on {addr}")
            return server
        except Exception as e:
            logger_selkies_gamepad.error(f"Failed to start {'EVDEV' if is_evdev_socket else 'JS'} server on {interposer_socket_path}: {e}", exc_info=True)
            return None

    async def run_servers(self):
        if not self.mapper:
            logger_selkies_gamepad.error("Mapper not set. Call set_config() before run_servers().")
            return

        self.running = True
        if self._event_processor_task is None or self._event_processor_task.done():
            self._event_processor_task = asyncio.create_task(self._process_event_queue())

        self.js_server = await self._run_single_server(self.js_sock_path, is_evdev_socket=False)
        self.evdev_server = await self._run_single_server(self.evdev_sock_path, is_evdev_socket=True)

        if not self.js_server and not self.evdev_server:
            logger_selkies_gamepad.error("Neither JS nor EVDEV interposer server could be started. Stopping.")
            self.running = False
            if self._event_processor_task and not self._event_processor_task.done():
                self._event_processor_task.cancel()
            return
        
        while self.running:
            await asyncio.sleep(1)
        logger_selkies_gamepad.info("run_servers loop exited.")

    def send_event(self, client_event_idx, client_value, is_button_event):
        if not self.mapper or not self.running:
            return
        event_package = self.mapper.get_mapped_events(client_event_idx, client_value, is_button_event)
        if event_package:
            logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Queuing event: {event_package}")
            self.events_queue.put_nowait(event_package)

    async def _process_event_queue(self):
        logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor started.")
        while self.running:
            try:
                event_package = await self.events_queue.get()
                if event_package is None: # Sentinel for shutdown
                    self.events_queue.task_done()
                    break
                
                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Dequeued event: {event_package}")
                
                js_data = event_package.get('js_event_data')
                evdev_template = event_package.get('evdev_event_template') 

                # Send to JS clients
                if js_data:
                    for i, (writer, client_info) in enumerate(list(self.js_clients.items())):
                        if not writer.is_closing():
                            try:
                                writer.write(js_data)
                                await writer.drain()
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: JS event drained to client #{i}.")
                            except (ConnectionResetError, BrokenPipeError): pass 
                            except Exception as e: 
                                logger_selkies_gamepad.error(f"Error sending to JS client #{i}: {e}", exc_info=True) 
                
                # Send to EVDEV clients
                if evdev_template:
                    ev_type, ev_code, ev_value = evdev_template
                    for i, (writer, client_info) in enumerate(list(self.evdev_clients.items())):
                        if not writer.is_closing():
                            try:
                                client_arch_bits = client_info.get('arch_bits', 64) 
                                evdev_data = get_evdev_events_packed(ev_type, ev_code, ev_value, client_arch_bits)
                                writer.write(evdev_data)
                                await writer.drain()
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: EVDEV event drained to client #{i}.")
                            except (ConnectionResetError, BrokenPipeError): pass 
                            except Exception as e: 
                                logger_selkies_gamepad.error(f"Error sending to EVDEV client #{i}: {e}", exc_info=True)
                
                self.events_queue.task_done()
            except asyncio.CancelledError:
                logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor task cancelled.")
                break
            except Exception as e:
                logger_selkies_gamepad.error(f"Gamepad {self.js_sock_path}: Unhandled error in event processor: {e}", exc_info=True)
        logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor stopped.")


    async def close(self):
        logger_selkies_gamepad.info(f"Closing gamepad services for JS:{self.js_sock_path}, EVDEV:{self.evdev_sock_path}")
        self.running = False

        if self.js_server:
            self.js_server.close()
            try: await self.js_server.wait_closed()
            except AttributeError: pass
            self.js_server = None
            logger_selkies_gamepad.info(f"JS interposer server {self.js_sock_path} closed.")
        if self.evdev_server:
            self.evdev_server.close()
            try: await self.evdev_server.wait_closed()
            except AttributeError: pass
            self.evdev_server = None
            logger_selkies_gamepad.info(f"EVDEV interposer server {self.evdev_sock_path} closed.")

        for writer in list(self.js_clients.keys()):
            if not writer.is_closing(): writer.close()
        self.js_clients.clear()
        for writer in list(self.evdev_clients.keys()):
            if not writer.is_closing(): writer.close()
        self.evdev_clients.clear()
        
        if self._event_processor_task and not self._event_processor_task.done():
            try:
                self.events_queue.put_nowait(None) 
                await asyncio.wait_for(self._event_processor_task, timeout=2.0)
            except asyncio.TimeoutError:
                logger_selkies_gamepad.warning("Event processor task timed out on close, cancelling.")
                self._event_processor_task.cancel()
            except asyncio.CancelledError:
                pass 
            except Exception as e:
                logger_selkies_gamepad.error(f"Exception stopping event processor: {e}")
        self._event_processor_task = None
        
        for sock_path in [self.js_sock_path, self.evdev_sock_path]:
            if sock_path and os.path.exists(sock_path):
                try:
                    os.unlink(sock_path)
                    logger_selkies_gamepad.info(f"Removed socket file: {sock_path}")
                except OSError as e:
                    logger_selkies_gamepad.warning(f"Could not remove socket file {sock_path} on close: {e}")
        
        logger_selkies_gamepad.info(f"Gamepad services fully closed.")


# --- WebRTCInput Class ---
class WebRTCInputError(Exception): pass

class WebRTCInput:
    def __init__(
        self,
        gst_webrtc_app,
        uinput_mouse_socket_path="",
        js_socket_path_prefix="/tmp", 
        enable_clipboard="",
        enable_binary_clipboard="",
        enable_cursors=True,
        cursor_size=16, 
        cursor_scale=1.0,
        cursor_debug=False,
        max_cursor_size=32,
        data_server_instance=None,
        upload_dir=None,
        is_wayland=False,
        wayland_socket_index=0,
    ):
        self.wayland_socket_index = wayland_socket_index
        self.active_shortcut_modifiers = set()
        self.SHORTCUT_MODIFIER_XKEY_NAMES = {
            'Control_L', 'Control_R', 
            'Alt_L', 'Alt_R', 
            'Super_L', 'Super_R',
            'Meta_L', 'Meta_R'
        }
        self.active_modifiers = set()
        self.atomically_typed_keys = set()
        self.MODIFIER_KEYSYMS = {
            65505, 65506,  # Shift_L, Shift_R
            65507, 65508,  # Control_L, Control_R
            65513, 65514,  # Alt_L, Alt_R
            65027,        # ISO_Level3_Shift (AltGr)
            65511, 65512,  # Meta_L, Meta_R / Super_L, Super_R
        }
        self.gst_webrtc_app = gst_webrtc_app
        self.loop = asyncio.get_event_loop()
        self.js_socket_path_prefix = js_socket_path_prefix
        self.num_gamepads = 4 
        self.gamepad_instances = {}
        self.client_gamepad_associations = {} 

        self.clipboard_running = False
        self.uinput_mouse_socket_path = uinput_mouse_socket_path
        self.uinput_mouse_socket = None
        self.enable_clipboard = enable_clipboard
        self.enable_binary_clipboard = enable_binary_clipboard
        self.enable_cursors = enable_cursors
        self.cursors_running = False
        self.cursor_scale = cursor_scale
        self.cursor_size = cursor_size
        self.cursor_debug = cursor_debug
        self.max_cursor_size = max_cursor_size
        self.system_dpi = 96.0
        self.cursor_size_cap = max_cursor_size
        self.keyboard = None
        self.mouse = None
        self.xdisplay = None
        self.button_mask = 0
        self.last_x = -1
        self.last_y = -1
        self.ping_start = None

        self.upload_dir = upload_dir
        self.upload_dir_path = None
        self.active_uploads_by_path_conn = {}
        self.active_upload_target_path_conn = None
        
        self.on_video_encoder_bit_rate = lambda bitrate: logger_webrtc_input.warning("unhandled on_video_encoder_bit_rate")
        self.on_audio_encoder_bit_rate = lambda bitrate: logger_webrtc_input.warning("unhandled on_audio_encoder_bit_rate")
        self.on_mouse_pointer_visible = lambda visible: logger_webrtc_input.warning("unhandled on_mouse_pointer_visible")
        self.on_clipboard_read = self._on_clipboard_read
        self.on_set_fps = lambda fps: logger_webrtc_input.warning("unhandled on_set_fps")
        self.on_set_enable_resize = lambda enable_resize, res: logger_webrtc_input.warning("unhandled on_set_enable_resize")
        self.on_client_fps = lambda fps: logger_webrtc_input.warning("unhandled on_client_fps")
        self.on_client_latency = lambda latency: logger_webrtc_input.warning("unhandled on_client_latency")
        self.on_resize = lambda res: logger_webrtc_input.warning("unhandled on_resize")
        self.on_scaling_ratio = lambda res: logger_webrtc_input.warning("unhandled on_scaling_ratio")
        self.on_ping_response = lambda latency: logger_webrtc_input.warning("unhandled on_ping_response")
        self.on_cursor_change = self._on_cursor_change
        self.on_client_webrtc_stats = lambda webrtc_stat_type, webrtc_stats: logger_webrtc_input.warning("unhandled on_client_webrtc_stats")
        self.clipboard_monitor_task = None
        self.multipart_clipboard_buffer = None
        self.multipart_clipboard_mime_type = "text/plain"
        self.multipart_clipboard_total_size = 0
        self.multipart_clipboard_in_progress = False
        self.data_server_instance = data_server_instance
        self.on_update_settings = lambda settings_json: logger_webrtc_input.warning("unhandled update_settings")
        self.is_wayland = is_wayland
        self.wayland_input = None
        self.wayland_scancode_map = {}
        if self.is_wayland:
            try:
                from pixelflux import ScreenCapture
                self.wayland_input = ScreenCapture()
                logger_webrtc_input.info("Wayland input injection initialized.")
                
                if xkb:
                    try:
                        self.xkb_ctx = xkb.Context()
                        self.xkb_keymap = self.xkb_ctx.keymap_new_from_names()
                        self._build_wayland_keymap()
                        logger_webrtc_input.info(f"Built Wayland scancode map with {len(self.wayland_scancode_map)} keys.")
                    except Exception as e:
                        logger_webrtc_input.error(f"Failed to build xkb keymap: {e}")
                else:
                    logger_webrtc_input.warning("xkbcommon not found. Keyboard input on Wayland may fail.")

            except Exception as e:
                logger_webrtc_input.error(f"Failed to initialize Wayland input: {e}")

    def _build_wayland_keymap(self):
        """Builds a reverse mapping from Keysyms to Scancodes using xkbcommon."""
        if not self.xkb_keymap:
            return
        
        try:
            self.xkb_keymap = self.xkb_ctx.keymap_new_from_names(
                rules="evdev", model="pc105", layout="us", variant="", options=""
            )
        except Exception as e:
            logger_webrtc_input.warning(f"Could not force 'us' layout, using default: {e}")

        min_kc = self.xkb_keymap.min_keycode()
        max_kc = self.xkb_keymap.max_keycode()
        
        for kc in range(min_kc, max_kc + 1):
            for level in range(4):
                syms = self.xkb_keymap.key_get_syms_by_level(kc, 0, level)
                if syms:
                    for sym in syms:
                        if sym not in self.wayland_scancode_map:
                            self.wayland_scancode_map[sym] = kc

    async def _on_clipboard_read(self, data, mime_type="text/plain"):
        await self.send_clipboard_data(data, mime_type)
    def _on_cursor_change(self, data): self.send_cursor_data(data)
    async def send_clipboard_data(self, data, mime_type="text/plain"):
        if self.gst_webrtc_app.mode != "websockets":
            self.gst_webrtc_app.send_clipboard_data(data, mime_type)
            return
        try:
            is_text = mime_type == "text/plain"
            data_bytes = data.encode('utf-8') if is_text and isinstance(data, str) else data
            total_size = len(data_bytes)
            if total_size < CLIPBOARD_CHUNK_SIZE:
                encoded_data = base64.b64encode(data_bytes).decode('ascii')
                if is_text:
                    message = f"clipboard,{encoded_data}"
                else:
                    message = f"clipboard_binary,{mime_type},{encoded_data}"
                self.gst_webrtc_app.send_ws_message(message)
            else:
                logger_webrtc_input.info(f"Sending large clipboard data ({mime_type}, {total_size} bytes) in multiple parts.")
                start_message = f"clipboard_start,{mime_type},{total_size}"
                self.gst_webrtc_app.send_ws_message(start_message)
                offset = 0
                while offset < total_size:
                    chunk = data_bytes[offset:offset + CLIPBOARD_CHUNK_SIZE]
                    encoded_chunk = base64.b64encode(chunk).decode('ascii')
                    data_message = f"clipboard_data,{encoded_chunk}"
                    self.gst_webrtc_app.send_ws_message(data_message)
                    offset += len(chunk)
                    await asyncio.sleep(0)
                self.gst_webrtc_app.send_ws_message("clipboard_finish")
                logger_webrtc_input.info("Finished sending multi-part clipboard data.")
        except Exception as e:
            logger_webrtc_input.error(f"Failed to send clipboard data: {e}", exc_info=True)
    def send_cursor_data(self, data):
        if self.gst_webrtc_app.mode == "websockets": self.gst_webrtc_app.send_ws_cursor_data(data)
        else: self.gst_webrtc_app.send_cursor_data(data)

    def __keyboard_connect(self): self.keyboard = pynput.keyboard.Controller()
    def __mouse_connect(self):
        if self.uinput_mouse_socket_path:
            logger_webrtc_input.info(f"Connecting to uinput mouse socket: {self.uinput_mouse_socket_path}")
            self.uinput_mouse_socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        if not self.is_wayland and pynput:
            self.mouse = pynput.mouse.Controller()
    def __mouse_disconnect(self):
        if self.mouse: del self.mouse; self.mouse = None
    def __mouse_emit(self, *args, **kwargs):
        if self.uinput_mouse_socket_path:
            cmd = {"args": args, "kwargs": kwargs}
            data = msgpack.packb(cmd, use_bin_type=True)
            self.uinput_mouse_socket.sendto(data, self.uinput_mouse_socket_path)

    async def __gamepad_connect(self, gamepad_idx, client_name, client_num_btns, client_num_axes):
        if not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client association: Gamepad index {gamepad_idx} out of range (0-{self.num_gamepads-1}).")
            return

        if gamepad_idx not in self.gamepad_instances:
            logger_webrtc_input.error(
                f"Client association: No persistent gamepad instance found for index {gamepad_idx}. "
                f"This should not happen if _initialize_persistent_gamepads ran correctly."
            )
            return

        # Log the association
        logger_webrtc_input.info(
            f"Client controller '{client_name}' ({client_num_btns}b, {client_num_axes}a) "
            f"is now associated with persistent virtual gamepad slot {gamepad_idx}."
        )
        
        self.client_gamepad_associations[gamepad_idx] = {
            "client_name": client_name,
            "client_num_btns": client_num_btns,
            "client_num_axes": client_num_axes,
            "association_time": time.time()
        }

    async def __gamepad_disconnect(self, gamepad_idx=None):
        if gamepad_idx is None: # Disassociate all if no specific index
            indices_to_disassociate = list(self.client_gamepad_associations.keys())
            logger_webrtc_input.info("Disassociating all client gamepads from persistent slots.")
        elif not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client disassociation: Gamepad index {gamepad_idx} out of range.")
            return
        else:
            indices_to_disassociate = [gamepad_idx]

        for idx in indices_to_disassociate:
            if idx in self.client_gamepad_associations:
                associated_info = self.client_gamepad_associations.pop(idx)
                logger_webrtc_input.info(
                    f"Client controller '{associated_info.get('client_name', 'Unknown')}' "
                    f"disassociated from persistent virtual gamepad slot {idx}."
                )
            elif gamepad_idx is not None: # Only log if a specific, non-associated index was requested
                 logger_webrtc_input.warning(
                    f"Client disassociation: No active client association found for gamepad slot {idx} to disassociate."
                )

    def __gamepad_emit_btn(self, gamepad_idx, client_btn_num, client_btn_val):
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_btn_num, client_btn_val, is_button_event=True)

    def __gamepad_emit_axis(self, gamepad_idx, client_axis_num, client_axis_val):
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_axis_num, client_axis_val, is_button_event=False)
            
    async def connect(self):
        if not self.is_wayland and X11_LIBS_AVAILABLE:
            try: self.xdisplay = display.Display()
            except Exception as e: logger_webrtc_input.error(f"Failed to connect to X display: {e}"); self.xdisplay = None
        if self.xdisplay:
            try:
                screen = self.xdisplay.screen()
                width_mm = screen.width_in_mms
                height_mm = screen.height_in_mms
                if width_mm > 0 and height_mm > 0:
                    dpi_x = (screen.width_in_pixels * 25.4) / width_mm
                    dpi_y = (screen.height_in_pixels * 25.4) / height_mm
                    self.system_dpi = (dpi_x + dpi_y) / 2.0
                dpi_scale_factor = self.system_dpi / 96.0
                self.cursor_size_cap = int(self.max_cursor_size * dpi_scale_factor)
                logger_webrtc_input.info(
                    f"System DPI detected as ~{self.system_dpi:.0f}. "
                    f"Cursor size cap set to {self.cursor_size_cap}x{self.cursor_size_cap}px."
                )
            except Exception as e:
                logger_webrtc_input.warning(f"Could not determine system DPI, using default 96. Error: {e}")
        if not self.is_wayland and X11_LIBS_AVAILABLE:
            self.__keyboard_connect()
        if self.xdisplay: await self.reset_keyboard()
        self.__mouse_connect()
        
        # Initialize persistent gamepad instances
        await self._initialize_persistent_gamepads()

    async def _initialize_persistent_gamepads(self):
        logger_webrtc_input.info(f"Initializing {self.num_gamepads} persistent gamepad instances...")
        if not os.path.exists(self.js_socket_path_prefix):
            try:
                os.makedirs(self.js_socket_path_prefix, exist_ok=True)
                logger_webrtc_input.info(f"Created directory for gamepad sockets: {self.js_socket_path_prefix}")
            except OSError as e:
                logger_webrtc_input.error(f"Failed to create directory {self.js_socket_path_prefix} for gamepad sockets: {e}")
                return # Cannot proceed if directory creation fails

        for i in range(self.num_gamepads):
            if i in self.gamepad_instances: # Should not happen on initial call but good for robustness
                logger_webrtc_input.warning(f"Gamepad instance for index {i} already exists. Skipping re-initialization.")
                continue

            js_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_js{i}.sock")
            evdev_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_event{1000+i}.sock") 
            
            gamepad = SelkiesGamepad(js_ip_sock_path, evdev_ip_sock_path, self.loop)
            
            # Use standardized name and capabilities from STANDARD_XPAD_CONFIG
            gamepad_name_for_interposer = STANDARD_XPAD_CONFIG.get("name", f"Selkies Virtual Gamepad {i}")
            std_num_btns = len(STANDARD_XPAD_CONFIG["btn_map"])
            std_num_axes = len(STANDARD_XPAD_CONFIG["axes_map"])
            
            # Pass the standardized name to set_config.
            gamepad.set_config(gamepad_name_for_interposer, std_num_btns, std_num_axes)
            
            asyncio.create_task(gamepad.run_servers()) 
            self.gamepad_instances[i] = gamepad # Store by index i
            logger_webrtc_input.info(f"Initialized and started persistent gamepad instance for index {i} (Name: '{gamepad_name_for_interposer}', JS: {js_ip_sock_path}, EVDEV: {evdev_ip_sock_path}).")

    async def disconnect(self):
        logger_webrtc_input.info("Closing all pre-allocated gamepad instances...")
        gamepad_indices_to_close = list(self.gamepad_instances.keys()) # Iterate over a copy of keys
        for gamepad_idx in gamepad_indices_to_close:
            gamepad = self.gamepad_instances.pop(gamepad_idx, None)
            if gamepad:
                logger_webrtc_input.info(f"Closing gamepad instance for index {gamepad_idx} (JS: {gamepad.js_sock_path}).")
                await gamepad.close()
        self.__mouse_disconnect()
        if self.xdisplay: self.xdisplay = None

    async def reset_keyboard(self):
        if self.is_wayland:
            if self.wayland_input:
                # Release common modifiers
                modifiers = [65507, 65505, 65513, 65508, 65506, 65027, 65511, 65512] # Ctrl, Shift, Alt, Meta
                for k in modifiers:
                    scancode = self.wayland_scancode_map.get(k)
                    if scancode:
                        try: self.wayland_input.inject_key(scancode, 0)
                        except: pass
            return

        if not self.keyboard or not self.xdisplay : 
            logger_webrtc_input.warning("Cannot reset keyboard, X display or keyboard controller not available.")
            return
        logger_webrtc_input.info("Resetting keyboard modifiers.")
        lctrl, lshift, lalt, rctrl, rshift, ralt = 65507, 65505, 65513, 65508, 65506, 65027
        lmeta, rmeta, keyf, keyF, keym, keyM, escape = 65511, 65512, 102, 70, 109, 77, 65307
        for k in [lctrl, lshift, lalt, rctrl, rshift, ralt, lmeta, rmeta, keyf, keyF, keym, keyM, escape]:
            try: await self.send_x11_keypress(k, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error resetting key {k}: {e}")
    
    def send_mouse(self, action, data):
        if action == MOUSE_POSITION:
            if self.mouse: self.mouse.position = data
        elif action == MOUSE_MOVE:
            x, y = data
            if self.uinput_mouse_socket_path:
                self.__mouse_emit(UINPUT_REL_X, x, syn=False)
                self.__mouse_emit(UINPUT_REL_Y, y)
            elif self.xdisplay:
                xtest.fake_input(self.xdisplay, Xlib.X.MotionNotify, detail=True, root=Xlib.X.NONE, x=x, y=y)
                self.xdisplay.sync()
        elif action == MOUSE_SCROLL_UP:
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, 1)
            elif self.mouse: self.mouse.scroll(0, -1)
        elif action == MOUSE_SCROLL_DOWN:
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, -1)
            elif self.mouse: self.mouse.scroll(0, 1)
        elif action == MOUSE_SCROLL_LEFT:
            if self.mouse: self.mouse.scroll(-1, 0)
        elif action == MOUSE_SCROLL_RIGHT:
            if self.mouse: self.mouse.scroll(1, 0)
        elif action == MOUSE_BUTTON: 
            btn_map_key = "uinput" if self.uinput_mouse_socket_path else "pynput"
            btn_uinput_or_pynput = MOUSE_BUTTON_MAP[data[1]][btn_map_key]
            if data[0] == MOUSE_BUTTON_PRESS: 
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_pynput, 1)
                elif self.mouse: self.mouse.press(btn_uinput_or_pynput)
            else: 
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_pynput, 0)
                elif self.mouse: self.mouse.release(btn_uinput_or_pynput)

    async def send_x11_keypress(self, keysym, down=True):
        if self.is_wayland and self.wayland_input:
            scancode = self.wayland_scancode_map.get(keysym)
            if scancode is None and (0x20 <= keysym <= 0xFF):
                try:
                    lower_sym = ord(chr(keysym).lower())
                    scancode = self.wayland_scancode_map.get(lower_sym)
                except: pass

            if scancode:
                try:
                    self.wayland_input.inject_key(scancode, 1 if down else 0)
                except Exception as e:
                    logger_webrtc_input.warning(f"Failed to inject Wayland key: {e}")
            else:
                await self._xdotool_fallback(keysym, down)
            return

        is_printable = (0x20 <= keysym <= 0xFF) or ((keysym & 0xFF000000) == 0x01000000)
        action = "keydown" if down else "keyup"
        command = None
        use_pynput_for_printable = False
        if is_printable:
            unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
            try:
                char = chr(unicode_codepoint)
                if char.isalpha():
                    use_pynput_for_printable = True
                else:
                    xdotool_arg = f"U{unicode_codepoint:04X}"
                    if not self.active_shortcut_modifiers:
                        command = ["xdotool", action, "--clearmodifiers", xdotool_arg]
                    else:
                        command = ["xdotool", action, xdotool_arg]
            except ValueError:
                use_pynput_for_printable = True

        else:
            map_entry = X11_KEYSYM_MAP.get(keysym)
            if map_entry:
                xdotool_arg = map_entry.get('xkey_name')
                if xdotool_arg:
                    command = ["xdotool", action, xdotool_arg]
                    if xdotool_arg in self.SHORTCUT_MODIFIER_XKEY_NAMES:
                        if down:
                            self.active_shortcut_modifiers.add(xdotool_arg)
                        else:
                            self.active_shortcut_modifiers.discard(xdotool_arg)

        if command:
            try:
                process = await subprocess.create_subprocess_exec(
                    *command, stdout=subprocess.PIPE, stderr=subprocess.PIPE
                )
                await asyncio.wait_for(process.communicate(), timeout=0.5)
                if process.returncode == 0:
                    return
            except Exception:
                pass

        if use_pynput_for_printable or not command:
            try:
                if not self.keyboard:
                    await self._xdotool_fallback(keysym, down)
                    return
                
                pynput_key = pynput.keyboard.KeyCode.from_vk(keysym)
                if down:
                    self.keyboard.press(pynput_key)
                else:
                    self.keyboard.release(pynput_key)
            except Exception:
                await self._xdotool_fallback(keysym, down)

    async def _xdotool_fallback(self, keysym_number, down=True):
        if self.is_wayland:
            if not down:
                return
            char_to_type = None
            if (keysym_number & 0xFF000000) == 0x01000000:
                unicode_codepoint = keysym_number & 0x00FFFFFF
                if 0 <= unicode_codepoint <= 0x10FFFF:
                    try:
                        char_to_type = chr(unicode_codepoint)
                    except ValueError:
                        pass
            else:
                keysym_name = None
                if XK is not None:
                    try:
                        keysym_name = XK.keysym_to_string(keysym_number)
                    except Exception:
                        pass
                
                if keysym_name is None:
                    if 0x20 <= keysym_number <= 0x7E or keysym_number >= 0xA0:
                        try:
                            char_to_type = chr(keysym_number)
                        except ValueError:
                            pass
                else:
                    if len(keysym_name) == 1:
                        char_to_type = keysym_name
                    elif keysym_number == 0x00a3:
                        char_to_type = "£"
            if char_to_type:
                try:
                    command_wtype = ["wtype", char_to_type]
                    process_wtype = await subprocess.create_subprocess_exec(
                        *command_wtype,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env=self._get_wl_env()
                    )
                    await asyncio.wait_for(process_wtype.communicate(), timeout=1.0)
                except Exception as e:
                    logger_webrtc_input.warning(f"wtype fallback failed: {e}")
            
            return

        if not self.xdisplay:
            return

        xdotool_key_arg = None
        char_for_type_cmd_fallback = None

        if (keysym_number & 0xFF000000) == 0x01000000:
            unicode_codepoint = keysym_number & 0x00FFFFFF
            if 0 <= unicode_codepoint <= 0x10FFFF:
                xdotool_key_arg = f"U{unicode_codepoint:04X}"
                try:
                    char_for_type_cmd_fallback = chr(unicode_codepoint)
                except ValueError:
                    pass
            else:
                return
        else:
            keysym_name_from_xlib = XK.keysym_to_string(keysym_number)

            if keysym_name_from_xlib is None:
                if 0x20 <= keysym_number <= 0x7E or keysym_number >= 0xA0:
                    try:
                        keysym_name_from_xlib = chr(keysym_number)
                        char_for_type_cmd_fallback = keysym_name_from_xlib
                    except ValueError:
                        return
                else:
                    return
            else:
                if len(keysym_name_from_xlib) == 1:
                    char_for_type_cmd_fallback = keysym_name_from_xlib
            
            xdotool_key_arg = keysym_name_from_xlib

            if len(keysym_name_from_xlib) == 1:
                char_code = ord(keysym_name_from_xlib)
                if char_code >= 0x80 or (char_code == keysym_number and char_code != 0x00):
                    xdotool_key_arg = f"U{char_code:04X}"
            elif keysym_number == 0x00a3: # XK_sterling
                xdotool_key_arg = "sterling"
                if not char_for_type_cmd_fallback:
                    try: char_for_type_cmd_fallback = chr(0xA3)
                    except ValueError: pass

        if xdotool_key_arg is None:
            return

        action = "keydown" if down else "keyup"
        command_key = ["xdotool", action, xdotool_key_arg]
        fallback_succeeded = False

        try:
            process_key = await subprocess.create_subprocess_exec(
                *command_key,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout_key, stderr_key = await asyncio.wait_for(process_key.communicate(), timeout=1.0)
            if process_key.returncode == 0 and not (stderr_key and (b"No such key name" in stderr_key or b"Error:" in stderr_key.lower())):
                fallback_succeeded = True
            else:
                char_to_type = char_for_type_cmd_fallback
                if not char_to_type and 'keysym_name_from_xlib' in locals() and keysym_name_from_xlib and len(keysym_name_from_xlib) == 1:
                    char_to_type = keysym_name_from_xlib
                
                if down and char_to_type and (0x20 <= ord(char_to_type) <= 0x7E or ord(char_to_type) >= 0xA0) and char_to_type.isprintable():
                    command_type = ["xdotool", "type", "--clearmodifiers", char_to_type]
                    try:
                        process_type = await subprocess.create_subprocess_exec(
                            *command_type,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE
                        )
                        await asyncio.wait_for(process_type.communicate(), timeout=1.0)
                        if process_type.returncode == 0:
                            fallback_succeeded = True
                    except (asyncio.TimeoutError, FileNotFoundError, Exception):
                        pass
        except (FileNotFoundError, asyncio.TimeoutError, Exception):
            pass

    async def send_x11_mouse(self, x, y, button_mask, scroll_magnitude, relative=False, display_id='primary'):
        if relative:
            final_x = self.last_x + x
            final_y = self.last_y + y
        else:
            offset_x = 0
            offset_y = 0
            if self.data_server_instance and hasattr(self.data_server_instance, 'display_layouts'):
                layout = self.data_server_instance.display_layouts.get(display_id) 
                if layout:
                    offset_x = layout.get('x', 0) 
                    offset_y = layout.get('y', 0)
            final_x = x + offset_x
            final_y = y + offset_y

        position_changed = (final_x != self.last_x or final_y != self.last_y)
        self.last_x = final_x
        self.last_y = final_y

        if self.wayland_input:
            is_static_relative = relative and (x == 0 and y == 0)
            
            if not is_static_relative:
                if relative:
                    if hasattr(self.wayland_input, 'inject_relative_mouse_move'):
                        self.wayland_input.inject_relative_mouse_move(float(x), float(y))
                    else:
                        self.wayland_input.inject_mouse_move(float(final_x), float(final_y))
                else:
                    self.wayland_input.inject_mouse_move(float(final_x), float(final_y))
            
            if button_mask != self.button_mask:
                for bit_index in range(8):
                    current_button_bit_value = (1 << bit_index)
                    button_state_changed = ((self.button_mask & current_button_bit_value) != \
                                            (button_mask & current_button_bit_value))

                    if button_state_changed:
                        is_pressed_now = (button_mask & current_button_bit_value) != 0
                        state = 1 if is_pressed_now else 0
                        mag = float(max(1, scroll_magnitude))

                        if bit_index == 0: # Left
                            self.wayland_input.inject_mouse_button(272, state)
                        elif bit_index == 1: # Middle
                            self.wayland_input.inject_mouse_button(274, state)
                        elif bit_index == 2: # Right
                            self.wayland_input.inject_mouse_button(273, state)
                        
                        elif bit_index == 3:
                            if scroll_magnitude > 0: 
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, 10.0 * mag)
                            else:
                                if is_pressed_now:
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)

                        elif bit_index == 4:
                            if scroll_magnitude > 0:
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, -10.0 * mag)
                            else:
                                if is_pressed_now:
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)

                        elif bit_index == 6:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(-10.0 * mag, 0.0)
                        elif bit_index == 7:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(10.0 * mag, 0.0)

            self.button_mask = button_mask
            return
        if relative:
            self.send_mouse(MOUSE_MOVE, (x, y))
        elif position_changed:
            self.send_mouse(MOUSE_POSITION, (final_x, final_y))
        self.last_x = final_x
        self.last_y = final_y
        if button_mask != self.button_mask:
            for bit_index in range(8):
                current_button_bit_value = (1 << bit_index)
                button_state_changed = ((self.button_mask & current_button_bit_value) != \
                                        (button_mask & current_button_bit_value))

                if button_state_changed:
                    is_pressed_now = (button_mask & current_button_bit_value) != 0
                    
                    action_to_send = None
                    data_to_send = None
                    is_scroll_action = False
                    performed_keyboard_combo = False 

                    if bit_index == 0:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_LEFT_ID)
                    elif bit_index == 1:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_MIDDLE_ID)
                    elif bit_index == 2:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_RIGHT_ID)
                    
                    elif bit_index == 3:
                        if scroll_magnitude > 0:
                            if is_pressed_now:
                                action_to_send = MOUSE_SCROLL_UP
                                is_scroll_action = True
                        else:
                            if is_pressed_now:
                                if self.keyboard:
                                    logger_webrtc_input.debug("Sending Alt+Left Arrow for Back")
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)
                                    performed_keyboard_combo = True
                                else:
                                    logger_webrtc_input.warning("Keyboard not available for Alt+Left.")
                    elif bit_index == 4:
                        if scroll_magnitude > 0:
                            if is_pressed_now:
                                action_to_send = MOUSE_SCROLL_DOWN
                                is_scroll_action = True
                        else:
                            if is_pressed_now:
                                if self.keyboard:
                                    logger_webrtc_input.debug("Sending Alt+Right Arrow for Forward")
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)
                                    performed_keyboard_combo = True
                                else:
                                    logger_webrtc_input.warning("Keyboard not available for Alt+Right.")
                    elif bit_index == 6:
                        if scroll_magnitude > 0 and is_pressed_now:
                            action_to_send = MOUSE_SCROLL_LEFT
                            is_scroll_action = True
                    elif bit_index == 7:
                        if scroll_magnitude > 0 and is_pressed_now:
                            action_to_send = MOUSE_SCROLL_RIGHT
                            is_scroll_action = True
                    if not performed_keyboard_combo and action_to_send is not None:
                        if is_scroll_action:
                            for _ in range(max(1, scroll_magnitude)):
                                self.send_mouse(action_to_send, None)
                        else:
                            self.send_mouse(action_to_send, data_to_send)
                
            self.button_mask = button_mask

        if not relative and self.xdisplay:
            self.xdisplay.sync()
    async def update_binary_clipboard_setting(self, enabled: bool):
        """Asynchronously updates the binary clipboard setting and restarts the monitor if it's running."""
        new_setting_str = "true" if enabled else "false"
        if self.enable_binary_clipboard == new_setting_str:
            return
        logger_webrtc_input.info(f"Binary clipboard setting changing to: {enabled}. Restarting monitor.")
        self.enable_binary_clipboard = new_setting_str
        if self.clipboard_monitor_task and not self.clipboard_monitor_task.done():
            self.stop_clipboard()  # Signal the loop to exit
            self.clipboard_monitor_task.cancel()
            try:
                await self.clipboard_monitor_task
            except asyncio.CancelledError:
                pass
            self.clipboard_monitor_task = asyncio.create_task(self.start_clipboard())
    def _get_wl_env(self):
        env = os.environ.copy()
        env["WAYLAND_DISPLAY"] = f"wayland-{self.wayland_socket_index}"
        return env

    async def read_clipboard(self, use_binary=False):
        """Reads clipboard. Supports Wayland (wl-paste) and X11 (xclip)."""
        if self.is_wayland:
            try:
                proc_types = await subprocess.create_subprocess_exec(
                    "wl-paste", "--list-types",
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    env=self._get_wl_env()
                )
                stdout_types, _ = await asyncio.wait_for(proc_types.communicate(), timeout=1.0)
                
                if proc_types.returncode != 0:
                    return None, None

                available_types = stdout_types.decode().strip().split('\n')

                if use_binary:
                    image_mimes = ['image/png', 'image/jpeg', 'image/bmp', 'image/webp']
                    target_mime = next((m for m in image_mimes if m in available_types), None)
                    if target_mime:
                        proc_data = await subprocess.create_subprocess_exec(
                            "wl-paste", "--type", target_mime,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            env=self._get_wl_env()
                        )
                        stdout_data, _ = await asyncio.wait_for(proc_data.communicate(), timeout=2.0)
                        if proc_data.returncode == 0 and stdout_data:
                            return stdout_data, target_mime
                text_mimes = ['text/plain', 'text/plain;charset=utf-8', 'UTF8_STRING', 'STRING']
                if any(t in available_types for t in text_mimes):
                    proc_text = await subprocess.create_subprocess_exec(
                        "wl-paste", "--no-newline", # Ensure exact content
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                        env=self._get_wl_env()
                    )
                    stdout_text, _ = await asyncio.wait_for(proc_text.communicate(), timeout=1.0)
                    if proc_text.returncode == 0:
                        return stdout_text.decode('utf-8', errors='replace'), 'text/plain'

                return None, None

            except Exception as e:
                logger_webrtc_input.warning(f"Error reading Wayland clipboard: {e}")
                return None, None
        try:
            proc_targets = await subprocess.create_subprocess_exec(
                "xclip", "-selection", "clipboard", "-o", "-t", "TARGETS",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            stdout_targets, _ = await asyncio.wait_for(proc_targets.communicate(), timeout=1)
            if proc_targets.returncode != 0:
                return None, None
            targets = stdout_targets.decode().strip().split('\n')
            if use_binary:
                for mime_type in ['image/png', 'image/jpeg', 'image/bmp', 'image/svg', 'image/webp']:
                    if mime_type in targets:
                        proc_data = await subprocess.create_subprocess_exec(
                            "xclip", "-selection", "clipboard", "-o", "-t", mime_type,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE
                        )
                        stdout_data, _ = await asyncio.wait_for(proc_data.communicate(), timeout=1)
                        if proc_data.returncode == 0 and stdout_data:
                            return stdout_data, mime_type
            if 'UTF8_STRING' in targets:
                proc_text = await subprocess.create_subprocess_exec(
                    "xclip", "-selection", "clipboard", "-o", "-t", "UTF8_STRING",
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE
                )
                stdout_text, _ = await asyncio.wait_for(proc_text.communicate(), timeout=1)
                if proc_text.returncode == 0:
                    return stdout_text.decode(), 'text/plain'
            return None, None
        except Exception as e:
            logger_webrtc_input.warning(f"Error reading clipboard with xclip: {e}")
            return None, None
    async def write_clipboard(self, data, mime_type="text/plain"):
        if not data:
            return True
        input_bytes = data if isinstance(data, bytes) else data.encode()
        if self.is_wayland:
            try:
                cmd = ["wl-copy", "--type", mime_type]
                process = await subprocess.create_subprocess_exec(
                    *cmd,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    env=self._get_wl_env()
                )
                if process.stdin:
                    process.stdin.write(input_bytes)
                    await process.stdin.drain()
                    process.stdin.close()
                stdout, stderr = await asyncio.wait_for(process.communicate(), timeout=2.0)
                if process.returncode == 0:
                    return True
                else:
                    logger_webrtc_input.warning(f"wl-copy failed code: {process.returncode}, err: {stderr.decode()}")
                    return False
            except Exception as e:
                return False
        try:
            process = await subprocess.create_subprocess_exec(
                "xclip", "-selection", "clipboard", "-i", "-t", mime_type,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL
            )
            if process.stdin:
                process.stdin.write(input_bytes)
                await process.stdin.drain()
                process.stdin.close()
            return_code = await asyncio.wait_for(process.wait(), timeout=2.0)
            if return_code == 0:
                return True
            else:
                logger_webrtc_input.warning(f"xclip process exited with non-zero code: {return_code}")
                return False
        except asyncio.TimeoutError:
            logger_webrtc_input.warning("Timeout waiting for xclip process to terminate.")
            return False
        except Exception:
            logger_webrtc_input.warning("Error writing to clipboard with xclip", exc_info=True)
            return False
    async def start_clipboard(self):
        if self.enable_clipboard not in ["true", "out"]:
            logger_webrtc_input.info("Skipping outbound clipboard service."); return
        
        logger_webrtc_input.info(f"Clipboard monitor running (binary mode: {self.enable_binary_clipboard in ['true', 'out']})")
        self.clipboard_running = True
        last_data_bytes = b""
        while self.clipboard_running:
            try:
                use_binary = self.enable_binary_clipboard in ["true", "out"]
                curr_data, curr_mime = await self.read_clipboard(use_binary=use_binary)
                if curr_data is None:
                    curr_data_bytes = None
                else:
                    curr_data_bytes = curr_data.encode('utf-8') if isinstance(curr_data, str) else curr_data
                if curr_data_bytes is not None and curr_data_bytes != last_data_bytes:
                    log_data = curr_data if isinstance(curr_data, str) else f"<{len(curr_data)} bytes>"
                    logger_webrtc_input.info(f"Clipboard changed. Sending content ({curr_mime})")
                    await self.on_clipboard_read(curr_data, curr_mime)
                    last_data_bytes = curr_data_bytes
                await asyncio.sleep(0.5)
            except asyncio.CancelledError:
                logger_webrtc_input.info("Clipboard monitor task cancelled.")
                break
            except Exception as e:
                logger_webrtc_input.error(f"Error in clipboard monitor loop: {e}", exc_info=True)
                await asyncio.sleep(2)
        
        self.clipboard_running = False
        logger_webrtc_input.info("Clipboard monitor stopped")

    def stop_clipboard(self): self.clipboard_running = False; logger_webrtc_input.info("Stopping clipboard monitor")
    
    async def start_cursor_monitor(self):
        if self.is_wayland:
            logger_webrtc_input.info("Wayland mode: Cursor monitor disabled (handled by compositor callback).")
            return
        if not self.xdisplay.has_extension("XFIXES"):
            if self.xdisplay.query_extension("XFIXES") is None:
                logger_webrtc_input.error(
                    "XFIXES extension not supported, cannot watch cursor changes"
                )
                return
        xfixes_version = self.xdisplay.xfixes_query_version()
        logger_webrtc_input.info(
            "Found XFIXES version %s.%s",
            xfixes_version.major_version,
            xfixes_version.minor_version,
        )
        logger_webrtc_input.info("starting cursor monitor")
        self.cursors_running = True
        screen = self.xdisplay.screen()
        self.xdisplay.xfixes_select_cursor_input(
            screen.root, xfixes.XFixesDisplayCursorNotifyMask
        )
        logger_webrtc_input.info("watching for cursor changes")
        try:
            cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
            cursor_data = self.cursor_to_msg(cursor_image)
            self.on_cursor_change(cursor_data)
        except Exception as e:
            logger_webrtc_input.warning("exception from fetching initial cursor image: %s", e)
            
        while self.cursors_running:
            if self.xdisplay.pending_events() == 0:
                await asyncio.sleep(0.02)
                continue
            
            event = self.xdisplay.next_event()
            if (event.type, 0) == self.xdisplay.extension_event.DisplayCursorNotify:
                try:
                    cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
                    cursor_data = self.cursor_to_msg(cursor_image)
                    self.on_cursor_change(cursor_data)
                except Exception as e:
                    logger_webrtc_input.warning(
                        "exception from fetching cursor image on change: %s", e
                    )
        logger_webrtc_input.info("cursor monitor stopped")

    def stop_cursor_monitor(self):
        logger_webrtc_input.info("stopping cursor monitor")
        self.cursors_running = False

    def _cursor_image_to_pil(self, cursor):
        byte_data = b''.join(p.to_bytes(4, 'little') for p in cursor.cursor_image)
        return Image.frombytes("RGBA", (cursor.width, cursor.height), byte_data, "raw", "BGRA")

    def cursor_to_msg(self, cursor):
        if not cursor or cursor.width == 0 or cursor.height == 0:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": cursor.cursor_serial if cursor else 0,
            }
        im = self._cursor_image_to_pil(cursor)
        bbox = im.getbbox()
        if bbox is None:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": cursor.cursor_serial,
            }
        cropped_im = im.crop(bbox)
        left, upper, right, lower = bbox
        new_hotx = cursor.xhot - left
        new_hoty = cursor.yhot - upper
        if cropped_im.width > self.cursor_size_cap or cropped_im.height > self.cursor_size_cap:
            if self.cursor_debug:
                logger_webrtc_input.info(f"Cursor ({cropped_im.width}x{cropped_im.height}) exceeds cap ({self.cursor_size_cap}x{self.cursor_size_cap}). Resizing.")
            max_dim = max(cropped_im.width, cropped_im.height)
            scale_factor = self.cursor_size_cap / max_dim
            new_width = int(cropped_im.width * scale_factor)
            new_height = int(cropped_im.height * scale_factor)
            try:
                resampling_filter = Image.Resampling.LANCZOS
            except AttributeError:
                resampling_filter = Image.LANCZOS
            cropped_im = cropped_im.resize((new_width, new_height), resample=resampling_filter)
            new_hotx = int(new_hotx * scale_factor)
            new_hoty = int(new_hoty * scale_factor)
        with io.BytesIO() as f:
            cropped_im.save(f, "PNG")
            png_data = f.getvalue()
        png_data_b64 = base64.b64encode(png_data)
        return {
            "curdata": png_data_b64.decode(),
            "width": cropped_im.width,
            "height": cropped_im.height,
            "hotx": new_hotx,
            "hoty": new_hoty,
            "handle": cursor.cursor_serial,
        }

    async def stop_gamepad_servers(self):
        logger_webrtc_input.info("Stopping all gamepad instances.")
        await self.__gamepad_disconnect()

    async def on_message(self, msg, display_id='primary'):
        toks = msg.split(",")
        msg_type = toks[0]

        if msg_type == "pong":
            if self.ping_start is None: logger_webrtc_input.warning("received pong before ping"); return
            self.on_ping_response(float("%.3f" % ((time.time() - self.ping_start) / 2 * 1000)))
        elif msg_type == "kd":
            keysym = int(toks[1])
            is_printable = (0x20 <= keysym <= 0xFF) or ((keysym & 0xFF000000) == 0x01000000)
            if keysym in self.MODIFIER_KEYSYMS:
                self.active_modifiers.add(keysym)
            if is_printable and not self.active_modifiers:
                unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
                try:            
                    char_to_type = chr(unicode_codepoint)
                    if not self.is_wayland and (not char_to_type.isalpha() and char_to_type != ' '):
                        await self.on_message(f"co,end,{char_to_type}")
                        self.atomically_typed_keys.add(keysym)
                    else:
                        await self.send_x11_keypress(keysym, down=True)
                except (ValueError, TypeError):
                    await self.send_x11_keypress(keysym, down=True)
            else:   
                await self.send_x11_keypress(keysym, down=True)
        elif msg_type == "ku":
            keysym = int(toks[1])
            
            if keysym in self.MODIFIER_KEYSYMS:
                self.active_modifiers.discard(keysym)
            if keysym in self.atomically_typed_keys:
                self.atomically_typed_keys.discard(keysym)
                pass
            else:
                await self.send_x11_keypress(keysym, down=False)
        elif msg_type == "kr": await self.reset_keyboard()
        elif msg_type in ["m", "m2"]:
            relative = msg_type == "m2"
            try: x, y, button_mask, scroll_magnitude = [int(i) for i in toks[1:]]
            except: x,y,button_mask,scroll_magnitude = 0,0,self.button_mask,0; relative=False
            try: await self.send_x11_mouse(x, y, button_mask, scroll_magnitude, relative, display_id=display_id)
            except Exception as e: logger_webrtc_input.warning(f"Failed to set mouse cursor: {e}")
        elif msg_type == "p": self.on_mouse_pointer_visible(bool(int(toks[1])))
        elif msg_type == "vb": await self.on_video_encoder_bit_rate(int(toks[1]))
        elif msg_type == "ab": await self.on_audio_encoder_bit_rate(int(toks[1]))
        elif msg_type == "js": 
            cmd = toks[1]
            gamepad_idx = int(toks[2])

            if not (0 <= gamepad_idx < self.num_gamepads):
                logger_webrtc_input.error(f"Client message for gamepad index {gamepad_idx} is out of range (0-{self.num_gamepads-1}).")
                return

            # Get the persistent gamepad instance. It should always exist after connect().
            target_gamepad_instance = self.gamepad_instances.get(gamepad_idx)
            if not target_gamepad_instance:
                logger_webrtc_input.error(
                    f"CRITICAL: No persistent SelkiesGamepad instance found for index {gamepad_idx} in on_message. "
                    f"Gamepad system may not be initialized correctly."
                )
                return

            if cmd == "c": 
                try: client_name_decoded = base64.b64decode(toks[3]).decode('latin-1', 'ignore')[:255]
                except Exception as e: client_name_decoded = f"ClientGamepad{gamepad_idx}"; logger_webrtc_input.warning(f"Error decoding client gamepad name: {e}")
                client_num_axes, client_num_btns = int(toks[4]), int(toks[5])
                
                await self.__gamepad_connect(gamepad_idx, client_name_decoded, client_num_btns, client_num_axes)

            elif cmd == "d": 
                await self.__gamepad_disconnect(gamepad_idx)
            
            elif cmd == "b": 
                button_num = int(toks[3])
                button_val = float(toks[4])
                # Send event to the persistent target_gamepad_instance
                target_gamepad_instance.send_event(button_num, button_val, is_button_event=True)

            elif cmd == "a": 
                axis_num = int(toks[3])
                axis_val = float(toks[4])
                # Send event to the persistent target_gamepad_instance
                target_gamepad_instance.send_event(axis_num, axis_val, is_button_event=False)
            
            else: logger_webrtc_input.warning(f"Unhandled joystick command for slot {gamepad_idx}: js {cmd}")
        elif msg_type == "cws":
            if self.enable_clipboard in ["true", "in"]:
                try:
                    self.multipart_clipboard_total_size = int(toks[1])
                    self.multipart_clipboard_mime_type = "text/plain"
                    self.multipart_clipboard_buffer = io.BytesIO()
                    self.multipart_clipboard_in_progress = True
                    logger_webrtc_input.info(f"Starting multi-part text clipboard receive, total size: {self.multipart_clipboard_total_size}")
                except Exception as e:
                    logger_webrtc_input.error(f"Invalid cws message: {msg}, error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting multi-part clipboard write: inbound clipboard disabled.")
        elif msg_type == "cbs":
            if self.enable_clipboard in ["true", "in"]:
                try:
                    self.multipart_clipboard_mime_type = toks[1]
                    self.multipart_clipboard_total_size = int(toks[2])
                    self.multipart_clipboard_buffer = io.BytesIO()
                    self.multipart_clipboard_in_progress = True
                    logger_webrtc_input.info(f"Starting multi-part binary clipboard receive ({self.multipart_clipboard_mime_type}), total size: {self.multipart_clipboard_total_size}")
                except Exception as e:
                    logger_webrtc_input.error(f"Invalid cbs message: {msg}, error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting multi-part clipboard write: inbound clipboard disabled.")
        elif msg_type == "cwd" or msg_type == "cbd":
            if self.multipart_clipboard_in_progress:
                try:
                    chunk_data = base64.b64decode(toks[1])
                    self.multipart_clipboard_buffer.write(chunk_data)
                except Exception as e:
                    logger_webrtc_input.error(f"Failed to process clipboard data chunk: {e}")
                    self.multipart_clipboard_in_progress = False
        elif msg_type == "cwe" or msg_type == "cbe":
            if self.multipart_clipboard_in_progress:
                received_size = self.multipart_clipboard_buffer.tell()
                if received_size != self.multipart_clipboard_total_size:
                    logger_webrtc_input.error(f"Multi-part clipboard size mismatch. Expected {self.multipart_clipboard_total_size}, got {received_size}. Aborting.")
                else:
                    logger_webrtc_input.info(f"Finished multi-part clipboard receive. Total size: {received_size}")
                    data = self.multipart_clipboard_buffer.getvalue()
                    mime_type = self.multipart_clipboard_mime_type
                    async def _write_multipart():
                        if mime_type == "text/plain":
                            text_data = data.decode("utf-8", "ignore")
                            if await self.write_clipboard(text_data):
                                logger_webrtc_input.info(f"Set multi-part clipboard content, length: {len(text_data)}")
                        else:
                            if await self.write_clipboard(data, mime_type=mime_type):
                                logger_webrtc_input.info(f"Set multi-part binary clipboard content ({mime_type}), size: {len(data)} bytes")
                    asyncio.create_task(_write_multipart())
                self.multipart_clipboard_buffer = None
                self.multipart_clipboard_in_progress = False
        elif msg_type == "cr": 
            if self.enable_clipboard in ["true", "out"]:
                data, mime_type = await self.read_clipboard(use_binary=self.enable_binary_clipboard in ["true", "out"])
                if data:
                    await self.on_clipboard_read(data, mime_type)
                else: logger_webrtc_input.debug("No clipboard content to send on request")
            else: logger_webrtc_input.warning("Rejecting clipboard read: outbound clipboard disabled.")
        elif msg_type == "cb":
            if self.enable_clipboard in ["true", "in"]:
                try:
                    _, mime_type, b64_data = toks
                    data_bytes = base64.b64decode(b64_data)
                    async def _write_cb():
                        if await self.write_clipboard(data_bytes, mime_type=mime_type):
                            logger_webrtc_input.info(f"Set binary clipboard content ({mime_type}), size: {len(data_bytes)} bytes")
                    asyncio.create_task(_write_cb())
                except Exception as e:
                    logger_webrtc_input.error(f"Binary clipboard write error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting binary clipboard write: inbound binary clipboard disabled.")
        elif msg_type == "cw": 
            if self.enable_clipboard in ["true", "in"]:
                try: 
                    data = base64.b64decode(toks[1]).decode("utf-8", 'ignore')
                    async def _write_cw():
                        if await self.write_clipboard(data):
                            logger_webrtc_input.info(f"Set clipboard content, length: {len(data)}")
                    asyncio.create_task(_write_cw())
                except Exception as e: 
                    logger_webrtc_input.error(f"Clipboard decode error: {e}")
                    return
            else: 
                logger_webrtc_input.warning("Rejecting clipboard write: inbound clipboard disabled.")
        elif msg_type == "r": 
            res = toks[1]
            if re.fullmatch(r"^\d+x\d+$", res):
                w, h = [int(i) + int(i)%2 for i in res.split("x")] 
                await self.on_resize(f"{w}x{h}")
            else: logger_webrtc_input.warning(f"Rejecting resolution change, invalid: {res}")
        elif msg_type == "s": 
            scale = toks[1]
            if re.fullmatch(r"^\d+(\.\d+)?$", scale): await self.on_scaling_ratio(float(scale))
            else: logger_webrtc_input.warning(f"Rejecting scaling change, invalid: {scale}")
        elif msg_type == "cmd":
            if len(toks) > 1:
                command_to_run = ",".join(toks[1:])
                logger_webrtc_input.info(f"Attempting to execute command: '{command_to_run}'")
                home_directory = os.path.expanduser("~")
                try:
                    # Use asyncio subprocess for fire-and-forget execution
                    # stdout and stderr are redirected to DEVNULL to ignore output.
                    process = await subprocess.create_subprocess_shell(
                        command_to_run,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        cwd=home_directory
                    )
                    logger_webrtc_input.info(f"Successfully launched command: '{command_to_run}'")
                except Exception as e:
                    logger_webrtc_input.error(f"Failed to launch command '{command_to_run}': {e}")
            else:
                logger_webrtc_input.warning("Received 'cmd' message without a command string.")
        elif msg_type == "_arg_fps": self.on_set_fps(int(toks[1]))
        elif msg_type == "_arg_resize":
            if len(toks) == 3:
                enabled, res_str = toks[1].lower() == "true", toks[2]
                enable_res = None
                if re.fullmatch(r"^\d+x\d+$", res_str):
                    w,h = [int(i)+int(i)%2 for i in res_str.split("x")]; enable_res = f"{w}x{h}"
                elif res_str: logger_webrtc_input.warning(f"Invalid resolution for enable_resize: {res_str}")
                self.on_set_enable_resize(enabled, enable_res)
            else: logger_webrtc_input.error("Invalid _arg_resize command format")
        elif msg_type == "_f": 
            try: self.on_client_fps(int(toks[1]))
            except: logger_webrtc_input.error(f"Failed to parse client FPS: {toks}")
        elif msg_type == "_l": 
            try: self.on_client_latency(int(toks[1]))
            except: logger_webrtc_input.error(f"Failed to parse client latency: {toks}")
        elif msg_type in ["_stats_video", "_stats_audio"]: 
            try: await self.on_client_webrtc_stats(msg_type, ",".join(toks[1:]))
            except: logger_webrtc_input.error("Failed to parse WebRTC Statistics")
        elif msg_type == "co" and toks[1] == "end": 
            try:
                text_to_type = msg[7:]
                cmd = ["wtype", "--", text_to_type] if self.is_wayland else ["xdotool", "type", text_to_type]
                process = await subprocess.create_subprocess_exec(
                    *cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE
                )
                await asyncio.wait_for(process.communicate(), timeout=0.5)
            except Exception as e: logger_webrtc_input.warning(f"Error with xdotool type: {e}")
        elif toks[0].startswith("FILE_UPLOAD_START:"):
            if self.upload_dir_path is None:
                logger_webrtc_input.warning("Upload directory doesn't exits, skipping the file upload")
                return
            _, file, size = toks[0].split(":", 2)
            # create dir/file instance to wirte data to
            self.handle_upload_dir(file, size)

        elif toks[0].startswith("FILE_UPLOAD_END:"):
            toks = toks[0].split(":", 2)
            logger_webrtc_input.info("Received FILE UPLOAD END: " + " ".join(toks[1:]))
            if (self.active_upload_target_path_conn and self.active_upload_target_path_conn in self.active_uploads_by_path_conn):
                self.active_uploads_by_path_conn[self.active_upload_target_path_conn].close()
                logger_webrtc_input.info(f"Upload finished: {self.active_upload_target_path_conn}")
                del self.active_uploads_by_path_conn[self.active_upload_target_path_conn]
                self.active_upload_target_path_conn = None

        elif toks[0].startswith("FILE_UPLOAD_ERROR:"):
            logger_webrtc_input.error(f"Client reported upload error: {toks[0]}")
            if (self.active_upload_target_path_conn and self.active_upload_target_path_conn in self.active_uploads_by_path_conn):
                self.active_uploads_by_path_conn[self.active_upload_target_path_conn].close()
                try:
                    os.remove(self.active_upload_target_path_conn)
                except OSError:
                    pass
                del self.active_uploads_by_path_conn[self.active_upload_target_path_conn]
            self.active_upload_target_path_conn = None
            logger_webrtc_input.info(f"Purged the file {toks[0].split(':', 2)[1]}")
        elif toks[0].startswith("SETTINGS"):
            settings_data = ','.join(toks[1:]) if len(toks) > 1 else ""
            logger_webrtc_input.info(f"Received SETTINGS message: {settings_data}")
            try:
                settings_json = json.loads(settings_data)
                self.on_update_settings(settings_json)
            except Exception as e:
                logger_webrtc_input.error(f"Failed to parse SETTINGS data: {e}")
        else:
            logger_webrtc_input.info(f"Unknown data channel message: {msg[:100]}") 

    def initialize_upload_dir(self):
        if self.upload_dir in ["/sys", "/proc", "/dev"]:
            logger_webrtc_input.info("Can not initialize upload directory at /sys /proc /dev locations")
            return
        if not self.upload_dir:
            logger_webrtc_input.info("Upload dir is empty")
            return

        if self.upload_dir == "~/Desktop":
            # expand the user dir path
            self.upload_dir_path = os.path.expanduser(self.upload_dir)
        else:
            self.upload_dir_path = self.upload_dir

        try:
            os.makedirs(self.upload_dir_path, exist_ok=True)
            logger_webrtc_input.info(f"Upload directory ensured: {self.upload_dir_path}")
        except OSError as e:
            logger_webrtc_input.error(f"Could not create upload directory {self.upload_dir_path}: {e}")
            self.upload_dir_path = None

    def handle_upload_dir(self, filename, filesize):
        try:
            rel_path_from_client, size_str = filename, filesize
            file_size = int(size_str)

            sane_rel_path = rel_path_from_client.strip('/\\')
            sane_rel_path = os.path.normpath(sane_rel_path)
            path_components = [comp for comp in sane_rel_path.split(os.sep) if comp and comp != '.']

            if not path_components or sane_rel_path.startswith(os.sep) or sane_rel_path.startswith('/') or \
                sane_rel_path.startswith('\\') or ".." in path_components:
                logger_webrtc_input.error(f"Invalid or malicious relative path from client: '{rel_path_from_client}'. Discarding.")

            sane_rel_path = os.path.join(*path_components)
            final_server_path = os.path.join(self.upload_dir_path, sane_rel_path)
            real_upload_dir = os.path.realpath(self.upload_dir_path)
            intended_parent_dir_abs = os.path.abspath(os.path.dirname(final_server_path))
            real_upload_dir_abs = os.path.abspath(real_upload_dir)

            if not intended_parent_dir_abs.startswith(real_upload_dir_abs):
                logger_webrtc_input.error(f"Path escape attempt detected: '{final_server_path}' (from client: '{rel_path_from_client}') is outside of '{real_upload_dir_abs}'. Discarding.")
                return

            target_dir = os.path.dirname(final_server_path)

            if target_dir and target_dir != real_upload_dir_abs and not os.path.exists(target_dir):
                if not os.path.abspath(target_dir).startswith(real_upload_dir_abs):
                    logger_webrtc_input.error(f"Directory creation escape attempt: '{target_dir}' is outside of '{real_upload_dir_abs}'. Discarding.")
                    return
                try:
                    os.makedirs(target_dir, exist_ok=True)
                    logger_webrtc_input.info(f"Created directory for upload: {target_dir}")
                except OSError as e_mkdir:
                    logger_webrtc_input.error(f"Could not create directory {target_dir} for upload: {e_mkdir}")
                    return

            if (self.active_upload_target_path_conn and self.active_upload_target_path_conn in self.active_uploads_by_path_conn):
                try:
                    self.active_uploads_by_path_conn[self.active_upload_target_path_conn].close()
                except Exception as e_close_old:
                    logger_webrtc_input.warning(f"Error closing previous upload stream {self.active_upload_target_path_conn}: {e_close_old}")
                del self.active_uploads_by_path_conn[self.active_upload_target_path_conn]

            self.active_uploads_by_path_conn[final_server_path] = open(final_server_path, "wb")
            self.active_upload_target_path_conn = final_server_path
            logger_webrtc_input.info(f"Upload started: {final_server_path} (client rel_path: '{rel_path_from_client}', size: {file_size})")
        except ValueError:
            logger_webrtc_input.error(f"Invalid FILE_UPLOAD_START format: {filename}")
        except Exception as e_fup_start:
            logger_webrtc_input.error(f"FILE_UPLOAD_START processing error: {e_fup_start}", exc_info=True)

    async def on_msg_data(self, data):
        # Data being received on auxiliary channel would be of type Bytes
        if len(data) <= 0:
            return
        data_type, payload = data[0], data[1:]
        if data_type == 0x01:  # inidicates file data
            if (self.active_upload_target_path_conn and self.active_upload_target_path_conn in self.active_uploads_by_path_conn):
                try:
                    self.active_uploads_by_path_conn[self.active_upload_target_path_conn].write(payload)
                except Exception as e_write:
                    logger_webrtc_input.error(f"File write error for {self.active_upload_target_path_conn}: {e_write}")
                    try:
                        self.active_uploads_by_path_conn[self.active_upload_target_path_conn].close()
                        os.remove(self.active_upload_target_path_conn)
                    except Exception:
                        pass
                    del self.active_uploads_by_path_conn[self.active_upload_target_path_conn]
                    self.active_upload_target_path_conn = None
            else:
                logger_webrtc_input.warning("received file data after upload path is closed")

# MOUSE_POSITION
MOUSE_POSITION = 10
MOUSE_MOVE = 11
MOUSE_SCROLL_UP = 20
MOUSE_SCROLL_DOWN = 21
MOUSE_SCROLL_LEFT = 22
MOUSE_SCROLL_RIGHT = 23
MOUSE_BUTTON_PRESS = 30
MOUSE_BUTTON_RELEASE = 31
MOUSE_BUTTON = 40
MOUSE_BUTTON_LEFT_ID = 41 
MOUSE_BUTTON_MIDDLE_ID = 42
MOUSE_BUTTON_RIGHT_ID = 43

# UINPUT constants if uinput_mouse_socket_path is used
UINPUT_BTN_LEFT = (EV_KEY, BTN_LEFT) 
UINPUT_BTN_MIDDLE = (EV_KEY, BTN_MIDDLE) 
UINPUT_BTN_RIGHT = (EV_KEY, BTN_RIGHT) 
UINPUT_REL_X = (EV_REL, 0x00) # REL_X
UINPUT_REL_Y = (EV_REL, 0x01) # REL_Y
UINPUT_REL_WHEEL = (EV_REL, 0x08) # REL_WHEEL

pynput_left = None
pynput_middle = None
pynput_right = None

if pynput is not None:
    pynput_left = pynput.mouse.Button.left
    pynput_middle = pynput.mouse.Button.middle
    pynput_right = pynput.mouse.Button.right

MOUSE_BUTTON_MAP = {
    MOUSE_BUTTON_LEFT_ID: {"uinput": UINPUT_BTN_LEFT, "pynput": pynput_left},
    MOUSE_BUTTON_MIDDLE_ID: {"uinput": UINPUT_BTN_MIDDLE, "pynput": pynput_middle},
    MOUSE_BUTTON_RIGHT_ID: {"uinput": UINPUT_BTN_RIGHT, "pynput": pynput_right},
}
