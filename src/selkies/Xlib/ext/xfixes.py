# Xlib.ext.xfixes -- XFIXES extension module
#
#    Copyright (C) 2010-2011 Outpost Embedded, LLC
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
A partial implementation of the XFIXES extension.  Only the HideCursor and
ShowCursor requests and SelectionNotify events are provided.
'''

from ..protocol import rq

extname = 'XFIXES'

XFixesSelectionNotify                   = 0
XFixesCursorNotify                      = 1

XFixesSetSelectionOwnerNotifyMask       = (1 << 0)
XFixesSelectionWindowDestroyNotifyMask  = (1 << 1)
XFixesSelectionClientCloseNotifyMask    = (1 << 2)
XFixesDisplayCursorNotifyMask           = (1 << 0)

XFixesSetSelectionOwnerNotify           = 0
XFixesSelectionWindowDestroyNotify      = 1
XFixesSelectionClientCloseNotify        = 2
XFixesDisplayCursorNotify               = 0

class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(0),
                         rq.RequestLength(),
                         rq.Card32('major_version'),
                         rq.Card32('minor_version')
                         )
    _reply = rq.Struct(rq.ReplyCode(),
                       rq.Pad(1),
                       rq.Card16('sequence_number'),
                       rq.ReplyLength(),
                       rq.Card32('major_version'),
                       rq.Card32('minor_version'),
                       rq.Pad(16)
                       )


def query_version(self):
    return QueryVersion(display=self.display,
                        opcode=self.display.get_extension_major(extname),
                        major_version=4,
                        minor_version=0)


class HideCursor(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(29),
                         rq.RequestLength(),
                         rq.Window('window')
                         )

def hide_cursor(self):
    HideCursor(display=self.display,
               opcode=self.display.get_extension_major(extname),
               window=self)


class ShowCursor(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(30),
                         rq.RequestLength(),
                         rq.Window('window')
                         )


def show_cursor(self):
    ShowCursor(display=self.display,
               opcode=self.display.get_extension_major(extname),
               window=self)

class SelectSelectionInput(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(2),
                         rq.RequestLength(),
                         rq.Window('window'),
                         rq.Card32('selection'),
                         rq.Card32('mask')
                         )

def select_selection_input(self, window, selection, mask):
    return SelectSelectionInput(opcode=self.display.get_extension_major(extname),
                                display=self.display,
                                window=window,
                                selection=selection,
                                mask=mask)


class SelectionNotify(rq.Event):
    _code = None
    _fields = rq.Struct(rq.Card8('type'),
                        rq.Card8('sub_code'),
                        rq.Card16('sequence_number'),
                        rq.Window('window'),
                        rq.Window('owner'),
                        rq.Card32('selection'),
                        rq.Card32('timestamp'),
                        rq.Card32('selection_timestamp'),
                        rq.Pad(8))


class SetSelectionOwnerNotify(SelectionNotify):
    pass


class SelectionWindowDestroyNotify(SelectionNotify):
    pass


class SelectionClientCloseNotify(SelectionNotify):
    pass


class SelectCursorInput(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(3),
                         rq.RequestLength(),
                         rq.Window('window'),
                         rq.Card32('mask')
                         )

def select_cursor_input(self, window, mask):
    return SelectCursorInput(opcode=self.display.get_extension_major(extname),
                             display=self.display,
                             window=window,
                             cursor_serial=0,
                             mask=mask)


class GetCursorImage(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(4),
                         rq.RequestLength()
                         )
    _reply = rq.Struct(rq.ReplyCode(),
                       rq.Pad(1),
                       rq.Card16('sequence_number'),
                       rq.ReplyLength(),
                       rq.Int16('x'),
                       rq.Int16('y'),
                       rq.Card16('width'),
                       rq.Card16('height'),
                       rq.Card16('xhot'),
                       rq.Card16('yhot'),
                       rq.Card32('cursor_serial'),
                       rq.Pad(8),
                       rq.List('cursor_image', rq.Card32)
                       )

def get_cursor_image(self, window):
    return GetCursorImage(opcode=self.display.get_extension_major(extname),
                          display=self.display,
                         )


class DisplayCursorNotify(rq.Event):
    _code = None
    _fields = rq.Struct(rq.Card8('type'),
                        rq.Card8('sub_code'),
                        rq.Card16('sequence_number'),
                        rq.Window('window'),
                        rq.Card32('cursor_serial'),
                        rq.Card32('timestamp'))


def init(disp, info):
    disp.extension_add_method('display', 'xfixes_select_selection_input', select_selection_input)
    disp.extension_add_method('display', 'xfixes_query_version', query_version)
    disp.extension_add_method('window', 'xfixes_hide_cursor', hide_cursor)
    disp.extension_add_method('window', 'xfixes_show_cursor', show_cursor)
    disp.extension_add_method('display', 'xfixes_select_cursor_input', select_cursor_input)
    disp.extension_add_method('display', 'xfixes_get_cursor_image', get_cursor_image)

    disp.extension_add_subevent(info.first_event + XFixesSelectionNotify, XFixesSetSelectionOwnerNotify, SetSelectionOwnerNotify)
    disp.extension_add_subevent(info.first_event + XFixesSelectionNotify, XFixesSelectionWindowDestroyNotify, SelectionWindowDestroyNotify)
    disp.extension_add_subevent(info.first_event + XFixesSelectionNotify, XFixesSelectionClientCloseNotify, SelectionClientCloseNotify)
    disp.extension_add_subevent(info.first_event + XFixesCursorNotify, XFixesDisplayCursorNotify, DisplayCursorNotify)
