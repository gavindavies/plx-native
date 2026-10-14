//! plx_media: the media layer of the application core.
//!
//! Demux (`ff`, FFmpeg over the local-disk plane `localfile`), the access-unit queues (`aq`), the
//! buffer-feed engine and its GStreamer sink (`player`), the playback session (`playback`) and
//! the local-file start (`localplay`). It is the `media` layer of `ci/module-layers.ini`: it uses
//! `base`, `machine`, `platform`, `gfx` and `data` and nothing above them.
//!
//! `test-support` exposes the `cfg(test)` seams the layers above test through
//! (`player::restore_state_for_test`, ...). The application crate enables it in
//! `[dev-dependencies]` only, so no shipped build sees it.

pub mod aq;
#[macro_use]
pub mod ff; // the demuxer: FFmpeg, dlopen'd by absolute path from the application's own directory
pub mod localfile; // the local-disk media plane: a file on this machine read by pread
pub mod localplay; // a file on this machine's disk, probed and installed as the next Load
pub mod thumbs; // scrub preview frames: a local file's pictures, decoded off the frame thread
pub mod playback; // the playback session: the stream, its declaration, the tracks, the Load reducer
pub mod player; // the buffer-feed engine and its GStreamer sink
