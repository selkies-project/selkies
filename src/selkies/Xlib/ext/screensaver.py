# Xlib.ext.screensaver -- X ScreenSaver extension module
#
#    Copyright (C) 2022 Vladimir Panteleev <git@cy.md>
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
#    51 Franklin Street,
#    Fifth Floor,
#    Boston, MA 02110-1301 USA

"""This extension allows registering the client as an X screensaver,
or query information about the current screensaver.

For detailed description see any of the following documents.
Protocol specification:
    https://www.x.org/releases/X11R7.7/doc/scrnsaverproto/saver.html
XCB Protocol specification:
    https://cgit.freedesktop.org/xcb/proto/tree/src/screensaver.xml

"""

from .. import X
from ..protocol import rq, structs

extname = 'MIT-SCREEN-SAVER'

# Event members
NotifyMask = 1
CycleMask = 2

# Notify state
StateOff = 0
StateOn = 1
StateCycle = 2

# Notify kind
KindBlanked = 0
KindInternal = 1
KindExternal = 2

class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(0),
        rq.RequestLength(),
        rq.Card8('major_version'),
        rq.Card8('minor_version'),
        rq.Pad(2),
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
    return QueryVersion(display=self.display,
                        opcode=self.display.get_extension_major(extname),
                        major_version=1,
                        minor_version=0)


class QueryInfo(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(1),
        rq.RequestLength(),
        rq.Drawable('drawable'),
        )

    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Card8('state'),
            rq.Card16('sequence_number'),
            rq.ReplyLength(),
            rq.Window('saver_window'),
            rq.Card32('til_or_since'),
            rq.Card32('idle'),
            rq.Card32('event_mask'), # rq.Set('event_mask', 4, (NotifyMask, CycleMask)),
            rq.Card8('kind'),
            rq.Pad(7),
            )

def query_info(self):
    return QueryInfo(display=self.display,
                     opcode=self.display.get_extension_major(extname),
                     drawable=self,
                     )


class SelectInput(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(2),
        rq.RequestLength(),
        rq.Drawable('drawable'),
        rq.Card32('event_mask'), # rq.Set('event_mask', 4, (NotifyMask, CycleMask)),
        )

def select_input(self, mask):
    return SelectInput(display=self.display,
                       opcode=self.display.get_extension_major(extname),
                       drawable=self,
                       event_mask=mask,
                       )


class SetAttributes(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(3),
        rq.RequestLength(),
        rq.Drawable('drawable'),
        rq.Int16('x'),
        rq.Int16('y'),
        rq.Card16('width'),
        rq.Card16('height'),
        rq.Card16('border_width'),
        rq.Set('window_class', 1, (X.CopyFromParent, X.InputOutput, X.InputOnly)),
        rq.Card8('depth'),
        rq.Card32('visual'),
        structs.WindowValues('attrs'),
        )

def set_attributes(self, x, y, width, height, border_width,
                   window_class = X.CopyFromParent,
                   depth = X.CopyFromParent,
                   visual = X.CopyFromParent,
                   onerror = None,
                   **keys):
    return SetAttributes(display=self.display,
                         onerror = onerror,
                         opcode=self.display.get_extension_major(extname),
                         drawable=self,
                         x = x,
                         y = y,
                         width = width,
                         height = height,
                         border_width = border_width,
                         window_class = window_class,
                         depth = depth,
                         visual = visual,
                         attrs = keys)


class UnsetAttributes(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(4),
        rq.RequestLength(),
        rq.Drawable('drawable'),
        )

def unset_attributes(self, onerror = None):
    return UnsetAttributes(display=self.display,
                           onerror = onerror,
                           opcode=self.display.get_extension_major(extname),
                           drawable=self)


class Notify(rq.Event):
    _code = None
    _fields = rq.Struct(
        rq.Card8('type'),
        rq.Set('state', 1, (StateOff, StateOn, StateCycle)),
        rq.Card16('sequence_number'),
        rq.Card32('timestamp'),
        rq.Window('root'),
        rq.Window('window'),
        rq.Set('kind', 1, (KindBlanked, KindInternal, KindExternal)),
        rq.Bool('forced'),
        rq.Pad(14),
        )

def init(disp, info):
    disp.extension_add_method('display', 'screensaver_query_version', query_version)
    disp.extension_add_method('drawable', 'screensaver_query_info', query_info)
    disp.extension_add_method('drawable', 'screensaver_select_input', select_input)
    disp.extension_add_method('drawable', 'screensaver_set_attributes', set_attributes)
    disp.extension_add_method('drawable', 'screensaver_unset_attributes', unset_attributes)

    disp.extension_add_event(info.first_event + 0, Notify)
