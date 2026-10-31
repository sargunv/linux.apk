#!/usr/bin/python3
"""Minimize or restore the probe windows on the runner's private desktop, never
the user's bus. Exits 1 when no window matched, so the runner can retry."""

import os
import sys

from gi.repository import Gio, GLib

assert os.environ.get("INNKEEPER_NESTED") == "1"
assert os.environ["XDG_RUNTIME_DIR"] != f"/run/user/{os.getuid()}"
(found,) = (
  Gio.bus_get_sync(Gio.BusType.SESSION, None)
  .call_sync(
    "dev.sargunv.Innkeeper.TestDesktop",
    "/dev/sargunv/Innkeeper/TestDesktop",
    "dev.sargunv.Innkeeper.TestDesktop",
    "Visibility",
    GLib.Variant("(b)", (sys.argv[1] == "restore",)),
    None,
    Gio.DBusCallFlags.NONE,
    5000,
    None,
  )
  .unpack()
)
sys.exit(0 if found else 1)
