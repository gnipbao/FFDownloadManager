#!/usr/bin/env sh
# Run from an MSYS2 UCRT64 shell on Windows. Produces a local-file-only helper.
set -eu
FFDM_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
FFDM_VERSION=8.0.3
FFDM_SHA256=6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818
FFDM_CACHE="$FFDM_ROOT/.tools/ffmpeg"
FFDM_ARCHIVE="$FFDM_CACHE/ffmpeg-$FFDM_VERSION.tar.xz"
FFDM_SOURCE="$FFDM_CACHE/ffmpeg-$FFDM_VERSION"
FFDM_BUILD="$FFDM_CACHE/build-$FFDM_VERSION-windows-x64"
FFDM_RESOURCES="$FFDM_ROOT/src-tauri/resources"
test "${MSYSTEM:-}" = UCRT64 || { echo 'Run in MSYS2 UCRT64.' >&2; exit 1; }
mkdir -p "$FFDM_CACHE" "$FFDM_BUILD" "$FFDM_RESOURCES/third-party"
if [ ! -f "$FFDM_ARCHIVE" ]; then
  curl --fail --location --retry 2 --connect-timeout 20 --max-time 300 \
    "https://ffmpeg.org/releases/ffmpeg-$FFDM_VERSION.tar.xz" -o "$FFDM_ARCHIVE.download"
  mv "$FFDM_ARCHIVE.download" "$FFDM_ARCHIVE"
fi
test "$(sha256sum "$FFDM_ARCHIVE" | cut -d ' ' -f 1)" = "$FFDM_SHA256" || {
  echo 'FFmpeg source SHA-256 mismatch; refusing to build.' >&2; exit 1;
}
if [ ! -f "$FFDM_SOURCE/configure" ]; then tar -xJf "$FFDM_ARCHIVE" -C "$FFDM_CACHE"; fi
cd "$FFDM_BUILD"
if [ ! -f ffmpeg.exe ]; then
  "$FFDM_SOURCE/configure" \
    --cc=gcc --disable-autodetect --disable-everything \
    --disable-network --disable-doc --disable-debug --disable-shared --enable-static \
    --disable-programs --enable-ffmpeg --disable-avdevice --enable-small \
    --enable-protocol=file,pipe \
    --enable-demuxer=mov,matroska,flv,concat,aac,mp3,ogg \
    --enable-muxer=mp4,ipod,matroska,webm,flv,mp3,ogg \
    --enable-parser=aac,h264,hevc,av1,vp9,opus \
    --enable-bsf=aac_adtstoasc,extract_extradata,h264_mp4toannexb,hevc_mp4toannexb,av1_frame_split,vp9_superframe \
    --extra-ldflags=-static > configure.log 2>&1 || {
      tail -n 80 configure.log >&2
      exit 1
    }
  make -j "${FFDM_BUILD_JOBS:-6}" > make.log 2>&1 || {
    tail -n 80 make.log >&2
    exit 1
  }
fi
cp ffmpeg.exe "$FFDM_RESOURCES/ffmpeg.exe"
cp "$FFDM_SOURCE/COPYING.LGPLv2.1" "$FFDM_RESOURCES/third-party/FFmpeg-LICENSE.txt"
cp "$FFDM_ARCHIVE" "$FFDM_RESOURCES/third-party/"
cp "$FFDM_ROOT/scripts/build-ffmpeg-windows.sh" "$FFDM_RESOURCES/third-party/"
"$FFDM_RESOURCES/ffmpeg.exe" -version | head -3
