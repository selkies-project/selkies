# Xlib.ext.security -- SECURITY extension module
#
#    Copyright (C) 2010-2013 Outpost Embedded, LLC
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
A partial implementation of the SECURITY extension.  Support for the
SecurityAuthorizationRevoked event is not implemented.
'''

from ..protocol import rq


extname = 'SECURITY'


SecurityClientTrusted = 0
SecurityClientUntrusted = 1

SecurityAuthorizationRevokedMask = 1


AUTHID = rq.Card32


class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(0),
                         rq.RequestLength(),
                         rq.Card16('major_version'),
                         rq.Card16('minor_version')
                         )
    _reply = rq.Struct(rq.ReplyCode(),
                       rq.Pad(1),
                       rq.Card16('sequence_number'),
                       rq.ReplyLength(),
                       rq.Card16('major_version'),
                       rq.Card16('minor_version'),
                       rq.Pad(20)
                       )


def query_version(self):
    return QueryVersion(display=self.display,
                        opcode=self.display.get_extension_major(extname),
                        major_version=1,
                        minor_version=0)


class SecurityGenerateAuthorization(rq.ReplyRequest):
    # The order of fields here does not match the specifications I've seen
    # online, but it *does* match with the X.org implementation.  I guess the
    # spec is out-of-date.
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(1),
                         rq.RequestLength(),
                         rq.LengthOf('auth_proto', 2),
                         rq.LengthOf('auth_data', 2),
                         rq.Card32('value_mask'),
                         rq.String8('auth_proto'),
                         rq.Binary('auth_data'),
                         rq.List('values', rq.Card32Obj)
                         )
    _reply = rq.Struct(rq.ReplyCode(),
                       rq.Pad(1),
                       rq.Card16('sequence_number'),
                       rq.ReplyLength(),
                       AUTHID('authid'),
                       rq.LengthOf('auth_data_return', 2),
                       rq.Pad(18),
                       rq.Binary('auth_data_return')
                       )


def generate_authorization(self, auth_proto, auth_data=b'', timeout=None,
                           trust_level=None, group=None, event_mask=None):
    value_mask = 0
    values = []
    if timeout is not None:
        value_mask |= 1
        values.append(timeout)
    if trust_level is not None:
        value_mask |= 2
        values.append(trust_level)
    if group is not None:
        value_mask |= 4
        values.append(group)
    if event_mask is not None:
        value_mask |= 8
        values.append(event_mask)
    return SecurityGenerateAuthorization(display=self.display,
                                         opcode=self.display.get_extension_major(extname),
                                         value_mask=value_mask,
                                         auth_proto=auth_proto,
                                         auth_data=auth_data,
                                         values=values)


class SecurityRevokeAuthorization(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'),
                         rq.Opcode(2),
                         rq.RequestLength(),
                         AUTHID('authid')
                         )


def revoke_authorization(self, authid):
    return SecurityRevokeAuthorization(display=self.display,
                                       opcode=self.display.get_extension_major(extname),
                                       authid=authid)


def init(disp, info):
    disp.extension_add_method('display',
                              'security_query_version',
                              query_version)
    disp.extension_add_method('display',
                              'security_generate_authorization',
                              generate_authorization)
    disp.extension_add_method('display',
                              'security_revoke_authorization',
                              revoke_authorization)
