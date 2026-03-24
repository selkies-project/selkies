# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import asyncio
import os
import struct
import socket
import time
from queue import Queue
from input_event_codes import *
from signal import (
    signal,
    SIGINT,
)

import logging
logger = logging.getLogger("selkies_gamepad")
logger.setLevel(logging.INFO)

STANDARD_XPAD_CONFIG = {
    # Browser detects xpad as 4 axes 17 button controller.
    # Linux xpad has 11 buttons and 8 axes.

    "name": "Selkies Controller",
    "btn_map": [
        BTN_A,      # 0
        BTN_B,      # 1
        BTN_X,      # 2
        BTN_Y,      # 3
        BTN_TL,     # 4
        BTN_TR,     # 5
        BTN_SELECT, # 6
        BTN_START,  # 7
        BTN_MODE,   # 8
        BTN_THUMBL, # 9
        BTN_THUMBR  # 10
    ],
    "axes_map": [
        ABS_X,      # 0
        ABS_Y,      # 1
        ABS_Z,      # 2
        ABS_RX,     # 3
        ABS_RY,     # 4
        ABS_RZ,     # 5
        ABS_HAT0X,  # 6
        ABS_HAT0Y   # 7
    ],


    # Input mapping from javascript:
    #   Axis 0: Left thumbstick X
    #   Axis 1: Left thumbstick Y
    #   Axis 2: Right thumbstick X
    #   Axis 3: Right thumbstick Y
    #   Button 0: A
    #   Button 1: B
    #   Button 2: X
    #   Button 3: Y
    #   Button 4: L1
    #   Button 5: R1
    #   Button 6: L2 (abs)
    #   Button 7: R2 (abs)
    #   Button 8: Select
    #   Button 9: Start
    #   Button 10: L3
    #   Button 11: R3
    #   Button 12: DPad Up
    #   Button 13: DPad Down
    #   Button 14: DPad Left
    #   Button 15: DPad Right
    #   Button 16: Xbox Button
    "mapping": {
        # Remap some buttons to axes
        "axes_to_btn": {
            2: (6,),     # ABS_Z to L2
            5: (7,),     # ABS_RZ to R2
            6: (15, 14), # ABS_HAT0X to DPad Left and DPad Right
            7: (13, 12)  # ABS_HAT0Y to DPad Down and DPad Up
        },
        # Remap axis, done in conjunction with axes_to_btn_map
        "axes": {
            2: 3, # Right Thumbstick X to ABS_RX
            3: 4, # Right Thumbstick Y to ABS_RY
        },
        # Because some buttons are remapped to axis, remap the other buttons to match target mapping.
        "btns": {
            8: 6,    # Select to BTN_SELECT
            9: 7,    # Start to BTN_START
            10: 9,   # L3 to BTN_THUMBL
            11: 10,  # R2 to BTN_THUMBR
            16: 8    # BTN_MODE
        },
        # Treat triggers as full range single axes
        "trigger_axes": [
            2, # ABS_Z
            5  # ABS_RZ
        ]
    }
}

# Vendor and product IDs to configs.
XPAD_CONFIG_MAP = {
    ("045e", "0b12"): STANDARD_XPAD_CONFIG,   # Xbox Series S/X
}

# From /usr/include/linux/joystick.h
JS_EVENT_BUTTON = 0x01
JS_EVENT_AXIS = 0x02

# Max num of buttons and axes
MAX_BTNS = 512
MAX_AXES = 64

# Range for axis values
ABS_MIN = -32767
ABS_MAX = 32767

# Joystick event struct
# https://www.kernel.org/doc/Documentation/input/joystick-api.txt
# struct js_event {
#    __u32 time;     /* event timestamp in milliseconds */
#    __s16 value;    /* value */
#    __u8 type;      /* event type */
#    __u8 number;    /* axis/button number */
# };

def get_btn_event(btn_num, btn_val):
    ts = int((time.time() * 1000) % 1000000000)

    # see js_event struct definition above.
    # https://docs.python.org/3/library/struct.html
    struct_format = 'IhBB'
    event = struct.pack(struct_format, ts, btn_val,
                        JS_EVENT_BUTTON, btn_num)

    logger.debug(struct.unpack(struct_format, event))

    return event


def get_axis_event(axis_num, axis_val):
    ts = int((time.time() * 1000) % 1000000000)

    # see js_event struct definition above.
    # https://docs.python.org/3/library/struct.html
    struct_format = 'IhBB'
    event = struct.pack(struct_format, ts, axis_val,
                        JS_EVENT_AXIS, axis_num)

    logger.debug(struct.unpack(struct_format, event))

    return event

def detect_gamepad_config(name):
    # TODO switch mapping based on name.
    return STANDARD_XPAD_CONFIG

def get_num_btns_for_mapping(cfg):
    num_mapped_btns = len(
        [i for j in cfg["axes_to_btn_map"].values() for i in j])
    return len(cfg["btn_map"]) + num_mapped_btns


def get_num_axes_for_mapping(cfg):
    return len(cfg["axes_map"])


def normalize_axis_val(val):
    return round(ABS_MIN + ((val+1) * (ABS_MAX - ABS_MIN)) / 2)


def normalize_trigger_val(val):
    return round(val * (ABS_MAX - ABS_MIN)) + ABS_MIN

class SelkiesGamepad:
    def __init__(self, socket_path):
        self.socket_path = socket_path

        # Gamepad input mapper instance
        # created when calling set_config()
        self.mapper = None
        self.name = None

        # socket server
        self.server = None

        # Joystick config, set dynamically.
        self.config = None

        # Map of client file descriptors to sockets.
        self.clients = {}

        # queue of events to send.
        self.events = Queue()

        # flag indicating instance running.
        self.running = False
    
    def set_config(self, name, num_btns, num_axes):
        self.name = name
        self.config = detect_gamepad_config(name)
        self.mapper = GamepadMapper(self.config, name, num_btns, num_axes)

    def __make_config(self):
        '''
        Build config message to be sent to new clients.
        Requires that self.config has been set first.
        '''
        if not self.config:
            logger.error("could not make js config because it has not yet been set.")
            return None

        num_btns = len(self.config["btn_map"])
        num_axes = len(self.config["axes_map"])

        # zero fill array to max length.
        btn_map = [i for i in self.config["btn_map"]]
        axes_map = [i for i in self.config["axes_map"]]

        btn_map[num_btns:MAX_BTNS] = [0 for i in range(num_btns, MAX_BTNS)]
        axes_map[num_axes:MAX_AXES] = [0 for i in range(num_axes, MAX_AXES)]

        struct_fmt = "255sHH%dH%dB" % (MAX_BTNS, MAX_AXES)
        data = struct.pack(struct_fmt,
                           self.config["name"].encode(),
                           num_btns,
                           num_axes,
                           *btn_map,
                           *axes_map
                           )
        return data

    async def __send_events(self):
        while self.running:
            if self.events.empty():
                await asyncio.sleep(0.001)
                continue
            while self.running and not self.events.empty():
                await self.send_event(self.events.get())

    def send_btn(self, btn_num, btn_val):
        if not self.mapper:
            logger.warning("failed to send js button event because mapper was not set")
            return
        event = self.mapper.get_mapped_btn(btn_num, btn_val)
        if event is not None:
            self.events.put(event)

    def send_axis(self, axis_num, axis_val):
        if not self.mapper:
            logger.warning("failed to send js axis event because mapper was not set")
            return
        event = self.mapper.get_mapped_axis(axis_num, axis_val)
        if event is not None:
            self.events.put(event)

    async def send_event(self, event):
        if len(self.clients) < 1:
            return

        closed_clients = []
        for fd in self.clients:
            try:
                client = self.clients[fd]
                logger.debug("Sending event to client with fd: %d" % fd)
                await asyncio.to_thread(socket.sendall, client, event)
            except BrokenPipeError:
                logger.info("Client %d disconnected" % fd)
                closed_clients.append(fd)
                client.close()

        for fd in closed_clients:
            del self.clients[fd]

    async def setup_client(self, client):
        logger.info("Sending config to client with fd: %d" % client.fileno())
        try:
            config_data = self.__make_config()
            if not config_data:
                return
            await asyncio.to_thread(socket.sendall, client, config_data)
            await asyncio.sleep(0.5)
            # Send zero values for all buttons and axis.
            for btn_num in range(len(self.config["btn_map"])):
                self.send_btn(btn_num, 0)
            for axis_num in range(len(self.config["axes_map"])):
                self.send_axis(axis_num, 0)

        except BrokenPipeError:
            client.close()
            logger.info("Client disconnected")

    async def run_server(self):
        try:
            os.unlink(self.socket_path)
        except OSError:
            if os.path.exists(self.socket_path):
                raise

        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.socket_path)
        self.server.listen(1)
        self.server.setblocking(False)

        logger.info('Listening for connections on %s' % self.socket_path)

        # start task to process event queue.
        asyncio.create_task(self.__send_events())

        self.running = True
        try:
            while self.running:
                try:
                    client, _ = await asyncio.wait_for(socket.accept(self.server), timeout=1)
                except asyncio.TimeoutError:
                    continue

                fd = client.fileno()
                logger.info("Client connected with fd: %d" % fd)

                # Send client the joystick configuration
                await self.setup_client(client)

                # Add client to dictionary to receive events.
                self.clients[fd] = client
        finally:
            self.server.close()
            try:
                os.unlink(self.socket_path)
            except Exception:
                pass
        
        logger.info("Stopped gamepad socket server for %s" % self.socket_path)

    def stop_server(self):
        self.running = False
        self.server.close()
        try:
            os.unlink(self.socket_path)
        except Exception:
            pass

class GamepadMapper:
    def __init__(self, config, name, num_btns, num_axes):
        self.config = config
        self.input_name = name
        self.input_num_btns = num_btns
        self.input_num_axes = num_axes
    
    def get_mapped_btn(self, btn_num, btn_val):
        '''
        return either a button or axis event based on mapping. 
        '''

        # Check to see if button is mapped to an axis
        axis_num = None
        axis_sign = 1
        for axis, mapping in self.config["mapping"]["axes_to_btn"].items():
            if btn_num in mapping:
                axis_num = axis
                if len(mapping) > 1:
                    axis_sign = 1 if mapping[0] == btn_num else -1
                break

        if axis_num is not None:
            # Remap button to axis
            # Normalize for input between -1 and 1
            axis_val = normalize_axis_val(btn_val*axis_sign)

            if axis_num in self.config["mapping"]["trigger_axes"]:
                # Normalize to full range for input between 0 and 1.
                axis_val = normalize_trigger_val(btn_val)
            
            return get_axis_event(axis_num, axis_val)

        # Perform button mapping.
        mapped_btn = self.config["mapping"]["btns"].get(btn_num, btn_num)
        if mapped_btn >= len(self.config["btn_map"]):
            logger.error("cannot send button num %d, max num buttons is %d" % (
                mapped_btn, len(self.config["btn_map"]) - 1))
            return None
        
        return get_btn_event(mapped_btn, int(btn_val))

    def get_mapped_axis(self, axis_num, axis_val):
        mapped_axis = self.config["mapping"]["axes"].get(axis_num, axis_num)
        if mapped_axis >= len(self.config["axes_map"]):
            logger.error("cannot send axis %d, max axis num is %d" %
                         (mapped_axis, len(self.config["axes_map"]) - 1))
            return None

        # Normalize axis value to be within range.
        return get_axis_event(mapped_axis, normalize_axis_val(axis_val))
