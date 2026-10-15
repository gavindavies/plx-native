//! The application binary: plays the video files on this machine's disk, driven by a remote (or
//! a keyboard), full screen on the television this machine is plugged into.
//!
//! Usage:
//!   pitv                  the library, from the folders in PITV_MEDIA_DIRS
//!   pitv <media-file>     play one file
//! Environment:
//!   PITV_MEDIA_DIRS     the library's folders, `:`-separated
//!   PITV_FULLSCREEN=0   a desktop window instead of full screen
//!   PITV_RUNTIME_DIR    this instance's trigger/FIFO/log root
//!   PITV_APP_DIR        where appfont*.ttf and the icons live (repo `pkg/`)


use std::os::raw::c_int;

fn main() {
    // first act: the version the log and Settings > About report
    plx_base::identity::set_version(env!("PLX_VERSION"));
    if let Some(arg) = std::env::args().nth(1) {
        let path = std::path::Path::new(&arg);
        if !path.is_file() {
            eprintln!("pitv: {arg}: not a file");
            std::process::exit(2);
        }
        plxnative_modules::set_launch_file(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    let root = plxnative_modules::runtime_dir();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("pitv: cannot create runtime dir {}: {e}", root.display());
        std::process::exit(1);
    }
    let events = plxnative_modules::events_log();
    let banner = "linux: PiTV (GStreamer playback, local library). Frame rates here \
                  describe this machine's GPU.\n";
    if let Err(e) = std::fs::write(&events, banner) {
        eprintln!("pitv: cannot truncate {}: {e}", events.display());
        std::process::exit(1);
    }
    eprintln!("pitv: runtime={} log={}", root.display(), events.display());

    let rc = plxnative_modules::run();
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
