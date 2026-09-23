#!/bin/sh
set -eu
FFDM_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$FFDM_ROOT"
test "$(uname -s)" = Darwin || { echo 'Run this build on macOS.' >&2; exit 1; }
if [ "${CI:-}" != true ]; then
  export RUSTUP_HOME="$FFDM_ROOT/.tools/rustup"
  export CARGO_HOME="$FFDM_ROOT/.tools/cargo"
fi
export MACOSX_DEPLOYMENT_TARGET=13.0
if [ "${CI:-}" != true ]; then
  case "$(rustup run stable rustc --version 2>/dev/null || true)" in
    'rustc 1.98.1 '*) export RUSTUP_TOOLCHAIN=stable ;;
    *) export RUSTUP_TOOLCHAIN=1.98.1 ;;
  esac
fi
if [ ! -x node_modules/.bin/tauri ]; then npm ci --ignore-scripts --cache .tools/npm-cache; fi
sh scripts/build-ffmpeg-macos.sh
node_modules/.bin/tauri build --bundles app -- --locked
FFDM_APP="$FFDM_ROOT/target/release/bundle/macos/FFDownload.app"
FFDM_DIST="$FFDM_ROOT/dist"
mkdir -p "$FFDM_DIST"
ditto "$FFDM_APP" "$FFDM_DIST/FFDownload.app"
# Sign the remux helper explicitly before sealing the application bundle.
codesign --force --sign - "$FFDM_DIST/FFDownload.app/Contents/Resources/ffmpeg"
codesign --force --deep --sign - "$FFDM_DIST/FFDownload.app"
codesign --verify --deep --strict "$FFDM_DIST/FFDownload.app"
FFDM_ARCH=$(uname -m)
FFDM_VERSION=$(node -p "require('./src-tauri/tauri.conf.json').version")
ditto -c -k --sequesterRsrc --keepParent "$FFDM_DIST/FFDownload.app" "$FFDM_DIST/FFDownload-$FFDM_VERSION-macos-$FFDM_ARCH.zip"
mkdir -p "$FFDM_ROOT/.bench-work/desktop"
FFDM_STAGE=$(mktemp -d "$FFDM_ROOT/.bench-work/desktop/dmg.XXXXXX")
trap 'rm -rf "$FFDM_STAGE"' EXIT HUP INT TERM
ditto "$FFDM_DIST/FFDownload.app" "$FFDM_STAGE/FFDownload.app"
ln -s /Applications "$FFDM_STAGE/Applications"
cp "$FFDM_ROOT/docs/macos-install.txt" "$FFDM_STAGE/使用说明.txt"
hdiutil create -quiet -volname FFDownload -srcfolder "$FFDM_STAGE" -ov -format UDZO \
  "$FFDM_DIST/FFDownload-$FFDM_VERSION-macos-$FFDM_ARCH.dmg"
hdiutil verify "$FFDM_DIST/FFDownload-$FFDM_VERSION-macos-$FFDM_ARCH.dmg"
echo "Built $FFDM_DIST/FFDownload.app"
echo "Archive $FFDM_DIST/FFDownload-$FFDM_VERSION-macos-$FFDM_ARCH.zip"
echo "Installer $FFDM_DIST/FFDownload-$FFDM_VERSION-macos-$FFDM_ARCH.dmg"
