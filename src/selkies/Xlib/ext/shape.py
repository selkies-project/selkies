# Automatically generated file; DO NOT EDIT.
# Generated from: /usr/share/xcb/shape.xml

from ..protocol import rq, structs


extname = 'SHAPE'

OP = rq.Card8

class SO:
    Set = 0
    Union = 1
    Intersect = 2
    Subtract = 3
    Invert = 4

class SK:
    Bounding = 0
    Clip = 1
    Input = 2

class KIND(rq.Set):

    def __init__(self, name):
        super(KIND, self).__init__(name, 1,
                                   values=(SK.Bounding,
                                           SK.Clip,
                                           SK.Input))

class NotifyEventData(rq.Event):
    _code = None
    _fields = rq.Struct(
        rq.Card8('type'),
        KIND('shape_kind'),
        rq.Card16('sequence_number'),
        rq.Window('affected_window'),
        rq.Int16('extents_x'),
        rq.Int16('extents_y'),
        rq.Card16('extents_width'),
        rq.Card16('extents_height'),
        rq.Card32('server_time'),
        rq.Card8('shaped'),
        rq.Pad(11),
    )

class QueryVersion(rq.ReplyRequest):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(0),
        rq.RequestLength(),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card16('major_version'),
        rq.Card16('minor_version'),
    )

class Rectangles(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(1),
        rq.RequestLength(),
        OP('operation'),
        KIND('destination_kind'),
        rq.Card8('ordering'),
        rq.Pad(1),
        rq.Window('destination_window'),
        rq.Int16('x_offset'),
        rq.Int16('y_offset'),
        rq.List('rectangles', structs.Rectangle, pad=0),
    )

class Mask(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(2),
        rq.RequestLength(),
        OP('operation'),
        KIND('destination_kind'),
        rq.Pad(2),
        rq.Window('destination_window'),
        rq.Int16('x_offset'),
        rq.Int16('y_offset'),
        rq.Pixmap('source_bitmap'),
    )

class Combine(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(3),
        rq.RequestLength(),
        OP('operation'),
        KIND('destination_kind'),
        KIND('source_kind'),
        rq.Pad(1),
        rq.Window('destination_window'),
        rq.Int16('x_offset'),
        rq.Int16('y_offset'),
        rq.Window('source_window'),
    )

class Offset(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(4),
        rq.RequestLength(),
        KIND('destination_kind'),
        rq.Pad(3),
        rq.Window('destination_window'),
        rq.Int16('x_offset'),
        rq.Int16('y_offset'),
    )

class QueryExtents(rq.ReplyRequest):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(5),
        rq.RequestLength(),
        rq.Window('destination_window'),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card8('bounding_shaped'),
        rq.Card8('clip_shaped'),
        rq.Pad(2),
        rq.Int16('bounding_shape_extents_x'),
        rq.Int16('bounding_shape_extents_y'),
        rq.Card16('bounding_shape_extents_width'),
        rq.Card16('bounding_shape_extents_height'),
        rq.Int16('clip_shape_extents_x'),
        rq.Int16('clip_shape_extents_y'),
        rq.Card16('clip_shape_extents_width'),
        rq.Card16('clip_shape_extents_height'),
    )

class SelectInput(rq.Request):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(6),
        rq.RequestLength(),
        rq.Window('destination_window'),
        rq.Card8('enable'),
        rq.Pad(3),
    )

class InputSelected(rq.ReplyRequest):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(7),
        rq.RequestLength(),
        rq.Window('destination_window'),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Card8('enabled'),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
    )

class GetRectangles(rq.ReplyRequest):

    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(8),
        rq.RequestLength(),
        rq.Window('window'),
        KIND('source_kind'),
        rq.Pad(3),
    )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Card8('ordering'),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.LengthOf('rectangles', 4),
        rq.Pad(20),
        rq.List('rectangles', structs.Rectangle, pad=0),
    )

class Event:
    # Sub events.
    Notify = 0

def combine(self, operation, destination_kind, source_kind, x_offset, y_offset):
    Combine(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        source_window=self,
        operation=operation,
        destination_kind=destination_kind,
        source_kind=source_kind,
        x_offset=x_offset,
        y_offset=y_offset,
    )

def get_rectangles(self, source_kind):
    return GetRectangles(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        window=self,
        source_kind=source_kind,
    )

def input_selected(self, ):
    return InputSelected(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
    )

def mask(self, operation, destination_kind, x_offset, y_offset, source_bitmap):
    Mask(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
        operation=operation,
        destination_kind=destination_kind,
        x_offset=x_offset,
        y_offset=y_offset,
        source_bitmap=source_bitmap,
    )

def offset(self, destination_kind, x_offset, y_offset):
    Offset(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
        destination_kind=destination_kind,
        x_offset=x_offset,
        y_offset=y_offset,
    )

def query_extents(self, ):
    return QueryExtents(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
    )

def query_version(self, ):
    return QueryVersion(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
    )

def rectangles(self, operation, destination_kind, ordering, x_offset, y_offset, rectangles):
    Rectangles(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
        operation=operation,
        destination_kind=destination_kind,
        ordering=ordering,
        x_offset=x_offset,
        y_offset=y_offset,
        rectangles=rectangles,
    )

def select_input(self, enable):
    SelectInput(
        display=self.display,
        opcode=self.display.get_extension_major(extname),
        destination_window=self,
        enable=enable,
    )

def init(disp, info):
    disp.extension_add_method('window', 'shape_combine', combine)
    disp.extension_add_method('window', 'shape_get_rectangles', get_rectangles)
    disp.extension_add_method('window', 'shape_input_selected', input_selected)
    disp.extension_add_method('window', 'shape_mask', mask)
    disp.extension_add_method('window', 'shape_offset', offset)
    disp.extension_add_method('window', 'shape_query_extents', query_extents)
    disp.extension_add_method('display', 'shape_query_version', query_version)
    disp.extension_add_method('window', 'shape_rectangles', rectangles)
    disp.extension_add_method('window', 'shape_select_input', select_input)
    disp.extension_add_event(info.first_event + Event.Notify, NotifyEventData, 'ShapeNotify')

