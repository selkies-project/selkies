# Xlib.ext.dpms -- X Display Power Management Signaling
#
#    Copyright (C) 2020 Thiago Kenji Okada <thiagokokada@gmail.com>
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
This extension provides X Protocol control over the VESA Display
Power Management Signaling (DPMS) characteristics of video boards
under control of the X Window System.

Documentation: https://www.x.org/releases/X11R7.7/doc/xextproto/dpms.html
'''

from ..protocol import rq

extname = 'DPMS'


# DPMS Extension Power Levels
#      0     DPMSModeOn          In use
#      1     DPMSModeStandby     Blanked, low power
#      2     DPMSModeSuspend     Blanked, lower power
#      3     DPMSModeOff         Shut off, awaiting activity
DPMSModeOn = 0
DPMSModeStandby = 1
DPMSModeSuspend = 2
DPMSModeOff = 3

DPMSPowerLevel = (
    DPMSModeOn,
    DPMSModeStandby,
    DPMSModeSuspend,
    DPMSModeOff,
)


class DPMSGetVersion(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(0),
        rq.RequestLength(),
        rq.Card16('major_version'),
        rq.Card16('minor_version'),
        )

    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16('sequence_number'),
            rq.ReplyLength(),
            rq.Card16('major_version'),
            rq.Card16('minor_version'),
            rq.Pad(20),
            )


def get_version(self):
    return DPMSGetVersion(display=self.display,
                          opcode=self.display.get_extension_major(extname),
                          major_version=1,
                          minor_version=1)


class DPMSCapable(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(1),
        rq.RequestLength(),
        )

    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16('sequence_number'),
            rq.ReplyLength(),
            rq.Bool('capable'),
            rq.Pad(23),
            )


def capable(self):
    return DPMSCapable(display=self.display,
                       opcode=self.display.get_extension_major(extname),
                       major_version=1,
                       minor_version=1)


class DPMSGetTimeouts(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(2),
        rq.RequestLength(),
        )

    _reply = rq.Struct(
            rq.ReplyCode(),
            rq.Pad(1),
            rq.Card16('sequence_number'),
            rq.ReplyLength(),
            rq.Card16('standby_timeout'),
            rq.Card16('suspend_timeout'),
            rq.Card16('off_timeout'),
            rq.Pad(18),
            )


def get_timeouts(self):
    return DPMSGetTimeouts(display=self.display,
                           opcode=self.display.get_extension_major(extname),
                           major_version=1,
                           minor_version=1)


class DPMSSetTimeouts(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(3),
        rq.RequestLength(),
        rq.Card16('standby_timeout'),
        rq.Card16('suspend_timeout'),
        rq.Card16('off_timeout'),
        rq.Pad(2)
        )


def set_timeouts(self, standby_timeout, suspend_timeout, off_timeout):
    return DPMSSetTimeouts(display=self.display,
                           opcode=self.display.get_extension_major(extname),
                           major_version=1,
                           minor_version=1,
                           standby_timeout=standby_timeout,
                           suspend_timeout=suspend_timeout,
                           off_timeout=off_timeout)


class DPMSEnable(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(4),
        rq.RequestLength(),
        )


def enable(self):
    return DPMSEnable(display=self.display,
                      opcode=self.display.get_extension_major(extname),
                      major_version=1,
                      minor_version=1)


class DPMSDisable(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(5),
        rq.RequestLength(),
        )


def disable(self):
    return DPMSDisable(display=self.display,
                       opcode=self.display.get_extension_major(extname),
                       major_version=1,
                       minor_version=1)


class DPMSForceLevel(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(6),
        rq.RequestLength(),
        rq.Resource('power_level', DPMSPowerLevel),
        )


def force_level(self, power_level):
    return DPMSForceLevel(display=self.display,
                          opcode=self.display.get_extension_major(extname),
                          major_version=1,
                          minor_version=1,
                          power_level=power_level)


class DPMSInfo(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'),
        rq.Opcode(7),
        rq.RequestLength(),
        )

    _reply = rq.Struct(
        rq.ReplyCode(),
        rq.Pad(1),
        rq.Card16('sequence_number'),
        rq.ReplyLength(),
        rq.Card16('power_level'),
        rq.Bool('state'),
        rq.Pad(21),
        )


def info(self):
    return DPMSInfo(display=self.display,
                    opcode=self.display.get_extension_major(extname),
                    major_version=1,
                    minor_version=1)


def init(disp, _info):
    disp.extension_add_method('display', 'dpms_get_version', get_version)
    disp.extension_add_method('display', 'dpms_capable', capable)
    disp.extension_add_method('display', 'dpms_get_timeouts', get_timeouts)
    disp.extension_add_method('display', 'dpms_set_timeouts', set_timeouts)
    disp.extension_add_method('display', 'dpms_enable', enable)
    disp.extension_add_method('display', 'dpms_disable', disable)
    disp.extension_add_method('display', 'dpms_force_level', force_level)
    disp.extension_add_method('display', 'dpms_info', info)
