#!/usr/bin/env bash
# Render Crew's app icon: three overlapping speech bubbles, one with a blue
# dot, on a dark rounded tile on the macOS icon grid (1024 px canvas,
# 824 px tile, 100 px inset, 185 px radius).
# Writes crates/crew-app/assets/AppIcon-1024.png and AppIcon.icns.
# Needs rsvg-convert (brew install librsvg), sips and iconutil.
# Usage: icon.sh [repo-root]
set -euo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
OUT="$ROOT/crates/crew-app/assets"

for tool in rsvg-convert sips iconutil; do
  command -v "$tool" >/dev/null || {
    echo "error: $tool not found (rsvg-convert: brew install librsvg)" >&2
    exit 1
  }
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# One bubble, 360 x 250 with a tail at the lower left, drawn three times
# from back to front. Each is filled with the tile colour so the one in
# front hides the outline behind it.
cat > "$WORK/icon.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1024" height="1024" viewBox="0 0 1024 1024">
  <defs>
    <path id="bubble" d="M70 0 H290 A70 70 0 0 1 360 70 V180 A70 70 0 0 1 290 250 H140 L70 320 L80 250 H70 A70 70 0 0 1 0 180 V70 A70 70 0 0 1 70 0 Z"/>
  </defs>
  <rect x="103" y="103" width="818" height="818" rx="182" fill="#0a0a0a" stroke="#1f1f1f" stroke-width="6"/>
  <g fill="#0a0a0a" stroke="#ededed" stroke-width="28" stroke-linejoin="round">
    <use xlink:href="#bubble" transform="translate(190 225)"/>
    <use xlink:href="#bubble" transform="translate(330 355)"/>
    <use xlink:href="#bubble" transform="translate(470 485)"/>
  </g>
  <circle cx="650" cy="610" r="36" fill="#52a8ff"/>
</svg>
SVG

mkdir -p "$OUT"
rsvg-convert -w 1024 -h 1024 "$WORK/icon.svg" -o "$OUT/AppIcon-1024.png"

SET="$WORK/AppIcon.iconset"
mkdir "$SET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$OUT/AppIcon-1024.png" --out "$SET/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$OUT/AppIcon-1024.png" --out "$SET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o "$OUT/AppIcon.icns"
echo "Wrote $OUT/AppIcon-1024.png and AppIcon.icns"
