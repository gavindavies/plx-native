# player/ — the buffer-feed video engine

The engine demuxes a local file (or a file on a mounted SMB share) to access units with the
bundled FFmpeg (`../ff.rs`), queues them per lane with byte-cap backpressure (`../aq.rs`) and
feeds them to a **video sink** (`sink.rs`'s `VideoSink`). The product sink is `gst_sink.rs`: one
`appsrc` per lane, the codec's parser, `decodebin` (a V4L2 hardware decoder on a Raspberry Pi 5,
VA-API on a PC), video converted to RGBA into an `appsink` that the render thread composites
UNDER the finished UI, audio decoded to PCM into the system audio sink. Read `gst_sink.rs`'s `//!`
before changing it.

The engine was written against a television's buffer-feed media API, and its verbs still have
that shape: a JSON Load payload, `feed`, an event callback (`sf_on_event`: type 0 = position,
2 = load completed, 18 = pipeline error, 23 = unloaded) and video-plane bind stages the GStreamer
sink answers as no-ops (`VP_EXPORTED`). A seek on this sink is a fresh Load, so each Load builds a
fresh pipeline.

## Pipeline and threads

- `engine.rs` — the **main-thread-confined** session object; spawns the workers below.
- `pump.rs` — the **main-thread pump**: each frame it drives Load → Play → feed and services seeks.
- `threads.rs` — `load_thread` (the sink's Load, off the main thread by design) and the timeline
  worker. The demux thread body is `ff::demux`: open the file, read and convert packets, push AUs
  to the two lanes, and service seeks between two `av_read_frame` calls.
- `shared.rs` — the engine's cross-thread transport, callback and clock state. Put new state under
  the owner whose invariant it belongs to; never smuggle it through a raw static.
- `ass*.rs`, `sidecar.rs` — subtitles: the bundled libass facade (`src/ass.c`) for ASS/SSA, image
  subtitles (PGS, VobSub) and sidecar `.srt`/`.ass` files beside the video.
- `ffi_host.rs` — `HostSink`, the clock-only sink the host tests run the engine against (nothing
  decodes; a presentation clock advances at real time).

**The main-thread rule is compiler-enforced.** Every sink verb but `load`, `window_mode` and
`window_id` takes a `task::MainThread` — a `!Send` token minted once at boot and moved into
`player::adapter::PlayerAdapter` (`App.adapters.player`), which also owns the native session. Take
the token iff you reach the sink, the adapter iff you reach the session.

## Verifying playback changes

`cargo +nightly test --workspace --lib` covers the engine's pure logic against `HostSink`
(`engine.rs`'s load/seek/teardown tests, `ff.rs`'s packet framing, the subtitle paths). Real
decoding is only observable in the running app: build `pitv`, play a file headless (`AGENTS.md`,
"Headless run") and read `pitv-events.log` in the runtime directory (feed stats, Load, seek/rebase,
GStreamer errors). `pitv-gstlog` in the runtime directory (devtriggers builds) turns on
`GST_DEBUG` into `pitv-gst.log`.
