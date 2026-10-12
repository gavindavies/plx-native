# PlxNative for Linux

A build of the app for an ordinary Linux machine connected to a television — a PC or a
single-board computer under the TV — that plays the video files on that machine's own disk. It is
the same application core as the webOS build, with two differences:

- **Playback goes through the system GStreamer** (`rust-modules/media/src/player/gst_sink.rs`)
  instead of LG's StarfishMediaAPIs. The demuxer, the access-unit queues, the player HUD and its
  controls are the television's; the decoder (hardware when GStreamer has one — VA-API, V4L2) and
  the audio output (decoded to PCM) are the machine's.
- **Its home is a local library** (`rust-modules/screens/src/local.rs`) instead of Plex: a folder
  browser over the folders it is pointed at. There is no account and no server.

It is built with the `gst` cargo feature, which also turns on the screens crate's `local` feature;
neither affects the television or the desktop simulator.

## Requirements

Ubuntu 24.04 package names:

```sh
sudo apt install build-essential cmake pkg-config curl \
  libsdl2-dev libsdl2-ttf-dev libgl1-mesa-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
```

and Rust (`rustup`, the stable toolchain is enough for this build).

## Build

```sh
make linux
```

builds the bundled FFmpeg and libass for this machine the first time (`HOST=1 ci/build-ffmpeg.sh`,
`HOST=1 ci/build-libass.sh`), then the app, and stages everything it loads into one directory,
`dist/linux/plxnative/` — copy that directory anywhere. `LINUX_DEV=1 make linux` keeps the
developer surface (the frame counter and the `/…/plxnative-*` triggers and remote FIFO).

## Run

```sh
dist/linux/plxnative/plxnative                    # the library
dist/linux/plxnative/plxnative /path/to/film.mkv  # play one file
```

| Variable | Meaning |
| --- | --- |
| `PLXNATIVE_MEDIA_DIRS` | Library folders, `:`-separated. Default `~/Videos`. One folder is shown as its contents; several as one card each. |
| `PLXNATIVE_FULLSCREEN=0` | A window instead of the whole screen. |
| `PLXNATIVE_GST_AUDIOSINK` | A GStreamer sink description for audio (default `autoaudiosink`), e.g. `alsasink device=hdmi:0`. |
| `PLXNATIVE_RUNTIME_DIR` | Where logs and state go. Default `$XDG_STATE_HOME/plxnative`, else `~/.local/state/plxnative`. |

Keys: arrows move, Enter/OK opens a folder or plays a video, Esc/Backspace goes back.

## The library

A folder lists its subfolders, then its videos (`mkv mp4 m4v mov ts m2ts avi webm mpg`), in natural
order (`Episode 2` before `Episode 10`); hidden entries are skipped. Artwork follows the usual
media-folder conventions: `poster`, `folder` or `cover` (`.jpg`/`.jpeg`/`.png`) inside a folder is
that folder's picture; `<name>-poster.jpg` or `<name>.jpg` beside a video is the video's; a video
alone in its folder wears the folder's.

## What plays

Video: H.264, HEVC, AV1 and VP9, each when this machine's GStreamer has a decoder for it.
Audio: AC3, E-AC3, DTS (core), AAC, FLAC, Opus, Vorbis, TrueHD, MP3, MP2 and plain PCM — the
first track in the file in one of these is played, decoded to PCM. A file with no audio track
plays silent. A file that cannot play is refused with the reason shown on screen (the Linux
build draws the toasts a television would show) and written to the event log.

## What does not work yet

- No resume position or watched state, no external subtitles, no HDMI-CEC remote.
- Video is copied to the GPU as RGBA each frame, scaled to fit 1920×1080 first: fine for 1080p on a
  PC, not yet measured on a small ARM board.
