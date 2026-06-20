# $Id: xtest.py,v 1.1 2000/08/21 10:03:45 petli Exp $
#
# Xlib.ext.composite -- Composite extension module
#
#    Copyright (C) 2007 Peter Liljenberg <peter.liljenberg@gmail.com>
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

"""Composite extension, allowing windows to be rendered to off-screen
storage.

For detailed description, see the protocol specification at
http://freedesktop.org/wiki/Software/CompositeExt

By itself this extension is not very useful, it is intended to be used
together with the DAMAGE and XFIXES extensions.  Typically you would
also need RENDER or glX or some similar method of creating fancy
graphics.
"""

from ..protocol import rq
from ..xobject import drawable

extname = 'Composite'

RedirectAutomatic = 0
RedirectManual = 1

class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8('opcode'),
            rq.Opcode(0),
            rq.RequestLength(),
            rq.Card32('major_version'),
            rq.Card32('minor_version')
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
    return QueryVersion(
        display = self.display,
        opcode = self.display.get_extension_major(extname),
        major_version=0,
        minor_version=4
        )


class RedirectWindow(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(1),
        rq.RequestLength(),
        rq.Window('window'),
        rq.Set('update', 1, (RedirectAutomatic, RedirectManual)),
        rq.Pad(3),
        )

def redirect_window(self, update, onerror = None):
    """Redirect the hierarchy starting at this window to off-screen
    storage.
    """
    RedirectWindow(display = self.display,
                   onerror = onerror,
                   opcode = self.display.get_extension_major(extname),
                   window = self,
                   update = update,
                   )


class RedirectSubwindows(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(2),
        rq.RequestLength(),
        rq.Window('window'),
        rq.Set('update', 1, (RedirectAutomatic, RedirectManual)),
        rq.Pad(3),
        )

def redirect_subwindows(self, update, onerror = None):
    """Redirect the hierarchies starting at all current and future
    children to this window to off-screen storage.
    """
    RedirectSubwindows(display = self.display,
                       onerror = onerror,
                       opcode = self.display.get_extension_major(extname),
                       window = self,
                       update = update,
                       )


class UnredirectWindow(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(3),
        rq.RequestLength(),
        rq.Window('window'),
        rq.Set('update', 1, (RedirectAutomatic, RedirectManual)),
        rq.Pad(3),
        )

def unredirect_window(self, update, onerror = None):
    """Stop redirecting this window hierarchy.
    """
    UnredirectWindow(display = self.display,
                     onerror = onerror,
                     opcode = self.display.get_extension_major(extname),
                     window = self,
                     update = update,
                     )


class UnredirectSubindows(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(4),
        rq.RequestLength(),
        rq.Window('window'),
        rq.Set('update', 1, (RedirectAutomatic, RedirectManual)),
        rq.Pad(3),
        )

def unredirect_subwindows(self, update, onerror = None):
    """Stop redirecting the hierarchies of children to this window.
    """
    RedirectWindow(display = self.display,
                   onerror = onerror,
                   opcode = self.display.get_extension_major(extname),
                   window = self,
                   update = update,
                   )


class CreateRegionFromBorderClip(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(5),
        rq.RequestLength(),
        rq.Card32('region'), # Protocol note: a Region from the XFIXES extension would be the exact type.
        rq.Window('window'),
        )

def create_region_from_border_clip(self, onerror = None):
    """Create a region of the border clip of the window, i.e. the area
    that is not clipped by the parent and any sibling windows.
    """

    rid = self.display.allocate_resource_id()
    CreateRegionFromBorderClip(
        display = self.display,
        onerror = onerror,
        opcode = self.display.get_extension_major(extname),
        region = rid,
        window = self,
        )

    # Protocol note: could wrap the reply in a Region object.
    return rid


class NameWindowPixmap(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(6),
        rq.RequestLength(),
        rq.Window('window'),
        rq.Pixmap('pixmap'),
        )

def name_window_pixmap(self, onerror = None):
    """Create a new pixmap that refers to the off-screen storage of
    the window, including its border.

    This pixmap will remain allocated until freed whatever happens
    with the window.  However, the window will get a new off-screen
    pixmap every time it is mapped or resized, so to keep track of the
    contents you must listen for these events and get a new pixmap
    after them.
    """

    pid = self.display.allocate_resource_id()
    NameWindowPixmap(display = self.display,
                     onerror = onerror,
                     opcode = self.display.get_extension_major(extname),
                     window = self,
                     pixmap = pid,
                     )

    cls = self.display.get_resource_class('pixmap', drawable.Pixmap)
    return cls(self.display, pid, owner = 1)

class GetOverlayWindow(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(7),
        rq.RequestLength(),
        rq.Window('window')
    )
    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Window('overlay_window'),
        rq.Pad(20),
    )

def get_overlay_window(self):
    """Return the overlay window of the root window.
    """

    return GetOverlayWindow(display = self.display,
                              opcode = self.display.get_extension_major(extname),
                              window = self)

def init(disp, info):
    disp.extension_add_method('display',
                              'composite_query_version',
                              query_version)

    disp.extension_add_method('window',
                              'composite_redirect_window',
                              redirect_window)

    disp.extension_add_method('window',
                              'composite_redirect_subwindows',
                              redirect_subwindows)

    disp.extension_add_method('window',
                              'composite_unredirect_window',
                              unredirect_window)

    disp.extension_add_method('window',
                              'composite_unredirect_subwindows',
                              unredirect_subwindows)

    disp.extension_add_method('window',
                              'composite_create_region_from_border_clip',
                              create_region_from_border_clip)

    disp.extension_add_method('window',
                              'composite_name_window_pixmap',
                              name_window_pixmap)

    disp.extension_add_method('window',
                              'composite_get_overlay_window',
                              get_overlay_window)
