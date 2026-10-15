# Project instructions

## Project

PiTV is a lightweight 4K video player (HEVC/H.264 in MKV/MP4, lossless audio, ASS and image
subtitles) for a **Raspberry Pi 5** under the TV, playing the files in local folders and mounted
SMB shares, driven by a remote or the arrow keys. It is Linux only. The interface is drawn with
GLES2 through SDL2; video is demuxed by a bundled FFmpeg and decoded by the system GStreamer (V4L2
hardware decode on the Pi), composited under the UI.

It began as PlxNative, a Plex client for LG webOS TVs; the Plex client and the webOS port are gone.
Crate names keep the old `plx_` prefix. Keep changes properly factored and finished.

## Layout

All Rust is the cargo workspace in `rust-modules/` (nightly toolchain). Layers, lowest first — a
crate names only crates below it:

| crate | dir | what |
|---|---|---|
| `plx_base` | `base/` | event log, paths, spawn discipline, dlopen, font coverage, dev triggers |
| `plx_machine` | `machine/` | the machine/effect contract, present gate, idle wake, motion springs |
| `plx_platform` | `platform/` | the English string catalog (`i18n::msg`, from `locales/en`) and the artwork cache |
| `plx_gfx` | `gfx/` | GLES2 renderer, text (SDL2_ttf), images, SVG icons (`src/svg.c`, nanosvg) |
| `plx_ui` | `ui/` | the UI library: theme, widgets, cards, tables/forms, focus, containers, dispatcher |
| `plx_data` | `data/` | the local library, `.nfo` metadata, watch progress, player settings |
| `plx_media` | `media/` | FFmpeg demuxer (`ff.rs`), the player engine and its GStreamer sink, thumbnails |
| `plx_appkit` | `appkit/` | widgets several screens share: player HUD, track menus, info card |
| `plx_screens` | `screens/` | the screens: Home, folder pages, title page, item menu, player, Settings |
| `plxnative-modules` | `src/` | the app: event loop (`app/`), remote FIFO and screenshots; binary `src/bin/pitv.rs` |

Outside it: `src/ass.c` + `include/ass.h` (the libass facade), `vendor/nanosvg`, `locales/en`,
`pkg/` (the Inter fonts), `ci/` (pinned FFmpeg and libass builds), `tools/linux/pitv` (the
launcher), `design/` (design system, specs, screenshots, demo library — do not edit it as part of
code changes).

Before UI work read `rust-modules/ui/src/CLAUDE.md`; before touching a screen
`rust-modules/screens/src/CLAUDE.md`; before player work `rust-modules/media/src/player/CLAUDE.md`.

## UI rules

1. **Never write a raw color literal** — every colour is a named `theme.rs` token.
2. **Never write a raw text size** — every size is a `theme::size` rung.
3. **Never hand-place text with a magic y** — position text by its cap band (`Label`, `TextView`,
   `text::text_vcenter_y`).
4. **Improve a component before forking one** — extend the shared widget; a new one lands in `ui/`.

## Build and test

Every direct cargo call is prefixed `CARGO_INCREMENTAL=0`, run from `rust-modules/`:

```sh
CARGO_INCREMENTAL=0 cargo +nightly test --workspace --lib          # the test suite
CARGO_INCREMENTAL=0 cargo +nightly check --lib --bins --no-default-features   # shipping features
CARGO_INCREMENTAL=0 cargo +nightly build --release --no-default-features --features devtriggers --bin pitv
```

From the repository root, `make` builds the app into `dist/linux/pitv` (building the pinned FFmpeg
and libass into `vendor/` the first time), `make check` runs the tests and the shipping-feature
check, `make run` opens it in a window.

Warnings are errors (`[workspace.lints] warnings = "deny"`). Fixing a reported bug starts with a
test that fails against the broken behaviour.

Features: `devtools` (on-screen frame counter), `devtriggers` (the remote FIFO, screenshots and the
`pitv-*` trigger files in the runtime directory), `threadcheck` (louder main-thread checks). All
three are default-on for development; a shipping build uses `--no-default-features`.

## Headless run

The app runs under Xvfb. `PITV_APP_DIR` holds `appfont*.ttf`, the icons and the bundled
`lib*-plx.so*` libraries (`dist/linux/pitv` after `make`); `PITV_RUNTIME_DIR` is where the event
log, the remote FIFO and screenshots go.

```sh
R=$(mktemp -d)
PITV_APP_DIR=dist/linux/pitv PITV_RUNTIME_DIR=$R PITV_MEDIA_DIRS=/path/to/videos PITV_FULLSCREEN=0 \
  xvfb-run -a -s "-screen 0 1920x1080x24" rust-modules/target/release/pitv &
sleep 10; echo shot > $R/pitv-remote     # writes $R/shot-1.png
echo ok > $R/pitv-remote                 # tokens: up down left right ok back okdown okup shot ...
pkill -x pitv
```

`PITV_MEDIA_DIRS` replaces the library config for the run; to exercise `library.conf` (SMB shares
mounted through gio) instead, leave it unset, point `PITV_CONFIG` at a file and run the app under
`dbus-run-session` with `XDG_RUNTIME_DIR` set (`rust-modules/data/src/library.rs`).
`PITV_SHOT*` and `PITV_RENDER_SCALE` (supersampled screenshots) are documented in
`rust-modules/src/shot.rs`; the event log is `$R/pitv-events.log`. Use `pkill -x pitv`, never
`pkill -f`.

## Working rules

- **Trunk-based, and `main` takes squash merges only.** Work on short-lived branches; a change
  lands on `main` as ONE squash commit whose message is the change's own account. Before pushing a
  squash, check `git diff --stat HEAD^ HEAD` touches only the paths the branch meant to touch.
- Preserve unrelated user changes; never clean or reset a dirty tree to make a task easier.
- `git add` explicit paths; keep cargo target directories outside the checkout.
