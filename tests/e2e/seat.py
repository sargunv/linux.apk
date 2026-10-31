#!/usr/bin/python3
"""Input on the disposable compositor seat: abs:x,y, rel:x,y, button:code,state,
key:code,state, axis:axis,value (from argv, or lines on stdin with --stdin)."""

import sys

from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
actions = sys.stdin if sys.argv[1:] == ["--stdin"] else sys.argv[1:]
for action in actions:
  bus.call_sync(
    "dev.sargunv.Innkeeper.TestDesktop",
    "/dev/sargunv/Innkeeper/TestDesktop",
    "dev.sargunv.Innkeeper.TestDesktop",
    "Input",
    GLib.Variant("(s)", (action.strip(),)),
    None,
    Gio.DBusCallFlags.NONE,
    5000,
    None,
  )
