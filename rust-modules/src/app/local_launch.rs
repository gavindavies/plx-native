//! **Open a file from the command line**: `plxnative-sim /path/to/film.mkv` plays it.
//!
//! The Linux build's equivalent of handing a media player a file. The binary records the path
//! before the app starts ([`set`]); once the app has drawn its first frames, [`tick`] probes the
//! file and starts it through the ordinary playback entry (`playback::start_playback`), so the
//! player, its HUD and its exit behave exactly as for a library item. A file that cannot play is
//! reported on screen (the app's toast) and in the event log, and the app carries on.

use std::path::PathBuf;
use std::sync::Mutex;

static PENDING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Settle time after boot before the launch: the window and the first page are up by then.
const AFTER_BOOT_MS: u32 = 300;

/// Record the file to play once the app is running.
pub fn set(path: PathBuf) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

pub(crate) fn tick(app: &mut super::App, now: u32) {
    if now.wrapping_sub(app.t0) < AFTER_BOOT_MS {
        return;
    }
    let Some(path) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    super::local_watch::play(&mut app.player.session, &mut app.adapters.player, &path, false, None,
        &mut app.pages, &mut app.bridge);
}
