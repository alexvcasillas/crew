#!/usr/bin/env bash
# Build Crew.app for local use: {target}/bundle/Crew.app, where {target} is
# $CARGO_TARGET_DIR when set, else target/ in the repo.
#
# Contents/MacOS holds the app (crew-app) and the binaries it runs: crewd,
# crew-mcp and crew. The bundle is ad-hoc signed; it isn't notarised. The
# icon comes from crates/crew-app/assets/AppIcon.icns (scripts/icon.sh
# renders it).
# Usage: bundle.sh [--release] [repo-root]
set -euo pipefail

PROFILE=debug
while [[ "${1:-}" == --* ]]; do
  case "$1" in
    --release) PROFILE=release ;;
    *)
      echo "error: unknown option $1" >&2
      exit 1
      ;;
  esac
  shift
done
ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"

[[ "$(uname -s)" == Darwin ]] || {
  echo "error: Crew.app is macOS only" >&2
  exit 1
}

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[[ -n "$VERSION" ]] || {
  echo "error: workspace version not found in Cargo.toml" >&2
  exit 1
}

TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
# Cargo reads a relative CARGO_TARGET_DIR from where it runs: here.
[[ "$TARGET" == /* ]] || TARGET="$PWD/$TARGET"

ICON="$ROOT/crates/crew-app/assets/AppIcon.icns"
[[ -f "$ICON" ]] || {
  echo "error: $ICON not found (run scripts/icon.sh)" >&2
  exit 1
}
ENTITLEMENTS="$ROOT/crates/crew-app/entitlements.plist"

BINS=(crew-app crewd crew-mcp crew)
cargo_flags=(--manifest-path "$ROOT/Cargo.toml")
for bin in "${BINS[@]}"; do
  cargo_flags+=(--bin "$bin")
done
[[ "$PROFILE" == release ]] && cargo_flags+=(--release)
cargo build --locked "${cargo_flags[@]}"

APP="$TARGET/bundle/Crew.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
for bin in "${BINS[@]}"; do
  cp "$TARGET/$PROFILE/$bin" "$APP/Contents/MacOS/$bin"
done
cp "$ICON" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>dev.crew.app</string>
  <key>CFBundleName</key>
  <string>Crew</string>
  <key>CFBundleDisplayName</key>
  <string>Crew</string>
  <key>CFBundleExecutable</key>
  <string>crew-app</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null
# A Unix-socket client outside the App Sandbox needs no entitlements; the
# file is empty and kept for when signing gets stricter.
codesign --force --deep --entitlements "$ENTITLEMENTS" -s - "$APP"
echo "Built $APP ($PROFILE, $VERSION)"
