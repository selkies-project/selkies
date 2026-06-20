# Xlib.ext.res -- X-Resource extension module
#
#    Copyright (C) 2021 Aleksei Bavshin <alebastr89@gmail.com>
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

"""X-Resource extension allows a client to query the X server about its usage
of various resources.

For detailed description see any of the following documents.
Protocol specification:
    https://www.x.org/releases/current/doc/resourceproto/resproto.txt
XCB Protocol specification:
    https://cgit.freedesktop.org/xcb/proto/tree/src/res.xml
"""
from ..protocol import rq

RES_MAJOR_VERSION = 1
RES_MINOR_VERSION = 2

extname = "X-Resource"

# v1.0
ResQueryVersion = 0
ResQueryClients = 1
ResQueryClientResources = 2
ResQueryClientPixmapBytes = 3
# v1.2
ResQueryClientIds = 4
ResQueryResourceBytes = 5


class QueryVersion(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryVersion),
            rq.RequestLength(),
            rq.Card8("client_major"),
            rq.Card8("client_minor"),
            rq.Pad(2))
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.Card16("server_major"),
            rq.Card16("server_minor"),
            rq.Pad(20))


def query_version(self, client_major=RES_MAJOR_VERSION,
                  client_minor=RES_MINOR_VERSION):
    """ Query the protocol version supported by the X server.

    The client sends the highest supported version to the server and the
    server sends the highest version it supports, but no higher than the
    requested version."""
    return QueryVersion(
            display=self.display,
            opcode=self.display.get_extension_major(extname),
            client_major=client_major,
            client_minor=client_minor)


Client = rq.Struct(
        rq.Card32("resource_base"),
        rq.Card32("resource_mask"))


class QueryClients(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryClients),
            rq.RequestLength())
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.LengthOf("clients", 4),
            rq.Pad(20),
            rq.List("clients", Client))


def query_clients(self):
    """Request the list of all currently connected clients."""
    return QueryClients(
            display=self.display,
            opcode=self.display.get_extension_major(extname))


Type = rq.Struct(
        rq.Card32("resource_type"),
        rq.Card32("count"))


class QueryClientResources(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryClientResources),
            rq.RequestLength(),
            rq.Card32("client"))
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.LengthOf("types", 4),
            rq.Pad(20),
            rq.List("types", Type))


def query_client_resources(self, client):
    """Request the number of resources owned by a client.

    The server will return the counts of each type of resource.
    """
    return QueryClientResources(
            display=self.display,
            opcode=self.display.get_extension_major(extname),
            client=client)


class QueryClientPixmapBytes(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryClientPixmapBytes),
            rq.RequestLength(),
            rq.Card32("client"))
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.Card32("bytes"),
            rq.Card32("bytes_overflow"),
            rq.Pad(16))


def query_client_pixmap_bytes(self, client):
    """Query the pixmap usage of some client.

    The returned number is a sum of memory usage of each pixmap that can be
    attributed to the given client.
    """
    return QueryClientPixmapBytes(
            display=self.display,
            opcode=self.display.get_extension_major(extname),
            client=client)


class SizeOf(rq.LengthOf):
    """A SizeOf stores the size in bytes of some other Field whose size
    may vary, e.g. List
    """
    def __init__(self, name, size, item_size):
        rq.LengthOf.__init__(self, name, size)
        self.item_size = item_size

    def parse_value(self, length, display):
        return length // self.item_size


ClientXIDMask = 1 << 0
LocalClientPIDMask = 1 << 1


ClientIdSpec = rq.Struct(
        rq.Card32("client"),
        rq.Card32("mask"))


ClientIdValue = rq.Struct(
        rq.Object("spec", ClientIdSpec),
        SizeOf("value", 4, 4),
        rq.List("value", rq.Card32Obj))


class QueryClientIds(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryClientIds),
            rq.RequestLength(),
            rq.LengthOf("specs", 4),
            rq.List("specs", ClientIdSpec))
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.LengthOf("ids", 4),
            rq.Pad(20),
            rq.List("ids", ClientIdValue))


def query_client_ids(self, specs):
    """Request to identify a given set of clients with some identification method.

    The request sends a list of specifiers that select clients and
    identification methods to server. The server then tries to identify the
    chosen clients using the identification methods specified for each client.
    The server returns IDs for those clients that were successfully identified.
    """
    return QueryClientIds(
            display=self.display,
            opcode=self.display.get_extension_major(extname),
            specs=specs)


ResourceIdSpec = rq.Struct(
        rq.Card32("resource"),
        rq.Card32("type"))


ResourceSizeSpec = rq.Struct(
        # inline struct ResourceIdSpec to work around
        # a parser bug with nested objects
        rq.Card32("resource"),
        rq.Card32("type"),
        rq.Card32("bytes"),
        rq.Card32("ref_count"),
        rq.Card32("use_count"))


ResourceSizeValue = rq.Struct(
        rq.Object("size", ResourceSizeSpec),
        rq.LengthOf("cross_references", 4),
        rq.List("cross_references", ResourceSizeSpec))


class QueryResourceBytes(rq.ReplyRequest):
    _request = rq.Struct(
            rq.Card8("opcode"),
            rq.Opcode(ResQueryResourceBytes),
            rq.RequestLength(),
            rq.Card32("client"),
            rq.LengthOf("specs", 4),
            rq.List("specs", ResourceIdSpec))
    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16("sequence_number"),
            rq.ReplyLength(),
            rq.LengthOf("sizes", 4),
            rq.Pad(20),
            rq.List("sizes", ResourceSizeValue))


def query_resource_bytes(self, client, specs):
    """Query the sizes of resources from X server.

    The request sends a list of specifiers that selects resources for size
    calculation. The server tries to calculate the sizes of chosen resources
    and returns an estimate for a resource only if the size could be determined
    """
    return QueryResourceBytes(
            display=self.display,
            opcode=self.display.get_extension_major(extname),
            client=client,
            specs=specs)


def init(disp, info):
    disp.extension_add_method("display", "res_query_version", query_version)
    disp.extension_add_method("display", "res_query_clients", query_clients)
    disp.extension_add_method("display", "res_query_client_resources",
                              query_client_resources)
    disp.extension_add_method("display", "res_query_client_pixmap_bytes",
                              query_client_pixmap_bytes)
    disp.extension_add_method("display", "res_query_client_ids",
                              query_client_ids)
    disp.extension_add_method("display", "res_query_resource_bytes",
                              query_resource_bytes)
