#!/usr/bin/env bash
# Render crates/pulsar-link/icon/icon.svg into the app icons beside it: AppIcon.icns, every
# size macOS asks of an app, 16 to 1024 px; and pulsar-link.ico, the tile alone (Windows
# draws no margin of its own), 16 to 256 px, built into the Windows executables by
# build.rs. Run after changing the SVG, and commit all three; a release needs no renderer.
# Needs Google Chrome.
set -euo pipefail
cd "$(dirname "$0")/.."
CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
DIR=crates/pulsar-link/icon
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

render() { # svg, png
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --default-background-color=00000000 --window-size=1024,1024 \
    --screenshot="$2" "file://$1" 2>/dev/null
}
render "$PWD/$DIR/icon.svg" "$WORK/mac.png"
# The same drawing, its view cropped to the tile.
sed 's/viewBox="0 0 1024 1024"/viewBox="100 100 824 824"/' "$DIR/icon.svg" > "$WORK/tile.svg"
render "$WORK/tile.svg" "$WORK/tile.png"

mkdir "$WORK/AppIcon.iconset"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" "$WORK/mac.png" --out "$WORK/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z "$d" "$d" "$WORK/mac.png" --out "$WORK/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns -o "$DIR/AppIcon.icns" "$WORK/AppIcon.iconset"

sizes=(16 24 32 48 64 128 256)
for s in "${sizes[@]}"; do
  sips -z "$s" "$s" "$WORK/tile.png" --out "$WORK/ico-$s.png" >/dev/null
done
# An .ico is a directory of PNG images (Windows Vista on); a width or height of 256
# is written as 0.
python3 - "$DIR/pulsar-link.ico" "${sizes[@]/#/$WORK/ico-}" <<'PY'
import struct, sys
out, pngs = sys.argv[1], [p + ".png" for p in sys.argv[2:]]
blobs = [open(p, "rb").read() for p in pngs]
sizes = [struct.unpack(">II", b[16:24]) for b in blobs]
offset = 6 + 16 * len(blobs)
head = struct.pack("<HHH", 0, 1, len(blobs))
for (w, h), b in zip(sizes, blobs):
    head += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(b), offset)
    offset += len(b)
open(out, "wb").write(head + b"".join(blobs))
PY
echo "$DIR/AppIcon.icns $DIR/pulsar-link.ico"
