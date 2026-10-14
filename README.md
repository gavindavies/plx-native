# PiTV

A native media player for the folders of films and shows on your own disk, built for the living
room: a Raspberry Pi or any Linux machine under the TV, driven by a remote or the arrow keys.

The interface is drawn straight on the GPU, and video is decoded by the machine's own GStreamer
(a hardware decoder where it has one: V4L2 on a Pi, VA-API on a PC) and composited under the UI.

![The library](design/screenshots/linux/01-library.jpg)

## What it does

- **Your library, from folders.** Point it at directories of films and shows; it lists them with
  their posters and remembers where you stopped in each file.
- **Kodi-style audio and subtitle choices per file.** Pick the audio track, the subtitle (embedded,
  or a `.srt`/`.ass` beside the video) and its timing; the choice is remembered for that file.
- **Lossless audio.** DTS-HD Master Audio, TrueHD, FLAC, Opus, E-AC-3, AC-3, AAC, MP3 and PCM, at
  the track's own channel count.
- **Styled subtitles.** ASS/SSA through libass, with the file's own fonts; image subtitles (PGS,
  VobSub) too.

| | |
|---|---|
| ![Films](design/screenshots/linux/02-films.jpg) | ![The hold menu](design/screenshots/linux/03-item-menu.jpg) |
| ![Player](design/screenshots/linux/05-player.jpg) | ![Subtitles](design/screenshots/linux/06-subtitles-menu.jpg) |
| ![Audio tracks](design/screenshots/linux/07-audio-menu.jpg) | ![A subtitle on screen](design/screenshots/linux/08-subtitle.jpg) |

## Building

Linux only. You need a nightly Rust toolchain, a C compiler, SDL2 and SDL2_ttf, and GStreamer with
its base, good, bad and libav plugin sets. On Debian, Ubuntu or Raspberry Pi OS:

```sh
sudo apt install build-essential pkg-config cmake nasm curl libsdl2-dev libsdl2-ttf-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
rustup toolchain install nightly
```

Then:

```sh
make             # builds the app into dist/linux/pitv (FFmpeg and libass the first time)
make check       # the test suite
```

The first `make` also builds the pinned FFmpeg and libass under `vendor/` and bundles them beside
the binary. `dist/linux/pitv` is self-contained apart from the system libraries above: copy it
anywhere and run `pitv` from it.

## Running

```sh
dist/linux/pitv/pitv                     # the library: ~/Videos
PITV_MEDIA_DIRS=/srv/films:/srv/tv dist/linux/pitv/pitv   # other folders, ':'-separated
dist/linux/pitv/pitv /path/to/film.mkv   # play one file
PITV_FULLSCREEN=0 dist/linux/pitv/pitv   # a window instead of the whole screen
```

Arrows move, Enter is OK, holding Enter opens a title's menu (play from the start, mark as
watched), and Escape (or Q) goes back. In the player, OK pauses and resumes, Left and Right seek,
and Up reaches the subtitle and audio buttons; a remote's media keys work too. Logs and the per-file progress live in `~/.local/state/pitv`.

## Demo library

`design/demo-library/generate.sh` makes a small library of invented films and a show, from
nothing but `ffmpeg` and the app's own fonts: posters, video, English and Spanish audio, and
subtitle files beside each video. It is what the screenshots above show.

```sh
design/demo-library/generate.sh          # into design/demo-library/library (not committed)
PITV_MEDIA_DIRS=design/demo-library/library dist/linux/pitv/pitv
```

## Status

PiTV began as a fork of PlxNative, a Plex client for LG televisions. The Plex client, its screens
and the LG webOS build are gone. Internal crate names still carry the old `plx` prefix, and some
webOS platform code below the player (device capability probes, the webOS key and storage layers)
is still in the tree and has yet to be removed.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE.md](NOTICE.md).
