# Xlib.ext.ge -- Generic Event extension module
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
ge - Generic Event Extension
'''

from ..protocol import rq

extname = 'Generic Event Extension'


GenericEventCode = 35


class GEQueryVersion(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(0),
        rq.RequestLength(),
        rq.Card32('major_version'),
        rq.Card32('minor_version'),
        )
    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card32('major_version'),
        rq.Card32('minor_version'),
        rq.Pad(16),
        )


def query_version(self):
    return GEQueryVersion(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        major_version=1,
        minor_version=0,
        )


class GenericEvent(rq.Event):
    _code = GenericEventCode
    _fields = rq.Struct(
        rq.Card8('type'),
        rq.Card8('extension'),
        rq.Card16('sequence_number'),
        rq.Card32('length'),
        rq.Card16('evtype'),
        # Some generic events make use of this space, but with
        # others the data is simply discarded.  In any case we
        # don't need to explicitly pad this out as we are
        # always given at least 32 bytes and we save
        # everything after the first ten as the "data" field.
        #rq.Pad(22),
        )

    def __init__(self, binarydata = None, display = None, **keys):
        if binarydata:
            data = binarydata[10:]
            binarydata = binarydata[:10]
        else:
            data = ''

        rq.Event.__init__(
            self,
            binarydata=binarydata,
            display=display,
            **keys
            )

        if display:
            ge_event_data = getattr(display, 'ge_event_data', None)
            if ge_event_data:
                estruct = ge_event_data.get((self.extension, self.evtype), None)
                if estruct:
                    data, _ = estruct.parse_binary(data, display)

        self._data['data'] = data


def add_event_data(self, extension, evtype, estruct):
    if not hasattr(self.display, 'ge_event_data'):
        self.display.ge_event_data = {}
    self.display.ge_event_data[(extension, evtype)] = estruct


def init(disp, info):
    disp.extension_add_method('display', 'ge_query_version', query_version)
    disp.extension_add_method('display', 'ge_add_event_data', add_event_data)
    disp.extension_add_event(GenericEventCode, GenericEvent)
