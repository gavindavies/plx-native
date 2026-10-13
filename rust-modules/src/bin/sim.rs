//! The application binary: plays the video files on this machine's disk, driven by a remote (or
//! a keyboard), full screen on the television this machine is plugged into.
//!
//! Usage:
//!   plxnative-sim                  the library, from the folders in PLXNATIVE_MEDIA_DIRS
//!   plxnative-sim <media-file>     play one file
//! Environment:
//!   PLXNATIVE_MEDIA_DIRS     the library's folders, `:`-separated
//!   PLXNATIVE_FULLSCREEN=0   a desktop window instead of full screen
//!   PLXNATIVE_RUNTIME_DIR    this instance's trigger/FIFO/log root
//!   PLXNATIVE_APP_DIR        where appfont*.ttf and the icons live (repo `pkg/`)

use plxnative_modules::port::plex_run;
use std::os::raw::c_int;

fn main() {
    if let Some(arg) = std::env::args().nth(1) {
        let path = std::path::Path::new(&arg);
        if !path.is_file() {
            eprintln!("plxnative-sim: {arg}: not a file");
            std::process::exit(2);
        }
        plxnative_modules::set_launch_file(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    let root = plxnative_modules::sim_runtime_dir();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("plxnative-sim: cannot create runtime dir {}: {e}", root.display());
        std::process::exit(1);
    }
    let events = plxnative_modules::sim_events_log();
    let banner = "linux: PlxNative for Linux (GStreamer playback, local library). Frame rates here \
                  describe this machine's GPU.\n";
    if let Err(e) = std::fs::write(&events, banner) {
        eprintln!("plxnative-sim: cannot truncate {}: {e}", events.display());
        std::process::exit(1);
    }
    eprintln!("plxnative-sim: runtime={} log={}", root.display(), events.display());

    let rc = plex_run(std::ptr::null(), 0);
    exit_without_atexit_teardown(rc)
}

/// Leave without running C `atexit` handlers: GStreamer's and the GL driver's teardown can race
/// worker threads that are still winding down, and the process has nothing left to flush.
fn exit_without_atexit_teardown(rc: c_int) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { libc::_exit(rc) }
}
