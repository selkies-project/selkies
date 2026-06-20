# Xlib.ext.xinput -- XInput extension module
#
#    Copyright (C) 2012 Outpost Embedded, LLC
#      Forest Bond <forest.bond@rapidrollout.com>
#
# This library is free software; you can redistribute it and/or
# modify it under the terms of the GNU Lesser General Public License
# as published by the Free Software Foundation; either version 2.1
# of the License, or (at your option) any later version.
#
# This library is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
# See the GNU Lesser General Public License for more details.
#
# You should have received a copy of the GNU Lesser General Public
# License along with this library; if not, write to the
#    Free Software Foundation, Inc.,
#    59 Temple Place,
#    Suite 330,
#    Boston, MA 02111-1307 USA

'''
A very incomplete implementation of the XInput extension.
'''

import sys
import array
import struct

# Python 2/3 compatibility.
from six import integer_types

from ..protocol import rq
from .. import X


extname = 'XInputExtension'

PropertyDeleted = 0
PropertyCreated = 1
PropertyModified = 2

NotifyNormal = 0
NotifyGrab = 1
NotifyUngrab = 2
NotifyWhileGrabbed = 3
NotifyPassiveGrab = 4
NotifyPassiveUngrab = 5

NotifyAncestor = 0
NotifyVirtual = 1
NotifyInferior = 2
NotifyNonlinear = 3
NotifyNonlinearVirtual = 4
NotifyPointer = 5
NotifyPointerRoot = 6
NotifyDetailNone = 7

GrabtypeButton = 0
GrabtypeKeycode = 1
GrabtypeEnter = 2
GrabtypeFocusIn = 3
GrabtypeTouchBegin = 4

AnyModifier = (1 << 31)
AnyButton = 0
AnyKeycode = 0

AsyncDevice = 0
SyncDevice = 1
ReplayDevice = 2
AsyncPairedDevice = 3
AsyncPair = 4
SyncPair = 5

SlaveSwitch = 1
DeviceChange = 2

MasterAdded = (1 << 0)
MasterRemoved = (1 << 1)
SlaveAdded = (1 << 2)
SlaveRemoved = (1 << 3)
SlaveAttached = (1 << 4)
SlaveDetached = (1 << 5)
DeviceEnabled = (1 << 6)
DeviceDisabled = (1 << 7)

AddMaster = 1
RemoveMaster = 2
AttachSlave = 3
DetachSlave = 4

AttachToMaster = 1
Floating = 2

ModeRelative = 0
ModeAbsolute = 1

MasterPointer = 1
MasterKeyboard = 2
SlavePointer = 3
SlaveKeyboard = 4
FloatingSlave = 5

KeyClass = 0
ButtonClass = 1
ValuatorClass = 2
ScrollClass = 3
TouchClass = 8

KeyRepeat = (1 << 16)

AllDevices = 0
AllMasterDevices = 1

DeviceChanged = 1
KeyPress = 2
KeyRelease = 3
ButtonPress = 4
ButtonRelease = 5
Motion = 6
Enter = 7
Leave = 8
FocusIn = 9
FocusOut = 10
HierarchyChanged = 11
PropertyEvent = 12
RawKeyPress = 13
RawKeyRelease = 14
RawButtonPress = 15
RawButtonRelease = 16
RawMotion = 17

DeviceChangedMask = (1 << DeviceChanged)
KeyPressMask = (1 << KeyPress)
KeyReleaseMask = (1 << KeyRelease)
ButtonPressMask = (1 << ButtonPress)
ButtonReleaseMask = (1 << ButtonRelease)
MotionMask = (1 << Motion)
EnterMask = (1 << Enter)
LeaveMask = (1 << Leave)
FocusInMask = (1 << FocusIn)
FocusOutMask = (1 << FocusOut)
HierarchyChangedMask = (1 << HierarchyChanged)
PropertyEventMask = (1 << PropertyEvent)
RawKeyPressMask = (1 << RawKeyPress)
RawKeyReleaseMask = (1 << RawKeyRelease)
RawButtonPressMask = (1 << RawButtonPress)
RawButtonReleaseMask = (1 << RawButtonRelease)
RawMotionMask = (1 << RawMotion)

GrabModeSync = 0
GrabModeAsync = 1
GrabModeTouch = 2

DEVICEID = rq.Card16
DEVICE = rq.Card16
DEVICEUSE = rq.Card8

PROPERTY_TYPE_FLOAT = 'FLOAT'

class FP1616(rq.Int32):

    def check_value(self, value):
        return int(value * 65536.0)

    def parse_value(self, value, display):
        return float(value) / float(1 << 16)

class FP3232(rq.ValueField):
    structcode = 'lL'
    structvalues = 2

    def check_value(self, value):
        return value

    def parse_value(self, value, display):
        integral, frac = value
        ret = float(integral)
        # optimised math.ldexp(float(frac), -32)
        ret += float(frac) * (1.0 / (1 << 32))
        return ret

class XIQueryVersion(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(47),
        rq.RequestLength(),
        rq.Card16('major_version'),
        rq.Card16('minor_version'),
        )
    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card16('major_version'),
        rq.Card16('minor_version'),
        rq.Pad(20),
        )


def query_version(self):
    return XIQueryVersion(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        major_version=2,
        minor_version=0,
        )

class Mask(rq.List):

    def __init__(self, name):
        rq.List.__init__(self, name, rq.Card32, pad=0)

    def pack_value(self, val):

        mask_seq = array.array(rq.struct_to_array_codes['L'])

        if isinstance(val, integer_types):
            # We need to build a "binary mask" that (as far as I can tell) is
            # encoded in native byte order from end to end.  The simple case is
            # with a single unsigned 32-bit value, for which we construct an
            # array with just one item.  For values too big to fit inside 4
            # bytes we build a longer array, being careful to maintain native
            # byte order across the entire set of values.
            if sys.byteorder == 'little':
                def fun(val):
                    mask_seq.insert(0, val)
            elif sys.byteorder == 'big':
                fun = mask_seq.append
            else:
                raise AssertionError(sys.byteorder)
            while val:
                fun(val & 0xFFFFFFFF)
                val = val >> 32
        else:
            mask_seq.extend(val)

        return rq.encode_array(mask_seq), len(mask_seq), None

EventMask = rq.Struct(
    DEVICE('deviceid'),
    rq.LengthOf('mask', 2),
    Mask('mask'),
)


class XISelectEvents(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(46),
        rq.RequestLength(),
        rq.Window('window'),
        rq.LengthOf('masks', 2),
        rq.Pad(2),
        rq.List('masks', EventMask),
    )

def select_events(self, event_masks):
    '''
    select_events(event_masks)

    event_masks:
      Sequence of (deviceid, mask) pairs, where deviceid is a numerical device
      ID, or AllDevices or AllMasterDevices, and mask is either an unsigned
      integer or sequence of 32 bits unsigned values
    '''
    return XISelectEvents(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        window=self,
        masks=event_masks,
    )

AnyInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.Pad(2),
)

class ButtonMask(object):

    def __init__(self, value, length):
        self._value = value
        self._length = length

    def __len__(self):
        return self._length

    def __getitem__(self, key):
        return self._value & (1 << key)

    def __str__(self):
        return repr(self)

    def __repr__(self):
        return '0b{value:0{width}b}'.format(value=self._value,
                                            width=self._length)

class ButtonState(rq.ValueField):

    structcode = None

    def __init__(self, name):
        rq.ValueField.__init__(self, name)

    def parse_binary_value(self, data, display, length, fmt):
        # Mask: bitfield of <length> button states.
        mask_len = 4 * ((((length + 7) >> 3) + 3) >> 2)
        mask_data = data[:mask_len]
        mask_value = 0
        for byte in reversed(struct.unpack('={0:d}B'.format(mask_len), mask_data)):
            mask_value <<= 8
            mask_value |= byte
        data = data[mask_len:]
        assert (mask_value & 1) == 0
        return ButtonMask(mask_value >> 1, length), data

ButtonInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.LengthOf(('state', 'labels'), 2),
    ButtonState('state'),
    rq.List('labels', rq.Card32),
)

KeyInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.LengthOf('keycodes', 2),
    rq.List('keycodes', rq.Card32),
)

ValuatorInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.Card16('number'),
    rq.Card32('label'),
    FP3232('min'),
    FP3232('max'),
    FP3232('value'),
    rq.Card32('resolution'),
    rq.Card8('mode'),
    rq.Pad(3),
)

ScrollInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.Card16('number'),
    rq.Card16('scroll_type'),
    rq.Pad(2),
    rq.Card32('flags'),
    FP3232('increment'),
)

TouchInfo = rq.Struct(
    rq.Card16('type'),
    rq.Card16('length'),
    rq.Card16('sourceid'),
    rq.Card8('mode'),
    rq.Card8('num_touches'),
)

INFO_CLASSES = {
    KeyClass: KeyInfo,
    ButtonClass: ButtonInfo,
    ValuatorClass: ValuatorInfo,
    ScrollClass: ScrollInfo,
    TouchClass: TouchInfo,
}

class ClassInfoClass(object):

    structcode = None

    def parse_binary(self, data, display):
        class_type, length = struct.unpack('=HH', data[:4])
        class_struct = INFO_CLASSES.get(class_type, AnyInfo)
        class_data, _ = class_struct.parse_binary(data, display)
        data = data[length * 4:]
        return class_data, data

ClassInfo = ClassInfoClass()

DeviceInfo = rq.Struct(
    DEVICEID('deviceid'),
    rq.Card16('use'),
    rq.Card16('attachment'),
    rq.LengthOf('classes', 2),
    rq.LengthOf('name', 2),
    rq.Bool('enabled'),
    rq.Pad(1),
    rq.String8('name', 4),
    rq.List('classes', ClassInfo),
)

class XIQueryDevice(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(48),
        rq.RequestLength(),
        DEVICEID('deviceid'),
        rq.Pad(2),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.LengthOf('devices', 2),
        rq.Pad(22),
        rq.List('devices', DeviceInfo),
        )

def query_device(self, deviceid):
    return XIQueryDevice(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        )

class XIListProperties(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(56),
        rq.RequestLength(),
        DEVICEID('deviceid'),
        rq.Pad(2),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.LengthOf('atoms', 2),
        rq.Pad(22),
        rq.List('atoms', rq.Card32Obj),
    )

def list_device_properties(self, deviceid):
    return XIListProperties(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
    )

class XIGetProperty(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(59),
        rq.RequestLength(),
        DEVICEID('deviceid'),
        rq.Card8('delete'),
        rq.Pad(1),
        rq.Card32('property'),
        rq.Card32('type'),
        rq.Card32('offset'),
        rq.Card32('length'),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card32('type'),
        rq.Card32('bytes_after'),
        rq.LengthOf('value', 4),
        rq.Format('value', 1),
        rq.Pad(11),
        rq.PropertyData('value')
    )

def get_device_property(self, deviceid, property, type, offset, length, delete=False):
    return XIGetProperty(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        property=property,
        type=type,
        offset=offset,
        length=length,
        delete=delete,
    )

class XIChangeProperty(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(57),
        rq.RequestLength(),
        DEVICEID('deviceid'),
        rq.Card8('mode'),
        rq.Format('value', 1),
        rq.Card32('property'),
        rq.Card32('type'),
        rq.LengthOf('value', 4),
        rq.PropertyData('value'),
    )

def change_device_property(self, deviceid, property, type, mode, value):
    return XIChangeProperty(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        property=property,
        type=type,
        mode=mode,
        value=value,
    )

class XIDeleteProperty(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(58),
        rq.RequestLength(),
        DEVICEID('deviceid'),
        rq.Pad(2),
        rq.Card32('property'),
    )

def delete_device_property(self, deviceid, property):
    return XIDeleteProperty(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        property=property,
    )

class XIGrabDevice(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(51),
        rq.RequestLength(),
        rq.Window('grab_window'),
        rq.Card32('time'),
        rq.Cursor('cursor', (X.NONE, )),
        DEVICEID('deviceid'),
        rq.Set('grab_mode', 1, (GrabModeSync, GrabModeAsync)),
        rq.Set('paired_device_mode', 1, (GrabModeSync, GrabModeAsync)),
        rq.Bool('owner_events'),
        rq.Pad(1),
        rq.LengthOf('mask', 2),
        Mask('mask'),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card8('status'),
        rq.Pad(23),
        )

def grab_device(self, deviceid, time, grab_mode, paired_device_mode, owner_events, event_mask):
    return XIGrabDevice(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        grab_window=self,
        time=time,
        cursor=X.NONE,
        grab_mode=grab_mode,
        paired_device_mode=paired_device_mode,
        owner_events=owner_events,
        mask=event_mask,
        )

class XIUngrabDevice(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(52),
        rq.RequestLength(),
        rq.Card32('time'),
        DEVICEID('deviceid'),
        rq.Pad(2),
    )

def ungrab_device(self, deviceid, time):
    return XIUngrabDevice(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        time=time,
        deviceid=deviceid,
    )

class XIPassiveGrabDevice(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(54),
        rq.RequestLength(),
        rq.Card32('time'),
        rq.Window('grab_window'),
        rq.Cursor('cursor', (X.NONE, )),
        rq.Card32('detail'),
        DEVICEID('deviceid'),
        rq.LengthOf('modifiers', 2),
        rq.LengthOf('mask', 2),
        rq.Set('grab_type', 1, (GrabtypeButton, GrabtypeKeycode, GrabtypeEnter,
                                GrabtypeFocusIn, GrabtypeTouchBegin)),
        rq.Set('grab_mode', 1, (GrabModeSync, GrabModeAsync)),
        rq.Set('paired_device_mode', 1, (GrabModeSync, GrabModeAsync)),
        rq.Bool('owner_events'),
        rq.Pad(2),
        Mask('mask'),
        rq.List('modifiers', rq.Card32),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.LengthOf('modifiers', 2),
        rq.Pad(22),
        rq.List('modifiers', rq.Card32),
        )

def passive_grab_device(self, deviceid, time, detail,
                        grab_type, grab_mode, paired_device_mode,
                        owner_events, event_mask, modifiers):
    return XIPassiveGrabDevice(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        grab_window=self,
        time=time,
        cursor=X.NONE,
        detail=detail,
        grab_type=grab_type,
        grab_mode=grab_mode,
        paired_device_mode=paired_device_mode,
        owner_events=owner_events,
        mask=event_mask,
        modifiers=modifiers,
        )

def grab_keycode(self, deviceid, time, keycode,
                 grab_mode, paired_device_mode,
                 owner_events, event_mask, modifiers):
    return passive_grab_device(self, deviceid, time, keycode,
                               GrabtypeKeycode,
                               grab_mode, paired_device_mode,
                               owner_events, event_mask, modifiers)

class XIPassiveUngrabDevice(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(55),
        rq.RequestLength(),
        rq.Window('grab_window'),
        rq.Card32('detail'),
        DEVICEID('deviceid'),
        rq.LengthOf('modifiers', 2),
        rq.Set('grab_type', 1, (GrabtypeButton, GrabtypeKeycode,
                                GrabtypeEnter, GrabtypeFocusIn,
                                GrabtypeTouchBegin)),
        rq.Pad(3),
        rq.List('modifiers', rq.Card32),
    )

def passive_ungrab_device(self, deviceid, detail, grab_type, modifiers):
    return XIPassiveUngrabDevice(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        deviceid=deviceid,
        grab_window=self,
        detail=detail,
        grab_type=grab_type,
        modifiers=modifiers,
        )

def ungrab_keycode(self, deviceid, keycode, modifiers):
    return passive_ungrab_device(self, deviceid, keycode,
                                 GrabtypeKeycode, modifiers)

HierarchyInfo = rq.Struct(
    DEVICEID('deviceid'),
    DEVICEID('attachment'),
    DEVICEUSE('type'),
    rq.Bool('enabled'),
    rq.Pad(2),
    rq.Card32('flags'),
)


HierarchyEventData = rq.Struct(
    DEVICEID('deviceid'),
    rq.Card32('time'),
    rq.Card32('flags'),
    rq.LengthOf('info', 2),
    rq.Pad(10),
    rq.List('info', HierarchyInfo),
)

ModifierInfo = rq.Struct(
    rq.Card32('base_mods'),
    rq.Card32('latched_mods'),
    rq.Card32('locked_mods'),
    rq.Card32('effective_mods'),
)

GroupInfo = rq.Struct(
    rq.Card8('base_group'),
    rq.Card8('latched_group'),
    rq.Card8('locked_group'),
    rq.Card8('effective_group'),
)

DeviceEventData = rq.Struct(
    DEVICEID('deviceid'),
    rq.Card32('time'),
    rq.Card32('detail'),
    rq.Window('root'),
    rq.Window('event'),
    rq.Window('child'),
    FP1616('root_x'),
    FP1616('root_y'),
    FP1616('event_x'),
    FP1616('event_y'),
    rq.LengthOf('buttons', 2),
    rq.Card16('valulators_len'),
    DEVICEID('sourceid'),
    rq.Pad(2),
    rq.Card32('flags'),
    rq.Object('mods', ModifierInfo),
    rq.Object('groups', GroupInfo),
    ButtonState('buttons'),
)

DeviceChangedEventData = rq.Struct(
    DEVICEID('deviceid'),
    rq.Card32('time'),
    rq.LengthOf('classes', 2),
    DEVICEID('sourceid'),
    rq.Card8('reason'),
    rq.Pad(11),
    rq.List('classes', ClassInfo),
)

PropertyEventData = rq.Struct(
    DEVICEID('deviceid'),
    rq.Card32('time'),
    rq.Card32('property'),
    rq.Card8('what'),
    rq.Pad(11),
)

def init(disp, info):
    disp.extension_add_method('display', 'xinput_query_version', query_version)
    disp.extension_add_method('window', 'xinput_select_events', select_events)
    disp.extension_add_method('display', 'xinput_query_device', query_device)
    disp.extension_add_method('window', 'xinput_grab_device', grab_device)
    disp.extension_add_method('display', 'xinput_ungrab_device', ungrab_device)
    disp.extension_add_method('window', 'xinput_grab_keycode', grab_keycode)
    disp.extension_add_method('window', 'xinput_ungrab_keycode', ungrab_keycode)
    disp.extension_add_method('display', 'xinput_get_device_property', get_device_property)
    disp.extension_add_method('display', 'xinput_list_device_properties', list_device_properties)
    disp.extension_add_method('display', 'xinput_change_device_property', change_device_property)
    disp.extension_add_method('display', 'xinput_delete_device_property', delete_device_property)
    if hasattr(disp,"ge_add_event_data"):
        for device_event in (ButtonPress, ButtonRelease, KeyPress, KeyRelease, Motion):
            disp.ge_add_event_data(info.major_opcode, device_event, DeviceEventData)
        disp.ge_add_event_data(info.major_opcode, DeviceChanged, DeviceEventData)
        disp.ge_add_event_data(info.major_opcode, HierarchyChanged, HierarchyEventData)
        disp.ge_add_event_data(info.major_opcode, PropertyEvent, PropertyEventData)
