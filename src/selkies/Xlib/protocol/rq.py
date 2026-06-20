# Xlib.protocol.rq -- structure primitives for request, events and errors
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

# Standard modules
import sys
import traceback
import struct
from array import array

# Python 2/3 compatibility.
from six import PY3, binary_type, byte2int, indexbytes, iterbytes

# Xlib modules
from .. import X
from ..support import lock


def decode_string(bs):
    return bs.decode('latin1')

if PY3:
    def encode_array(a):
        return a.tobytes()
else:
    def encode_array(a):
        return a.tostring()


class BadDataError(Exception): pass

# These are struct codes, we know their byte sizes

signed_codes = { 1: 'b', 2: 'h', 4: 'l' }
unsigned_codes = { 1: 'B', 2: 'H', 4: 'L' }


# Unfortunately, we don't know the array sizes of B, H and L, since
# these use the underlying architecture's size for a char, short and
# long.  Therefore we probe for their sizes, and additionally create
# a mapping that translates from struct codes to array codes.
#
# Bleah.

array_unsigned_codes = { }
struct_to_array_codes = { }

for c in 'bhil':
    size = array(c).itemsize
    array_unsigned_codes[size] = c.upper()
    try:
        struct_to_array_codes[signed_codes[size]] = c
        struct_to_array_codes[unsigned_codes[size]] = c.upper()
    except KeyError:
        pass

# print array_unsigned_codes, struct_to_array_codes


class Field(object):
    """Field objects represent the data fields of a Struct.

    Field objects must have the following attributes:

       name         -- the field name, or None
       structcode   -- the struct codes representing this field
       structvalues -- the number of values encodes by structcode

    Additionally, these attributes should either be None or real methods:

       check_value  -- check a value before it is converted to binary
       parse_value  -- parse a value after it has been converted from binary

    If one of these attributes are None, no check or additional
    parsings will be done one values when converting to or from binary
    form.  Otherwise, the methods should have the following behaviour:

       newval = check_value(val)
         Check that VAL is legal when converting to binary form.  The
         value can also be converted to another Python value.  In any
         case, return the possibly new value.  NEWVAL should be a
         single Python value if structvalues is 1, a tuple of
         structvalues elements otherwise.

       newval = parse_value(val, display)
         VAL is an unpacked Python value, which now can be further
         refined.  DISPLAY is the current Display object.  Return the
         new value.  VAL will be a single value if structvalues is 1,
         a tuple of structvalues elements otherwise.

    If `structcode' is None the Field must have the method
    f.parse_binary_value() instead.  See its documentation string for
    details.
    """
    name = None
    default = None

    structcode = None
    structvalues = 0

    check_value = None
    parse_value = None

    keyword_args = False

    def __init__(self):
        pass

    def parse_binary_value(self, data, display, length, format):
        """value, remaindata = f.parse_binary_value(data, display, length, format)

        Decode a value for this field from the binary string DATA.
        If there are a LengthField and/or a FormatField connected to this
        field, their values will be LENGTH and FORMAT, respectively.  If
        there are no such fields the parameters will be None.

        DISPLAY is the display involved, which is really only used by
        the Resource fields.

        The decoded value is returned as VALUE, and the remaining part
        of DATA shold be returned as REMAINDATA.
        """
        raise RuntimeError('Neither structcode or parse_binary_value ' \
                'provided for {0}'.format(self))


class Pad(Field):
    def __init__(self, size):
        self.size = size
        self.value = b'\0' * size
        self.structcode = '{0}x'.format(size)
        self.structvalues = 0


class ConstantField(Field):
    def __init__(self, value):
        self.value = value


class Opcode(ConstantField):
    structcode = 'B'
    structvalues = 1

class ReplyCode(ConstantField):
    structcode = 'B'
    structvalues = 1

    def __init__(self):
        self.value = 1

class LengthField(Field):
    """A LengthField stores the length of some other Field whose size
    may vary, e.g. List and String8.

    Its name should be the same as the name of the field whose size
    it stores.  The other_fields attribute can be used to specify the
    names of other fields whose sizes are stored by this field, so
    a single length field can set the length of multiple fields.

    The lf.get_binary_value() method of LengthFields is not used, instead
    a lf.get_binary_length() should be provided.

    Unless LengthField.get_binary_length() is overridden in child classes,
    there should also be a lf.calc_length().
    """
    structcode = 'L'
    structvalues = 1
    other_fields = None

    def calc_length(self, length):
        """newlen = lf.calc_length(length)

        Return a new length NEWLEN based on the provided LENGTH.
        """

        return length


class TotalLengthField(LengthField):
    pass

class RequestLength(TotalLengthField):
    structcode = 'H'
    structvalues = 1

    def calc_length(self, length):
        return length // 4

class ReplyLength(TotalLengthField):
    structcode = 'L'
    structvalues = 1

    def calc_length(self, length):
        return (length - 32) // 4


class LengthOf(LengthField):
    def __init__(self, name, size):
        if isinstance(name, (list, tuple)):
            self.name = name[0]
            self.other_fields = name[1:]
        else:
            self.name = name
        self.structcode = unsigned_codes[size]


class OddLength(LengthField):
    structcode = 'B'
    structvalues = 1

    def __init__(self, name):
        self.name = name

    def calc_length(self, length):
        return length % 2

    def parse_value(self, value, display):
        if value == 0:
            return 'even'
        else:
            return 'odd'


class FormatField(Field):
    """A FormatField encodes the format of some other field, in a manner
    similar to LengthFields.

    The ff.get_binary_value() method is not used, replaced by
    ff.get_binary_format().
    """

    structvalues = 1

    def __init__(self, name, size):
        self.name = name
        self.structcode = unsigned_codes[size]

Format = FormatField


class ValueField(Field):
    def __init__(self, name, default = None):
        self.name = name
        self.default = default


class Int8(ValueField):
    structcode = 'b'
    structvalues = 1

class Int16(ValueField):
    structcode = 'h'
    structvalues = 1

class Int32(ValueField):
    structcode = 'l'
    structvalues = 1

class Card8(ValueField):
    structcode = 'B'
    structvalues = 1

class Card16(ValueField):
    structcode = 'H'
    structvalues = 1

class Card32(ValueField):
    structcode = 'L'
    structvalues = 1


class Resource(Card32):
    cast_function = '__resource__'
    class_name = 'resource'

    def __init__(self, name, codes = (), default = None):
        Card32.__init__(self, name, default)
        self.codes = codes

    def check_value(self, value):
        if hasattr(value, self.cast_function):
            return getattr(value, self.cast_function)()
        else:
            return value

    def parse_value(self, value, display):
        # if not display:
        #    return value
        if value in self.codes:
            return value

        c = display.get_resource_class(self.class_name)
        if c:
            return c(display, value)
        else:
            return value

class Window(Resource):
    cast_function = '__window__'
    class_name = 'window'

class Pixmap(Resource):
    cast_function = '__pixmap__'
    class_name = 'pixmap'

class Drawable(Resource):
    cast_function = '__drawable__'
    class_name = 'drawable'

class Fontable(Resource):
    cast_function = '__fontable__'
    class_name = 'fontable'

class Font(Resource):
    cast_function = '__font__'
    class_name = 'font'

class GC(Resource):
    cast_function = '__gc__'
    class_name = 'gc'

class Colormap(Resource):
    cast_function = '__colormap__'
    class_name = 'colormap'

class Cursor(Resource):
    cast_function = '__cursor__'
    class_name = 'cursor'


class Bool(ValueField):
    structvalues = 1
    structcode = 'B'

    def check_value(self, value):
        return not not value

class Set(ValueField):
    structvalues = 1

    def __init__(self, name, size, values, default = None):
        ValueField.__init__(self, name, default)
        self.structcode = unsigned_codes[size]
        self.values = values

    def check_value(self, val):
        if val not in self.values:
            raise ValueError('field %s: argument %s not in %s'
                             % (self.name, val, self.values))

        return val

class Gravity(Set):
    def __init__(self, name):
        Set.__init__(self, name, 1, (X.ForgetGravity, X.StaticGravity,
                                    X.NorthWestGravity, X.NorthGravity,
                                    X.NorthEastGravity, X.WestGravity,
                                    X.CenterGravity, X.EastGravity,
                                    X.SouthWestGravity, X.SouthGravity,
                                    X.SouthEastGravity))


class FixedBinary(ValueField):
    structvalues = 1

    def __init__(self, name, size):
        ValueField.__init__(self, name)
        self.structcode = '{0}s'.format(size)


class Binary(ValueField):
    structcode = None

    def __init__(self, name, pad = 1):
        ValueField.__init__(self, name)
        self.pad = pad

    def pack_value(self, val):
        val_bytes = val
        slen = len(val_bytes)

        if self.pad:
            return val_bytes + b'\0' * ((4 - slen % 4) % 4), slen, None
        else:
            return val_bytes, slen, None

    def parse_binary_value(self, data, display, length, format):
        if length is None:
            return data, b''

        if self.pad:
            slen = length + ((4 - length % 4) % 4)
        else:
            slen = length

        return data[:length], data[slen:]


class String8(ValueField):
    structcode = None

    def __init__(self, name, pad = 1):
        ValueField.__init__(self, name)
        self.pad = pad

    def pack_value(self, val):
        if isinstance(val, bytes):
            val_bytes = val
        else:
            val_bytes = val.encode()
        slen = len(val_bytes)

        if self.pad:
            return val_bytes + b'\0' * ((4 - slen % 4) % 4), slen, None
        else:
            return val_bytes, slen, None

    def parse_binary_value(self, data, display, length, format):
        if length is None:
            return decode_string(data), b''

        if self.pad:
            slen = length + ((4 - length % 4) % 4)
        else:
            slen = length

        data_str = decode_string(data[:length])

        return data_str, data[slen:]


class String16(ValueField):
    structcode = None

    def __init__(self, name, pad = 1):
        ValueField.__init__(self, name)
        self.pad = pad

    def pack_value(self, val):
        """Convert 8-byte string into 16-byte list"""
        if isinstance(val, bytes):
            val = list(iterbytes(val))

        slen = len(val)

        if self.pad:
            pad = b'\0\0' * (slen % 2)
        else:
            pad = b''

        return struct.pack('>' + 'H' * slen, *val) + pad, slen, None

    def parse_binary_value(self, data, display, length, format):
        if length == 'odd':
            length = len(data) // 2 - 1
        elif length == 'even':
            length = len(data) // 2

        if self.pad:
            slen = length + (length % 2)
        else:
            slen = length

        return struct.unpack('>' + 'H' * length, data[:length * 2]), data[slen * 2:]



class List(ValueField):
    """The List, FixedList and Object fields store compound data objects.
    The type of data objects must be provided as an object with the
    following attributes and methods:

    ...

    """

    structcode = None

    def __init__(self, name, type, pad = 1):
        ValueField.__init__(self, name)
        self.type = type
        self.pad = pad

    def parse_binary_value(self, data, display, length, format):
        if length is None:
            ret = []
            if self.type.structcode is None:
                while data:
                    val, data = self.type.parse_binary(data, display)
                    ret.append(val)
            else:
                scode = '=' + self.type.structcode
                slen = struct.calcsize(scode)
                pos = 0
                while pos + slen <= len(data):
                    v = struct.unpack(scode, data[pos: pos + slen])

                    if self.type.structvalues == 1:
                        v = v[0]

                    if self.type.parse_value is None:
                        ret.append(v)
                    else:
                        ret.append(self.type.parse_value(v, display))

                    pos = pos + slen

                data = data[pos:]

        else:
            ret = [None] * int(length)

            if self.type.structcode is None:
                for i in range(0, length):
                    ret[i], data = self.type.parse_binary(data, display)
            else:
                scode = '=' + self.type.structcode
                slen = struct.calcsize(scode)
                pos = 0
                for i in range(0, length):
                    v = struct.unpack(scode, data[pos: pos + slen])

                    if self.type.structvalues == 1:
                        v = v[0]

                    if self.type.parse_value is None:
                        ret[i] = v
                    else:
                        ret[i] = self.type.parse_value(v, display)

                    pos = pos + slen

                data = data[pos:]

        if self.pad:
            data = data[len(data) % 4:]

        return ret, data

    def pack_value(self, val):
        # Single-char values, we'll assume that means integer lists.
        if self.type.structcode and len(self.type.structcode) == 1:
            if self.type.check_value is not None:
                val = [self.type.check_value(v) for v in val]
            a = array(struct_to_array_codes[self.type.structcode], val)
            data = encode_array(a)
        else:
            data = []
            for v in val:
                data.append(self.type.pack_value(v))

            data = b''.join(data)

        if self.pad:
            dlen = len(data)
            data = data + b'\0' * ((4 - dlen % 4) % 4)

        return data, len(val), None


class FixedList(List):
    def __init__(self, name, size, type, pad = 1):
        List.__init__(self, name, type, pad)
        self.size = size

    def parse_binary_value(self, data, display, length, format):
        return List.parse_binary_value(self, data, display, self.size, format)

    def pack_value(self, val):
        if len(val) != self.size:
            raise BadDataError('length mismatch for FixedList %s' % self.name)
        return List.pack_value(self, val)


class Object(ValueField):
    def __init__(self, name, type, default = None):
        ValueField.__init__(self, name, default)
        self.type = type
        self.structcode = self.type.structcode
        self.structvalues = self.type.structvalues

    def parse_binary_value(self, data, display, length, format):
        return self.type.parse_binary(data, display)

    def parse_value(self, val, display):
        return self.type.parse_value(val, display)

    def pack_value(self, val):
        return self.type.pack_value(val)

    def check_value(self, val):
        if isinstance(val, tuple):
            vals = []
            i = 0
            for f in self.type.fields:
                if f.name:
                    if f.check_value is None:
                        v = val[i]
                    else:
                        v = f.check_value(val[i])
                    if f.structvalues == 1:
                        vals.append(v)
                    else:
                        vals.extend(v)
                    i = i + 1
            return vals

        if isinstance(val, dict):
            data = val
        elif isinstance(val, DictWrapper):
            data = val._data
        else:
            raise TypeError('Object value must be tuple, dictionary or DictWrapper: %s' % val)

        vals = []
        for f in self.type.fields:
            if f.name:
                if f.check_value is None:
                    v = data[f.name]
                else:
                    v = f.check_value(data[f.name])
                if f.structvalues == 1:
                    vals.append(v)
                else:
                    vals.extend(v)

        return vals


class PropertyData(ValueField):
    structcode = None

    def parse_binary_value(self, data, display, length, format):
        if length is None:
            length = len(data) // (format // 8)
        else:
            length = int(length)

        if format == 0:
            ret = None

        elif format == 8:
            ret = (8, data[:length])
            data = data[length + ((4 - length % 4) % 4):]

        elif format == 16:
            ret = (16, array(array_unsigned_codes[2], data[:2 * length]))
            data = data[2 * (length + length % 2):]

        elif format == 32:
            ret = (32, array(array_unsigned_codes[4], data[:4 * length]))
            data = data[4 * length:]

        return ret, data

    def pack_value(self, value):
        fmt, val = value

        if fmt not in (8, 16, 32):
            raise BadDataError('Invalid property data format {0}'.format(fmt))

        if isinstance(val, binary_type):
            size = fmt // 8
            vlen = len(val)
            if vlen % size:
                vlen = vlen - vlen % size
                data = val[:vlen]
            else:
                data = val

            dlen = vlen // size

        else:
            if isinstance(val, tuple):
                val = list(val)

            size = fmt // 8
            a = array(array_unsigned_codes[size], val)
            data = encode_array(a)
            dlen = len(val)

        dl = len(data)
        data = data + b'\0' * ((4 - dl % 4) % 4)

        return data, dlen, fmt


class FixedPropertyData(PropertyData):
    def __init__(self, name, size):
        PropertyData.__init__(self, name)
        self.size = size

    def parse_binary_value(self, data, display, length, format):
        return PropertyData.parse_binary_value(self, data, display,
                                               self.size // (format // 8), format)

    def pack_value(self, value):
        data, dlen, fmt = PropertyData.pack_value(self, value)

        if len(data) != self.size:
            raise BadDataError('Wrong data length for FixedPropertyData: %s'
                               % (value, ))

        return data, dlen, fmt


class ValueList(Field):
    structcode = None
    keyword_args = True
    default = 'usekeywords'

    def __init__(self, name, mask, pad, *fields):
        self.name = name
        self.maskcode = '={0}{1}x'.format(unsigned_codes[mask], pad).encode()
        self.maskcodelen = struct.calcsize(self.maskcode)
        self.fields = []

        flag = 1
        for f in fields:
            if f.name:
                self.fields.append((f, flag))
                flag = flag << 1

    def pack_value(self, arg, keys):
        mask = 0
        data = b''

        if arg == self.default:
            arg = keys

        for field, flag in self.fields:
            if field.name in arg:
                mask = mask | flag

                val = arg[field.name]
                if field.check_value is not None:
                    val = field.check_value(val)

                d = struct.pack('=' + field.structcode, val)
                data = data + d + b'\0' * (4 - len(d))

        return struct.pack(self.maskcode, mask) + data, None, None

    def parse_binary_value(self, data, display, length, format):
        r = {}

        mask = int(struct.unpack(self.maskcode, data[:self.maskcodelen])[0])
        data = data[self.maskcodelen:]

        for field, flag in self.fields:
            if mask & flag:
                if field.structcode:
                    vals = struct.unpack('=' + field.structcode,
                                         data[:struct.calcsize('=' + field.structcode)])
                    if field.structvalues == 1:
                        vals = vals[0]

                    if field.parse_value is not None:
                        vals = field.parse_value(vals, display)

                else:
                    vals, d = field.parse_binary_value(data[:4], display, None, None)

                r[field.name] = vals
                data = data[4:]

        return DictWrapper(r), data


class KeyboardMapping(ValueField):
    structcode = None

    def parse_binary_value(self, data, display, length, format):
        if length is None:
            dlen = len(data)
        else:
            dlen = 4 * length * format

        a = array(array_unsigned_codes[4], bytes(data[:dlen]))

        ret = []
        for i in range(0, len(a), format):
            ret.append(a[i : i + format])

        return ret, data[dlen:]

    def pack_value(self, value):
        keycodes = 0
        for v in value:
            keycodes = max(keycodes, len(v))

        a = array(array_unsigned_codes[4])

        for v in value:
            for k in v:
                a.append(k)
            for i in range(len(v), keycodes):
                a.append(X.NoSymbol)

        return encode_array(a), len(value), keycodes


class ModifierMapping(ValueField):
    structcode = None

    def parse_binary_value(self, data, display, length, format):
        a = array(array_unsigned_codes[1], data[:8 * format])

        ret = []
        for i in range(0, 8):
            ret.append(a[i * format : (i + 1) * format])

        return ret, data[8 * format:]

    def pack_value(self, value):
        if len(value) != 8:
            raise BadDataError('ModifierMapping list should have eight elements')

        keycodes = 0
        for v in value:
            keycodes = max(keycodes, len(v))

        a = array(array_unsigned_codes[1])

        for v in value:
            for k in v:
                a.append(k)
            for i in range(len(v), keycodes):
                a.append(0)

        return encode_array(a), len(value), keycodes

class EventField(ValueField):
    structcode = None

    def pack_value(self, value):
        if not isinstance(value, Event):
            raise BadDataError('%s is not an Event for field %s' % (value, self.name))

        return value._binary, None, None

    def parse_binary_value(self, data, display, length, format):
        from . import event

        estruct = display.event_classes.get(byte2int(data) & 0x7f, event.AnyEvent)
        if type(estruct) == dict:
            # this etype refers to a set of sub-events with individual subcodes
            estruct = estruct[indexbytes(data, 1)]

        return estruct(display = display, binarydata = data[:32]), data[32:]


#
# Objects usable for List and FixedList fields.
# Struct is also usable.
#

class ScalarObj(object):
    def __init__(self, code):
        self.structcode = code
        self.structvalues = 1
        self.parse_value = None
        self.check_value = None

Card8Obj  = ScalarObj('B')
Card16Obj = ScalarObj('H')
Card32Obj = ScalarObj('L')

class ResourceObj(object):
    structcode = 'L'
    structvalues = 1

    def __init__(self, class_name):
        self.class_name = class_name
        self.check_value = None

    def parse_value(self, value, display):
        # if not display:
        #     return value
        c = display.get_resource_class(self.class_name)
        if c:
            return c(display, value)
        else:
            return value

WindowObj = ResourceObj('window')
ColormapObj = ResourceObj('colormap')

class StrClass(object):
    structcode = None

    def pack_value(self, val):
        return (chr(len(val)) + val).encode()

    def parse_binary(self, data, display):
        slen = byte2int(data) + 1
        return decode_string(data[1:slen]), data[slen:]

Str = StrClass()


class Struct(object):

    """Struct objects represents a binary data structure.  It can
    contain both fields with static and dynamic sizes.  However, all
    static fields must appear before all dynamic fields.

    Fields are represented by various subclasses of the abstract base
    class Field.  The fields of a structure are given as arguments
    when instantiating a Struct object.

    Struct objects have two public methods:

      to_binary()    -- build a binary representation of the structure
                        with the values given as arguments
      parse_binary() -- convert a binary (string) representation into
                        a Python dictionary or object.

    These functions will be generated dynamically for each Struct
    object to make conversion as fast as possible.  They are
    generated the first time the methods are called.
    """

    def __init__(self, *fields):
        self.fields = fields

        # Structures for to_binary, parse_value and parse_binary
        self.static_codes = '='
        self.static_values = 0
        self.static_fields = []
        self.static_size = None
        self.var_fields = []

        for f in self.fields:
            # Append structcode if there is one and we haven't
            # got any varsize fields yet.
            if f.structcode is not None:
                assert not self.var_fields

                self.static_codes = self.static_codes + f.structcode

                # Only store fields with values
                if f.structvalues > 0:
                    self.static_fields.append(f)
                    self.static_values = self.static_values + f.structvalues

            # If we have got one varsize field, all the rest must
            # also be varsize fields.
            else:
                self.var_fields.append(f)

        self.static_size = struct.calcsize(self.static_codes)
        if self.var_fields:
            self.structcode = None
            self.structvalues = 0
        else:
            self.structcode = self.static_codes[1:]
            self.structvalues = self.static_values


    # These functions get called only once, as they will override
    # themselves with dynamically created functions in the Struct
    # object

    def to_binary(self, *varargs, **keys):
        """data = s.to_binary(...)

        Convert Python values into the binary representation.  The
        arguments will be all value fields with names, in the order
        given when the Struct object was instantiated.  With one
        exception: fields with default arguments will be last.

        Returns the binary representation as the string DATA.
        """
        # Emulate Python function argument handling with our field names
        names = [f.name for f in self.fields \
                 if isinstance(f, ValueField) and f.name]
        field_args = dict(zip(names, varargs))
        if set(field_args).intersection(keys):
            dupes = ", ".join(set(field_args).intersection(keys))
            raise TypeError("{0} arguments were passed both positionally and by keyword".format(dupes))
        field_args.update(keys)
        for f in self.fields:
            if f.name and (f.name not in field_args):
                if f.default is None:
                    raise TypeError("Missing required argument {0}".format(f.name))
                field_args[f.name] = f.default
        # /argument handling

        # First pack all varfields so their lengths and formats are
        # available when we pack their static LengthFields and
        # FormatFields

        total_length = self.static_size
        var_vals = {}
        lengths = {}
        formats = {}

        for f in self.var_fields:
            if f.keyword_args:
                v, l, fm = f.pack_value(field_args[f.name], keys)
            else:
                v, l, fm = f.pack_value(field_args[f.name])
            var_vals[f.name] = v
            lengths[f.name] = l
            formats[f.name] = fm

            total_length += len(v)


        # Construct item list for struct.pack call, packing all static fields.
        pack_items = []

        for f in self.static_fields:
            if isinstance(f, LengthField):

                # If this is a total length field, insert
                # the calculated field value here
                if isinstance(f, TotalLengthField):
                    pack_items.append(f.calc_length(total_length))
                else:
                    pack_items.append(f.calc_length(lengths[f.name]))

            # Format field, just insert the value we got previously
            elif isinstance(f, FormatField):
                pack_items.append(formats[f.name])

            # A constant field, insert its value directly
            elif isinstance(f, ConstantField):
                pack_items.append(f.value)

            # Value fields
            else:
                if f.structvalues == 1:
                    # If there's a value check/convert function, call it
                    if f.check_value is not None:
                        pack_items.append(f.check_value(field_args[f.name]))
                    # Else just use the argument as provided
                    else:
                        pack_items.append(field_args[f.name])

                # Multivalue field.  Handled like single valuefield,
                # but the value are tuple unpacked into separate arguments
                # which are appended to pack_items
                else:
                    if f.check_value is not None:
                        pack_items.extend(f.check_value(field_args[f.name]))
                    else:
                        pack_items.extend(field_args[f.name])

        static_part = struct.pack(self.static_codes, *pack_items)
        var_parts = [var_vals[f.name] for f in self.var_fields]
        return static_part + b''.join(var_parts)


    def pack_value(self, value):

        """ This function allows Struct objects to be used in List and
        Object fields.  Each item represents the arguments to pass to
        to_binary, either a tuple, a dictionary or a DictWrapper.

        """

        if type(value) is tuple:
            return self.to_binary(*value)
        elif isinstance(value, dict):
            return self.to_binary(**value)
        elif isinstance(value, DictWrapper):
            return self.to_binary(**value._data)
        else:
            raise BadDataError('%s is not a tuple or a list' % (value))


    def parse_value(self, val, display, rawdict = False):

        """This function is used by List and Object fields to convert
        Struct objects with no var_fields into Python values.

        """
        ret = {}
        vno = 0
        for f in self.static_fields:
            # Fields without names should be ignored, and there should
            # not be any length or format fields if this function
            # ever gets called.  (If there were such fields, there should
            # be a matching field in var_fields and then parse_binary
            # would have been called instead.

            if not f.name:
                pass

            elif isinstance(f, LengthField):
                pass

            elif isinstance(f, FormatField):
                pass

            # Value fields
            else:
                # If this field has a parse_value method, call it, otherwise
                # use the unpacked value as is.
                if f.structvalues == 1:
                    field_val = val[vno]
                else:
                    field_val = val[vno:vno+f.structvalues]

                if f.parse_value is not None:
                    field_val = f.parse_value(field_val, display, rawdict=rawdict)
                ret[f.name] = field_val

            vno = vno + f.structvalues

        if not rawdict:
            return DictWrapper(ret)
        return ret

    def parse_binary(self, data, display, rawdict = False):

        """values, remdata = s.parse_binary(data, display, rawdict = False)

        Convert a binary representation of the structure into Python values.

        DATA is a string or a buffer containing the binary data.
        DISPLAY should be a Xlib.protocol.display.Display object if
        there are any Resource fields or Lists with ResourceObjs.

        The Python values are returned as VALUES.  If RAWDICT is true,
        a Python dictionary is returned, where the keys are field
        names and the values are the corresponding Python value.  If
        RAWDICT is false, a DictWrapper will be returned where all
        fields are available as attributes.

        REMDATA are the remaining binary data, unused by the Struct object.

        """
        ret = {}
        val = struct.unpack(self.static_codes, data[:self.static_size])
        lengths = {}
        formats = {}

        vno = 0
        for f in self.static_fields:

            # Fields without name should be ignored.  This is typically
            # pad and constant fields

            if not f.name:
                pass

            # Store index in val for Length and Format fields, to be used
            # when treating varfields.

            elif isinstance(f, LengthField):
                f_names = [f.name]
                if f.other_fields:
                    f_names.extend(f.other_fields)
                field_val = val[vno]
                if f.parse_value is not None:
                    field_val = f.parse_value(field_val, display)
                for f_name in f_names:
                    lengths[f_name] = field_val

            elif isinstance(f, FormatField):
                formats[f.name] = val[vno]

            # Treat value fields the same was as in parse_value.
            else:
                if f.structvalues == 1:
                    field_val = val[vno]
                else:
                    field_val = val[vno:vno+f.structvalues]

                if f.parse_value is not None:
                    field_val = f.parse_value(field_val, display)
                ret[f.name] = field_val

            vno = vno + f.structvalues

        data = data[self.static_size:]

        # Call parse_binary_value for each var_field, passing the
        # length and format values from the unpacked val.

        for f in self.var_fields:
            ret[f.name], data = f.parse_binary_value(data, display,
                                                     lengths.get(f.name),
                                                     formats.get(f.name),
                                                    )

        if not rawdict:
            ret = DictWrapper(ret)
        return ret, data


class TextElements8(ValueField):
    string_textitem = Struct( LengthOf('string', 1),
                              Int8('delta'),
                              String8('string', pad = 0) )

    def pack_value(self, value):
        data = b''
        args = {}

        for v in value:
            # Let values be simple strings, meaning a delta of 0
            if type(v) in (str, bytes):
                v = (0, v)

            # A tuple, it should be (delta, string)
            # Encode it as one or more textitems

            if isinstance(v, (tuple, dict, DictWrapper)):

                if isinstance(v, tuple):
                    delta, m_str = v
                else:
                    delta = v['delta']
                    m_str = v['string']

                while delta or m_str:
                    args['delta'] = delta
                    args['string'] = m_str[:254]

                    data = data + self.string_textitem.to_binary(*(), **args)

                    delta = 0
                    m_str = m_str[254:]

            # Else an integer, i.e. a font change
            else:
                # Use fontable cast function if instance
                if isinstance(v, Fontable):
                    v = v.__fontable__()

                data = data + struct.pack('>BL', 255, v)

        # Pad out to four byte length
        dlen = len(data)
        return data + b'\0' * ((4 - dlen % 4) % 4), None, None

    def parse_binary_value(self, data, display, length, format):
        values = []
        while 1:
            if len(data) < 2:
                break

            # font change
            if byte2int(data) == 255:
                values.append(struct.unpack('>L', bytes(data[1:5]))[0])
                data = data[5:]

            # skip null strings
            elif byte2int(data) == 0 and indexbytes(data, 1) == 0:
                data = data[2:]

            # string with delta
            else:
                v, data = self.string_textitem.parse_binary(data, display)
                values.append(v)

        return values, ''



class TextElements16(TextElements8):
    string_textitem = Struct( LengthOf('string', 1),
                              Int8('delta'),
                              String16('string', pad = 0) )



class GetAttrData(object):
    def __getattr__(self, attr):
        try:
            if self._data:
                return self._data[attr]
            else:
                raise AttributeError(attr)
        except KeyError:
            raise AttributeError(attr)

class DictWrapper(GetAttrData):
    def __init__(self, dict):
        self.__dict__['_data'] = dict

    def __getitem__(self, key):
        return self._data[key]

    def __setitem__(self, key, value):
        self._data[key] = value

    def __delitem__(self, key):
        del self._data[key]

    def __setattr__(self, key, value):
        self._data[key] = value

    def __delattr__(self, key):
        del self._data[key]

    def __str__(self):
        return str(self._data)

    def __repr__(self):
        return '%s(%s)' % (self.__class__.__name__, repr(self._data))

    def __lt__(self, other):
        if isinstance(other, DictWrapper):
            return self._data < other._data
        else:
            return self._data < other

    def __gt__(self, other):
        if isinstance(other, DictWrapper):
            return self._data > other._data
        else:
            return self._data > other

    def __eq__(self, other):
        if isinstance(other, DictWrapper):
            return self._data == other._data
        else:
            return self._data == other


class Request(object):
    def __init__(self, display, onerror = None, *args, **keys):
        self._errorhandler = onerror
        self._binary = self._request.to_binary(*args, **keys)
        self._serial = None
        display.send_request(self, onerror is not None)

    def _set_error(self, error):
        if self._errorhandler is not None:
            return call_error_handler(self._errorhandler, error, self)
        else:
            return 0

class ReplyRequest(GetAttrData):
    def __init__(self, display, defer = False, *args, **keys):
        self._display = display
        self._binary = self._request.to_binary(*args, **keys)
        self._serial = None
        self._data = None
        self._error = None

        self._response_lock = lock.allocate_lock()

        self._display.send_request(self, True)
        if not defer:
            self.reply()

    def reply(self):
        # Send request and wait for reply if we hasn't
        # already got one.  This means that reply() can safely
        # be called more than one time.

        self._response_lock.acquire()
        while self._data is None and self._error is None:
            self._display.send_recv_lock.acquire()
            self._response_lock.release()

            self._display.send_and_recv(request = self._serial)
            self._response_lock.acquire()

        self._response_lock.release()
        self._display = None

        # If error has been set, raise it
        if self._error:
            raise self._error

    def _parse_response(self, data):
        self._response_lock.acquire()
        self._data, d = self._reply.parse_binary(data, self._display, rawdict = True)
        self._response_lock.release()

    def _set_error(self, error):
        self._response_lock.acquire()
        self._error = error
        self._response_lock.release()
        return 1

    def __repr__(self):
        return '<%s serial = %s, data = %s, error = %s>' % (self.__class__.__name__, self._serial, self._data, self._error)


class Event(GetAttrData):
    def __init__(self, binarydata = None, display = None,
                 **keys):
        if binarydata:
            self._binary = binarydata
            self._data, data = self._fields.parse_binary(binarydata, display,
                                                         rawdict = True)
            # split event type into type and send_event bit
            self._data['send_event'] = not not self._data['type'] & 0x80
            self._data['type'] = self._data['type'] & 0x7f
        else:
            if self._code:
                keys['type'] = self._code

            keys['sequence_number'] = 0

            self._binary = self._fields.to_binary(**keys)

            keys['send_event'] = 0
            self._data = keys

    def __repr__(self):
        kwlist = []
        for kw, val in self._data.items():
            if kw == 'send_event':
                continue
            if kw == 'type' and self._data['send_event']:
                val = val | 0x80
            kwlist.append('%s = %s' % (kw, repr(val)))

        kws = ', '.join(kwlist)
        return '%s(%s)' % (self.__class__.__name__, kws)

    def __lt__(self, other):
        if isinstance(other, Event):
            return self._data < other._data
        else:
            return self._data < other

    def __gt__(self, other):
        if isinstance(other, Event):
            return self._data > other._data
        else:
            return self._data > other

    def __eq__(self, other):
        if isinstance(other, Event):
            return self._data == other._data
        else:
            return self._data == other


def call_error_handler(handler, error, request):
    try:
        return handler(error, request)
    except:
        sys.stderr.write('Exception raised by error handler.\n')
        traceback.print_exc()
        return 0
