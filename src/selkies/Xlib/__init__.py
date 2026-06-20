# Xlib.__init__ -- glue for Xlib package
#
#    Copyright (C) 2000-2002 Peter Liljenberg <petli@ctrl-c.liu.se>
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

__version__ = (0, 33)

__version_extra__ = ''

__version_string__ = '.'.join(map(str, __version__)) + __version_extra__

__all__ = [
    'X',
    'XK',
    'Xatom',
    'Xcursorfont',
    'Xutil',
    'display',
    'error',
    'rdb',
    # Explicitly exclude threaded, so that it isn't imported by
    #  from Xlib import *
    ]
