# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""The Python side of the selkies wheel, which is only its Jupyter entry point.

The server is the `selkies` executable the wheel installs; `selkies.session`
hands its Jupyter configuration to jupyter-server-proxy, whose registry of
servers is a group of Python entry points.
"""
