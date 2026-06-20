# Xlib.support.connect -- OS-independent display connection functions
#
#    Copyright (C) 2000 Peter Liljenberg <petli@ctrl-c.liu.se>
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

import sys
import importlib

# List the modules which contain the corresponding functions

_display_mods = {
    'OpenVMS': 'vms_connect',
    }

_default_display_mod = 'unix_connect'

_socket_mods = {
    'OpenVMS': 'vms_connect'
    }

_default_socket_mod = 'unix_connect'

_auth_mods = {
    'OpenVMS': 'vms_connect'
    }

_default_auth_mod = 'unix_connect'


# Figure out which OS we're using.
# sys.platform is either "OS-ARCH" or just "OS".

_parts = sys.platform.split('-')
platform = _parts[0]
del _parts


def _relative_import(modname):
    return importlib.import_module('..' + modname, __name__)


def get_display(display):
    """dname, protocol, host, dno, screen = get_display(display)

    Parse DISPLAY into its components.  If DISPLAY is None, use
    the default display.  The return values are:

      DNAME    -- the full display name (string)
      PROTOCOL -- the protocol to use (None if automatic)
      HOST     -- the host name (string, possibly empty)
      DNO      -- display number (integer)
      SCREEN   -- default screen number (integer)
    """

    modname = _display_mods.get(platform, _default_display_mod)
    mod = _relative_import(modname)
    return mod.get_display(display)


def get_socket(dname, protocol, host, dno):
    """socket = get_socket(dname, protocol, host, dno)

    Connect to the display specified by DNAME, PROTOCOL, HOST and DNO, which
    are the corresponding values from a previous call to get_display().

    Return SOCKET, a new socket object connected to the X server.
    """

    modname = _socket_mods.get(platform, _default_socket_mod)
    mod = _relative_import(modname)
    return mod.get_socket(dname, protocol, host, dno)


def get_auth(sock, dname, protocol, host, dno):
    """auth_name, auth_data = get_auth(sock, dname, protocol, host, dno)

    Return authentication data for the display on the other side of
    SOCK, which was opened with DNAME, HOST and DNO, using PROTOCOL.

    Return AUTH_NAME and AUTH_DATA, two strings to be used in the
    connection setup request.
    """

    modname = _auth_mods.get(platform, _default_auth_mod)
    mod = _relative_import(modname)
    return mod.get_auth(sock, dname, protocol, host, dno)
