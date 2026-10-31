#!/usr/bin/python3
"""dev.sargunv.Innkeeper.TestDesktop on the disposable KWin desktop.

The same interface tests/e2e/mutter.js serves in GNOME Shell: Input through
org_kde_kwin_fake_input, Visibility and Position through KWin scripts, and
Screenshot through org.kde.KWin.ScreenShot2. KWin grants this executable both
restricted interfaces from the desktop file tests/nested.sh writes.
"""

import os
import socket
import struct
import sys
from pathlib import Path

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf, GLib  # noqa: E402

assert os.environ["XDG_RUNTIME_DIR"] != f"/run/user/{os.getuid()}"

NAME = "dev.sargunv.Innkeeper.TestDesktop"
PATH = "/dev/sargunv/Innkeeper/TestDesktop"
# KWin scripts report back on this interface.
SCRIPT = "dev.sargunv.Innkeeper.TestDesktopScript"
PROBES = "dev.sargunv.innkeeper.probes"
scratch = Path(sys.argv[1])


class FakeInput:
  """A Wayland client bound to org_kde_kwin_fake_input."""

  def __init__(self):
    self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    self.sock.connect(
      os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"])
    )
    self.buf = b""
    self.next_id = 2
    registry = self.new_id()
    self.send(1, 1, struct.pack("<I", registry))
    globals_ = {}

    def on_global(obj, op, p):
      if obj == registry and op == 0:
        name, n = struct.unpack("<II", p[:8])
        iface = p[8 : 8 + n - 1].decode()
        version = struct.unpack("<I", p[8 + n + ((-n) % 4) :][:4])[0]
        globals_[iface] = (name, version)

    self.roundtrip(on_global)
    name, version = globals_["org_kde_kwin_fake_input"]
    self.fi = self.new_id()
    self.send(
      registry,
      0,
      struct.pack("<I", name)
      + string("org_kde_kwin_fake_input")
      + struct.pack("<II", min(version, 4), self.fi),
    )
    self.send(self.fi, 0, string("innkeeper test") + string("pointer test"))
    self.roundtrip()

  def new_id(self):
    self.next_id += 1
    return self.next_id - 1

  def send(self, obj, op, payload=b""):
    self.sock.sendall(
      struct.pack("<II", obj, ((8 + len(payload)) << 16) | op) + payload
    )

  def events(self):
    while True:
      while len(self.buf) >= 8:
        obj, so = struct.unpack("<II", self.buf[:8])
        size, op = so >> 16, so & 0xFFFF
        if len(self.buf) < size:
          break
        payload, self.buf = self.buf[8:size], self.buf[size:]
        yield obj, op, payload
      d = self.sock.recv(65536)
      if not d:
        raise SystemExit("kwin: Wayland disconnected")
      self.buf += d

  def roundtrip(self, on_event=None):
    cb = self.new_id()
    self.send(1, 0, struct.pack("<I", cb))
    for obj, op, p in self.events():
      if obj == cb and op == 0:
        return
      if obj == 1 and op == 0:
        raise SystemExit(f"kwin: Wayland error: {p!r}")
      if on_event:
        on_event(obj, op, p)

  def action(self, action):
    kind, _, values = action.partition(":")
    x, y = (float(v) for v in values.split(","))
    if kind == "abs":
      self.send(self.fi, 9, fixed(x) + fixed(y))
    elif kind == "rel":
      self.send(self.fi, 1, fixed(x) + fixed(y))
    elif kind in ("key", "button"):
      op = 10 if kind == "key" else 2
      self.send(self.fi, op, struct.pack("<II", int(x), int(y)))
    elif kind == "axis":
      self.send(self.fi, 3, struct.pack("<I", int(x)) + fixed(y))
    else:
      raise ValueError(f"unknown test seat action: {action}")
    self.roundtrip()


def string(s):
  b = s.encode() + b"\0"
  return struct.pack("<I", len(b)) + b + b"\0" * ((-len(b)) % 4)


def fixed(v):
  return struct.pack("<i", int(round(v * 256)))


def png(path, meta, data):
  """Write ScreenShot2's raw QImage as a PNG."""
  width, height, stride = (int(meta[k]) for k in ("width", "height", "stride"))
  fmt = int(meta["format"])
  rgba = bytearray(width * height * 4)
  for y in range(height):
    row = data[y * stride : y * stride + width * 4]
    out = memoryview(rgba)[y * width * 4 : (y + 1) * width * 4]
    if fmt in (4, 5, 6):  # QImage RGB32/ARGB32(_Premultiplied): BGRA bytes
      out[0::4], out[1::4], out[2::4] = row[2::4], row[1::4], row[0::4]
      out[3::4] = row[3::4] if fmt != 4 else b"\xff" * width
    elif fmt in (16, 17, 18):  # RGBX8888/RGBA8888(_Premultiplied)
      out[:] = row
    else:
      raise ValueError(f"unsupported QImage format {fmt}")
  GdkPixbuf.Pixbuf.new_from_bytes(
    GLib.Bytes.new(bytes(rgba)),
    GdkPixbuf.Colorspace.RGB,
    True,
    8,
    width,
    height,
    width * 4,
  ).savev(path, "png", [], [])


class TestDesktop(dbus.service.Object):
  def __init__(self, bus):
    super().__init__(bus, PATH)
    self.bus = bus
    self.seat = FakeInput()
    self.pending = {}
    self.scripts = 0

  @dbus.service.method(NAME, in_signature="s", out_signature="")
  def Input(self, action):
    self.seat.action(str(action))

  @dbus.service.method(NAME, in_signature="s", out_signature="")
  def Screenshot(self, path):
    kwin = dbus.Interface(
      self.bus.get_object("org.kde.KWin", "/org/kde/KWin/ScreenShot2"),
      "org.kde.KWin.ScreenShot2",
    )
    read, write = os.pipe()
    try:
      meta = kwin.CaptureWorkspace({}, dbus.types.UnixFd(write))
    finally:
      os.close(write)
    with os.fdopen(read, "rb") as f:
      data = f.read()
    png(str(path), meta, data)

  @dbus.service.method(
    NAME, in_signature="b", out_signature="b", async_callbacks=("ok", "err")
  )
  def Visibility(self, visible, ok, err):
    body = f"w.minimized = {str(not visible).lower()};\n"
    if visible:
      body += "workspace.activeWindow = w;\n"
    self.script(body, ok)

  @dbus.service.method(
    NAME, in_signature="", out_signature="b", async_callbacks=("ok", "err")
  )
  def Position(self, ok, err):
    # Move the probe for absolute input without resizing Android's display.
    self.script(
      "w.minimized = false;\n"
      "const g = w.frameGeometry; g.x = 300; g.y = 100; w.frameGeometry = g;\n"
      "workspace.activeWindow = w;\n",
      ok,
    )

  @dbus.service.method(SCRIPT, in_signature="sb", out_signature="")
  def Done(self, name, matched):
    self.finish(str(name), bool(matched))

  def finish(self, name, matched):
    reply = self.pending.pop(name, None)
    if reply:
      reply(matched)
    return False

  def script(self, body, reply):
    """Run body for each probe window; reply whether any matched."""
    self.scripts += 1
    name = f"innkeeper-test-desktop-{self.scripts}"
    path = scratch / f"{name}.js"
    path.write_text(
      "let matched = false;\n"
      "for (const w of workspace.windowList()) {\n"
      f'  if (!String(w.resourceClass).includes("{PROBES}")) continue;\n'
      f"  {body}"
      "  matched = true;\n"
      "}\n"
      f'callDBus("{NAME}", "{PATH}", "{SCRIPT}", "Done", "{name}", matched);\n'
    )
    scripting = dbus.Interface(
      self.bus.get_object("org.kde.KWin", "/Scripting"),
      "org.kde.kwin.Scripting",
    )
    ident = scripting.loadScript(str(path), name, signature="ss")

    def finish(matched):
      scripting.unloadScript(name)
      path.unlink(missing_ok=True)
      reply(matched)

    self.pending[name] = finish
    dbus.Interface(
      self.bus.get_object("org.kde.KWin", f"/Scripting/Script{ident}"),
      "org.kde.kwin.Script",
    ).run()
    # A mapped Android frame can precede the desktop window list. Answer
    # false so the runner retries rather than accepting a silent no-op.
    GLib.timeout_add(1000, self.finish, name, False)


DBusGMainLoop(set_as_default=True)
session = dbus.SessionBus()
service = TestDesktop(session)
(scratch / "kwin-support.txt").write_text(
  str(
    dbus.Interface(
      session.get_object("org.kde.KWin", "/KWin"), "org.kde.KWin"
    ).supportInformation()
  )
)
name = dbus.service.BusName(NAME, session, do_not_queue=True)
print("test desktop: native input and window control ready", flush=True)
GLib.MainLoop().run()
