//! **Follows a local file's playback and keeps its place** (`plx_data::local_progress`).
//!
//! A Plex playback reports its progress to the server (`/:/timeline`, the scrobble); a local file
//! has nobody to report to, so this records it instead. [`begin`] is called when a local file is
//! handed to the player; from then [`tick`] reads the player's position once a frame while the
//! player page is up, saves it every [`SAVE_EVERY_MS`] (so a crash or a power cut loses little),
//! and settles the record when the player page closes — with the player's own end-of-stream
//! ([`note_ended`]), so a film watched to the end is marked watched however long its credits are.

use plx_data::local_progress;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How often a running playback's place is saved.
const SAVE_EVERY_MS: u32 = 10_000;
/// A player that never opens (the Load was refused) is forgotten after this long.
const MOUNT_WAIT_MS: u32 = 20_000;

struct Watch {
    path: PathBuf,
    size: u64,
    /// The frame time [`begin`] ran, until the player first shows; then the last save's.
    since: Option<u32>,
    mounted: bool,
    position_ms: u64,
    duration_ms: u64,
    ended: bool,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// A local file (`size` bytes) has been handed to the player. Replaces any playback still being
/// followed, which is settled first, as its player is being replaced.
pub(crate) fn begin(path: PathBuf, size: u64) {
    let mut watch = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = watch.take() {
        settle(&old);
    }
    *watch = Some(Watch { path, size, since: None, mounted: false, position_ms: 0, duration_ms: 0, ended: false });
}

fn settle(w: &Watch) {
    if w.mounted && (w.position_ms > 0 || w.ended) {
        local_progress::record(&w.path, w.size, w.position_ms, w.duration_ms, w.ended);
        plx_base::eventlog::log(&format!(
            "local progress: {} stopped at {}s of {}s{}",
            w.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            w.position_ms / 1_000,
            w.duration_ms / 1_000,
            if w.ended { " (end of stream)" } else { "" }
        ));
    }
}

/// Play the local file at `path`: resumed where it was left unless `from_start`. Probes it first
/// (a local disk answers in milliseconds, and the press that asked is this frame's only work); a
/// file that cannot play is refused on screen and nothing else happens. `ret` is where BACK out of
/// the player returns to.
pub(crate) fn play(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    path: &Path,
    from_start: bool,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    if !super::content::clear_engine_for_play(ps, pa, super::bridge::player(pages).is_some()) {
        return;
    }
    plx_media::route::clear_url(ps);
    let probe = match plx_media::localplay::prepare(ps, path) {
        Ok(probe) => probe,
        Err(e) => {
            plx_base::eventlog::log(&format!("local play: refused: {e}"));
            super::toast::raise(e.message());
            return;
        }
    };
    // The probe just read this file, so the size is a cached stat away.
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let resume_ms = if from_start { 0 } else { local_progress::resume_ms(path, size) };
    begin(path.to_path_buf(), size);
    // The subtitles the track menu offers: the file's own tracks and the files beside it.
    let item = plx_media::localplay::playing_item(path, &probe, &plx_data::local::sidecars(path));
    super::playback::start_playback(ps, pa, resume_ms.saturating_mul(1_000_000) as i64,
        super::playback::Origin::Here, plx_appkit::player_hud::LINGER_MS, ret, pages, bridge);
    // After the start: a new play retires the previous item's store as it begins.
    if let Some(audio) = plx_media::localplay::starting_audio(&item, &probe) {
        plx_media::route::set_cur_audio(ps, audio);
    }
    bridge.metadata_mut().run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(Some(item)));
}

/// The item menu's rows for a local video (`ItemMenuKind::Local`): its `rk` is the file's path.
pub(crate) fn apply_menu_action(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    act: plx_screens::item_menu::Action,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    use plx_screens::item_menu::Action;
    let path = PathBuf::from(act.rk());
    let size = || std::fs::metadata(&path).map_or(0, |m| m.len());
    match act {
        Action::PlayFromStart(_) => play(ps, pa, &path, true, None, pages, bridge),
        Action::MarkWatched(_) => local_progress::set_watched(&path, size(), true),
        Action::MarkUnwatched(_) => local_progress::set_watched(&path, size(), false),
        // No other row is built for a local video (`item_menu::build_episode`).
        _ => {}
    }
}

/// The player ran to the end of the stream. The frame loop says so as it closes the player,
/// which clears the player's own flag in the same frame, before [`tick`] could read it.
pub(crate) fn note_ended() {
    if let Some(w) = WATCH.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        w.ended = true;
    }
}

/// Once a frame.
pub(crate) fn tick(app: &super::App, now: u32) {
    let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = guard.as_mut() else { return };
    let since = *w.since.get_or_insert(now);
    if super::bridge::player(&app.pages).is_none() {
        if w.mounted || now.wrapping_sub(since) >= MOUNT_WAIT_MS {
            // The player closed (or never opened): settle and stop following.
            if let Some(w) = guard.take() {
                settle(&w);
            }
        }
        return;
    }
    if !w.mounted {
        w.mounted = true;
        w.since = Some(now);
    }
    // Only a position the viewer has seen: before the first picture the clock is not this file's.
    if plx_media::player::seen_frame() {
        let pos = plx_media::player::playpos_ns();
        let dur = plx_media::player::duration_ns();
        if pos > 0 {
            w.position_ms = (pos / 1_000_000) as u64;
        }
        if dur > 0 {
            w.duration_ms = (dur / 1_000_000) as u64;
        }
    }
    if plx_media::player::ended() {
        w.ended = true;
    }
    if now.wrapping_sub(w.since.unwrap_or(now)) >= SAVE_EVERY_MS && w.position_ms > 0 {
        local_progress::record(&w.path, w.size, w.position_ms, w.duration_ms, w.ended);
        w.since = Some(now);
    }
}
