# Xlib.xobject.resource -- any X resource object
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

from ..protocol import request

class Resource(object):
    def __init__(self, display, rid, owner = 0):
        self.display = display
        self.id = rid
        self.owner = owner

    def __resource__(self):
        return self.id

    def __eq__(self, obj):
        if isinstance(obj, Resource):
            if self.display == obj.display:
                return self.id == obj.id
            else:
                return False
        else:
            return id(self) == id(obj)

    def __ne__(self, obj):
        return not self == obj

    def __hash__(self):
        return int(self.id)

    def __repr__(self):
        return '<%s 0x%08x>' % (self.__class__.__name__, self.id)

    def kill_client(self, onerror = None):
        request.KillClient(display = self.display,
                           onerror = onerror,
                           resource = self.id)
