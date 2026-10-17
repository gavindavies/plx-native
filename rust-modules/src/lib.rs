//! PiTV — a lightweight 4K video player for a laptop under the TV: the files in local folders and
//! mounted SMB shares, played with GStreamer.
//! Licensed under GPL-3.0-or-later; see LICENSE at the repository root, and THIRD-PARTY-NOTICES.md
//! for the components this links or redistributes.
//!
//! plxnative-modules — the application crate: the event loop (`app`), the headless test harness
//! (`remote`, `shot`) and the dev trigger surface (`dev`). `src/bin/pitv.rs` is the binary.
mod app; // run_application — the app core / event loop
mod dev; // the runtime-dir trigger surface, behind one `devtriggers` feature — read it before adding a trigger
mod remote; // the remote-control channel: a FIFO the loop drains into synthetic SDL keys
mod shot; // screenshots: read the frame back and write a PNG (see the module doc)

/// Play `path` once the app is running — `pitv <file>` (see `app::local_launch`).
pub fn set_launch_file(path: std::path::PathBuf) {
    app::local_launch::set(path);
}

/// The instance root (`PITV_RUNTIME_DIR`): the binary creates it and truncates the event log
/// inside it before the app starts.
pub fn runtime_dir() -> std::path::PathBuf {
    plx_base::paths::runtime_dir().to_path_buf()
}

/// The event log's path, built by the ONE expression [`plx_base::eventlog::log`] uses.
pub fn events_log() -> std::path::PathBuf {
    plx_base::eventlog::events_log()
}

/// Run the application until it quits; the process exit code.
pub fn run() -> std::os::raw::c_int {
    app::run_application()
}
