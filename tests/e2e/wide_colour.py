"""Whether a desktop screenshot shows WideColorProbe's SDR orange."""

import sys

import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf  # noqa: E402


def visible(path):
  shot = GdkPixbuf.Pixbuf.new_from_file(path)
  pixels = shot.get_pixels()
  stride, step = shot.get_rowstride(), shot.get_n_channels()
  count = 0
  for y in range(shot.get_height()):
    row = pixels[y * stride : y * stride + shot.get_width() * step]
    for i in range(0, len(row), step):
      if row[i] > 220 and 60 < row[i + 1] < 100 and 10 < row[i + 2] < 50:
        count += 1
  return count > 10000


if __name__ == "__main__":
  sys.exit(0 if visible(sys.argv[1]) else 1)
