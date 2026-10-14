#!/bin/sh
# Build the FFmpeg PiTV bundles — the demuxer only — for this machine (Linux).
#
# WHY WE BUNDLE. The demuxer's component list is part of the app: which containers, parsers,
# bitstream filters and subtitle decoders exist is a compile-time fact here, not whatever the
# distribution happens to ship. Video and audio are decoded by GStreamer; FFmpeg never touches a
# video frame.
#
# LICENCE. Built SHARED, deliberately. FFmpeg here is LGPL-2.1 (no --enable-gpl, no
# --enable-nonfree), and shipping it as separate .so files keeps us in §6(b) — the user can
# replace the library. See THIRD-PARTY-NOTICES.md.
#
# BUILD SUFFIX. The libraries are named libavformat-plx.so.63 and so on, so they can never be
# confused with the system's own FFmpeg, and `ff.rs` opens them by absolute path from the app
# directory.
#
# NO RPATH, DELIBERATELY. FFmpeg's configure evals its flags, so `$ORIGIN` does not survive
# quoting. `ff.rs` dlopens the libraries in dependency order with RTLD_GLOBAL instead — libavutil,
# then libavcodec, then libavformat — so each is already loaded by SONAME when the next names it.
#
# Output: vendor/ffmpeg-prefix-host/lib (the Makefile stages the four libraries from there).
set -eu

VERSION=9.0
SHA256=7f607a00dd0d28a729d5a4811205812eef01cf6ef6155025febb6f36a9062d52
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFIX="$ROOT/vendor/ffmpeg-prefix-host"
WORK="$ROOT/vendor/ffmpeg-build-host"
TARDIR="$ROOT/vendor/ffmpeg-build"
SRC="$WORK/ffmpeg-$VERSION"

# Wipe the prefix whenever the version changes: `make install` only ADDS files, so a version bump
# would leave the previous major's libraries beside the new ones.
STAMP="$PREFIX/.plx-version"
if [ ! -f "$STAMP" ] || [ "$(cat "$STAMP")" != "$VERSION" ]; then
  rm -rf "$PREFIX"
  mkdir -p "$PREFIX"
  printf '%s' "$VERSION" > "$STAMP"
fi

# Components: exactly what the app uses.
#   demuxers   matroska/mov/mpegts containers, and raw Annex-B h264/hevc.
#   parsers    needed by avformat_find_stream_info to fill AVCodecParameters.
#   bsf        AVCC -> Annex-B for mp4-family video, which the GStreamer sink is fed.
#   decoders   SUBTITLES ONLY.
#
# `--disable-autodetect` keeps configure from folding in whatever external libraries the machine
# has. zlib is the one external library, and it is NAMED: the matroska demuxer only inflates a
# track's ContentCompression when built with zlib, and mkvmerge compresses every PGS and VobSub
# track with it by default (`ff_image_subtitle_tests.rs` holds this in place).
#
# `--prefix` is the literal /plx and the install is redirected with DESTDIR below: FFmpeg records
# its configure invocation inside libavutil, and a real prefix would bake the build path into it.
set -- --prefix=/plx \
  --build-suffix=-plx \
  --enable-shared --disable-static --enable-pic \
  --extra-cflags='-Dstatic_assert=_Static_assert' \
  --disable-programs --disable-doc --disable-avdevice --disable-avfilter \
  --disable-network --disable-swresample --disable-swscale \
  --disable-debug --enable-small \
  --disable-everything \
  --disable-autodetect \
  --enable-zlib \
  --enable-demuxer=matroska,mov,mpegts,h264,hevc \
  --enable-parser=h264,hevc,aac,ac3,dvdsub,dvbsub \
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata \
  --enable-decoder=pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt \
  --enable-protocol=file

mkdir -p "$WORK" "$TARDIR"
TAR="$TARDIR/ffmpeg-$VERSION.tar.xz"
tarball_ok() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$SHA256" ]; }
if ! tarball_ok "$TAR"; then
  echo "ffmpeg: fetching $VERSION"
  curl -fsSL "https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz" -o "$TAR.$$.part"
  mv "$TAR.$$.part" "$TAR"
fi
# Verify before extracting: this tarball becomes code inside the shipped app.
if ! tarball_ok "$TAR"; then
  echo "ffmpeg: SHA256 MISMATCH for $TAR (expected $SHA256)" >&2
  exit 1
fi

# Extract to a staging directory and rename, so an interrupted extraction never leaves a partial
# source tree that later runs would build against.
if [ ! -d "$SRC" ]; then
  stage="$WORK/.extract.$$"
  rm -rf "$stage"
  mkdir -p "$stage"
  tar xf "$TAR" -C "$stage"
  mv "$stage/ffmpeg-$VERSION" "$SRC"
  rm -rf "$stage"
fi

cd "$SRC"
# Reconfigure only when the flags change; configure is slow.
FLAGS_FILE="$SRC/.plx-flags"
NEW_FLAGS=$*
if [ ! -f "$FLAGS_FILE" ] || [ "$(cat "$FLAGS_FILE")" != "$NEW_FLAGS" ]; then
  echo "ffmpeg: configuring $VERSION"
  ./configure "$@" > "$WORK/configure.log" 2>&1 || {
    echo "ffmpeg: CONFIGURE FAILED — $WORK/configure.log" >&2; tail -20 "$WORK/configure.log" >&2; exit 1; }
  printf '%s' "$NEW_FLAGS" > "$FLAGS_FILE"
fi

echo "ffmpeg: building"
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" > "$WORK/build.log" 2>&1 || {
  echo "ffmpeg: BUILD FAILED — $WORK/build.log" >&2; tail -20 "$WORK/build.log" >&2; exit 1; }
rm -rf "$WORK/destdir"
make install DESTDIR="$WORK/destdir" > "$WORK/install.log" 2>&1
cp -R "$WORK/destdir/plx/." "$PREFIX/"

echo "ffmpeg: installed to $PREFIX"
for f in "$PREFIX"/lib/lib*-plx.so.*; do
  if [ -f "$f" ] && [ ! -L "$f" ]; then
    printf '  %-28s %6s KB\n' "$(basename "$f")" "$(( $(wc -c < "$f") / 1024 ))"
  fi
done
