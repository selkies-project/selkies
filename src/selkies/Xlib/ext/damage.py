# Xlib.ext.damage -- DAMAGE extension module
#
#    Copyright (C) 2018 Joseph Kogut <joseph.kogut@gmail.com>
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


from .. import X
from ..protocol import rq, structs
from ..error import XError

extname = 'DAMAGE'

# Event codes #
DamageNotifyCode = 0

# Error codes #
BadDamageCode = 0

class BadDamageError(XError):
    pass

# DamageReportLevel options
DamageReportRawRectangles = 0
DamageReportDeltaRectangles = 1
DamageReportBoundingBox = 2
DamageReportNonEmpty = 3

DamageReportLevel = (
    DamageReportRawRectangles,
    DamageReportDeltaRectangles,
    DamageReportBoundingBox,
    DamageReportNonEmpty,
)

DAMAGE = rq.Card32

# Methods

class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(0),
                         rq.RequestLength(),
                         rq.Card32('major_version'),
                         rq.Card32('minor_version'),
                         )

    _reply = rq.Struct(rq.ReplyCode(),
                       rq.Pad(1),
                       rq.Card16('sequence_number'),
                       rq.ReplyLength(),
                       rq.Card32('major_version'),
                       rq.Card32('minor_version'),
                       rq.Pad(16),
                       )

def query_version(self):
    return QueryVersion(display=self.display,
                        opcode=self.display.get_extension_major(extname),
                        major_version=1,
                        minor_version=1)

class DamageCreate(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(1),
                         rq.RequestLength(),
                         DAMAGE('damage'),
                         rq.Drawable('drawable'),
                         rq.Set('level', 1, DamageReportLevel),
                         rq.Pad(3),
                         )

def damage_create(self, level):
    did = self.display.allocate_resource_id()
    DamageCreate(display=self.display,
                 opcode=self.display.get_extension_major(extname),
                 damage=did,
                 drawable=self.id,
                 level=level,
                 )
    return did

class DamageDestroy(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(2),
                         rq.RequestLength(),
                         DAMAGE('damage')
                         )

def damage_destroy(self, damage):
    DamageDestroy(display=self.display,
                  opcode=self.display.get_extension_major(extname),
                  damage=damage,
                  )

    self.display.free_resource_id(damage)

class DamageSubtract(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(3),
                         rq.RequestLength(),
                         DAMAGE('damage'),
                         rq.Card32('repair'),
                         rq.Card32('parts')
                         )

def damage_subtract(self, damage, repair=X.NONE, parts=X.NONE):
    DamageSubtract(display=self.display,
                   opcode=self.display.get_extension_major(extname),
                   damage=damage,
                   repair=repair,
                   parts=parts)

class DamageAdd(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(4),
                         rq.RequestLength(),
                         rq.Card32('repair'),
                         rq.Card32('parts'),
                         )

def damage_add(self, repair, parts):
    DamageAdd(display=self.display,
              opcode=self.display.get_extension_major(extname),
              repair=repair,
              parts=parts)

# Events #

class DamageNotify(rq.Event):
    _code = None
    _fields = rq.Struct(
        rq.Card8('type'),
        rq.Card8('level'),
        rq.Card16('sequence_number'),
        rq.Drawable('drawable'),
        DAMAGE('damage'),
        rq.Card32('timestamp'),
        rq.Object('area', structs.Rectangle),
        rq.Object('drawable_geometry', structs.Rectangle)
        )

def init(disp, info):
    disp.extension_add_method('display',
                              'damage_query_version',
                              query_version)

    disp.extension_add_method('drawable',
                              'damage_create',
                              damage_create)

    disp.extension_add_method('display',
                              'damage_destroy',
                              damage_destroy)

    disp.extension_add_method('display',
                              'damage_subtract',
                              damage_subtract)

    disp.extension_add_method('drawable',
                              'damage_add',
                              damage_add)

    disp.extension_add_event(info.first_event + DamageNotifyCode, DamageNotify)

    disp.extension_add_error(code=BadDamageCode, err=BadDamageError)
