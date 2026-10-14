//! **Where each local file was left, and whether it was finished** — the local library's answer to
//! the `viewOffset` and `viewCount` a Plex server keeps.
//!
//! One record per file, keyed by its absolute path and kept with the file's size, so a file
//! replaced by another of the same name does not inherit the old one's place. The records live in
//! memory and in one JSON file under the persistent state root (`local-progress.json`), read once
//! at boot ([`load`], blocking) and rewritten whole on a worker after every change ([`record`],
//! [`set_watched`]): a few hundred bytes per film, written a handful of times per playback.
//!
//! The rules a Plex server applies are applied here, so the two libraries behave alike:
//!
//! * a playback that reaches [`FINISHED_SHARE`] of the file, or its end, marks it **watched** and
//!   forgets the place (the next play starts at the beginning);
//! * otherwise the place is kept when it is at least [`MIN_RESUME_MS`] in, and the next play
//!   resumes there ([`resume_ms`]);
//! * watched stays watched until the viewer says otherwise ([`set_watched`]): watching the start
//!   of a finished film again keeps both the tick and the new place.
//!
//! [`revision`] moves on every change, so a screen drawing the marks redraws when one lands.
//!
//! Beside each place the file keeps the viewer's own **choices** for that file ([`Choices`]: the
//! audio track, the subtitle and its timing), Kodi-style, so reopening a film plays it the way it
//! was left. They are a separate map, never folded into the watch rules: marking a film unwatched
//! forgets where it was, not which language it is watched in.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

/// The share of a file after which a stop counts as having finished it.
pub const FINISHED_SHARE: f64 = 0.9;
/// A place nearer the start than this is not worth resuming.
pub const MIN_RESUME_MS: u64 = 30_000;

const FILE_NAME: &str = "local-progress.json";

/// One file's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Where to resume, 0 when there is nowhere to resume (never started, or finished).
    pub offset_ms: u64,
    /// The file's length as last played, 0 when unknown.
    pub duration_ms: u64,
    pub watched: bool,
    /// The file's size when this was recorded; a different size is a different file.
    pub size: u64,
}

/// The subtitle a file was left on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleChoice {
    Off,
    /// The file's own subtitle track, by its position among them (file order).
    Track(u32),
    /// A subtitle file beside it, by file name.
    File(String),
}

/// What the viewer chose for one file. `None` is "never chosen": the file's own default then.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choices {
    /// The audio track, by its position among the file's audio tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<SubtitleChoice>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub subtitle_offset_ms: i64,
    /// The file's size when chosen, as [`Progress::size`].
    pub size: u64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Default, Serialize, Deserialize)]
struct Disk {
    version: u32,
    items: HashMap<String, Progress>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    choices: HashMap<String, Choices>,
}

static ITEMS: Mutex<Option<HashMap<String, Progress>>> = Mutex::new(None);
static CHOICES: Mutex<Option<HashMap<String, Choices>>> = Mutex::new(None);
static REVISION: AtomicU64 = AtomicU64::new(0);
/// Serializes the file writes, so an older snapshot can never land after a newer one.
static WRITER: Mutex<()> = Mutex::new(());

fn file() -> PathBuf {
    plx_base::paths::persistent_state_root().join(FILE_NAME)
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Read the records from disk. Blocking: call it at boot, before the frame loop. A missing or
/// unreadable file is an empty library, logged when it is not simply absent.
pub fn load() {
    let _block = plx_base::task::assert_may_block(const { &plx_base::task::BlockingLabel::new("local progress load") });
    let disk = match std::fs::read(file()) {
        Ok(bytes) => match serde_json::from_slice::<Disk>(&bytes) {
            Ok(d) => d,
            Err(e) => {
                plx_base::eventlog::log(&format!("local progress: {FILE_NAME} unreadable ({e}); starting empty"));
                Disk::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Disk::default(),
        Err(e) => {
            plx_base::eventlog::log(&format!("local progress: {FILE_NAME} unreadable ({e}); starting empty"));
            Disk::default()
        }
    };
    plx_base::eventlog::log(&format!("local progress: {} record(s), {} with choices", disk.items.len(), disk.choices.len()));
    *ITEMS.lock().unwrap_or_else(|e| e.into_inner()) = Some(disk.items);
    *CHOICES.lock().unwrap_or_else(|e| e.into_inner()) = Some(disk.choices);
    REVISION.fetch_add(1, Relaxed);
}

/// Moves whenever a record changes.
pub fn revision() -> u64 {
    REVISION.load(Relaxed)
}

/// The record for `path`, when there is one for the file now at that path. `size` is the file's
/// current size (the library's listing already has it; pass 0 to skip the check).
pub fn get(path: &Path, size: u64) -> Option<Progress> {
    let items = ITEMS.lock().unwrap_or_else(|e| e.into_inner());
    let p = *items.as_ref()?.get(&key(path))?;
    (size == 0 || p.size == 0 || p.size == size).then_some(p)
}

/// Where the next play of `path` starts, milliseconds; 0 for the beginning.
pub fn resume_ms(path: &Path, size: u64) -> u64 {
    get(path, size).map_or(0, |p| p.offset_ms)
}

/// The resume bar's fraction for a record: how far in the kept place is.
pub fn fraction(p: &Progress) -> Option<f32> {
    (p.offset_ms > 0 && p.duration_ms > 0).then(|| (p.offset_ms as f64 / p.duration_ms as f64).clamp(0.0, 1.0) as f32)
}

/// What a playback that stopped at `position_ms` of `duration_ms` leaves behind, given what was
/// recorded before. Pure: [`record`] applies it.
pub fn after_stop(before: Option<Progress>, position_ms: u64, duration_ms: u64, reached_end: bool, size: u64) -> Progress {
    let was_watched = before.is_some_and(|p| p.watched);
    let finished = reached_end || (duration_ms > 0 && position_ms as f64 >= duration_ms as f64 * FINISHED_SHARE);
    let offset_ms = if finished || position_ms < MIN_RESUME_MS { 0 } else { position_ms };
    Progress { offset_ms, duration_ms, watched: finished || was_watched, size }
}

/// Record where a playback of `path` (`size` bytes) stands. Called while it plays (so a crash
/// loses little) and once when it stops; `reached_end` is the player's own end-of-stream. Touches
/// no disk on the caller's thread: the size comes from whoever already read it.
pub fn record(path: &Path, size: u64, position_ms: u64, duration_ms: u64, reached_end: bool) {
    let next = after_stop(get(path, size), position_ms, duration_ms, reached_end, size);
    put(path, next);
}

/// The viewer's own verdict: watched (place forgotten) or not watched (from the beginning).
pub fn set_watched(path: &Path, size: u64, watched: bool) {
    let duration_ms = get(path, size).map_or(0, |p| p.duration_ms);
    put(path, Progress { offset_ms: 0, duration_ms, watched, size });
}

/// The viewer's choices for the file now at `path` (`size` bytes), when any were made.
pub fn choices(path: &Path, size: u64) -> Option<Choices> {
    let choices = CHOICES.lock().unwrap_or_else(|e| e.into_inner());
    let c = choices.as_ref()?.get(&key(path))?.clone();
    (size == 0 || c.size == 0 || c.size == size).then_some(c)
}

/// Keep the viewer's choices for `path`. Saved like a place: on a worker, the whole file.
pub fn set_choices(path: &Path, choices: Choices) {
    {
        let mut all = CHOICES.lock().unwrap_or_else(|e| e.into_inner());
        let all = all.get_or_insert_with(HashMap::new);
        if all.get(&key(path)) == Some(&choices) {
            return;
        }
        all.insert(key(path), choices);
    }
    save();
}

fn put(path: &Path, p: Progress) {
    {
        let mut items = ITEMS.lock().unwrap_or_else(|e| e.into_inner());
        let items = items.get_or_insert_with(HashMap::new);
        if items.get(&key(path)) == Some(&p) {
            return;
        }
        if p == (Progress { offset_ms: 0, duration_ms: p.duration_ms, watched: false, size: p.size }) {
            items.remove(&key(path));
        } else {
            items.insert(key(path), p);
        }
    }
    REVISION.fetch_add(1, Relaxed);
    plx_machine::idle::invalidate();
    save();
}

/// Write the current records on a worker: the latest state at the moment the write runs, under
/// one lock, through a temporary file renamed over the old one.
fn save() {
    let spawned = plx_base::task::spawn_small("local progress save", || {
        let _writer = WRITER.lock().unwrap_or_else(|e| e.into_inner());
        let disk = Disk {
            version: 1,
            items: ITEMS.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default(),
            choices: CHOICES.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default(),
        };
        let json = match serde_json::to_vec_pretty(&disk) {
            Ok(j) => j,
            Err(e) => {
                plx_base::eventlog::log(&format!("local progress: not saved ({e})"));
                return;
            }
        };
        let path = file();
        let tmp = path.with_extension("json.tmp");
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&tmp, &json))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            plx_base::eventlog::log(&format!("local progress: not saved to {} ({e})", path.display()));
        }
    });
    if !spawned {
        plx_base::eventlog::log("local progress: could not start the save worker; not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    #[test]
    fn a_stop_part_way_keeps_the_place() {
        let p = after_stop(None, 40 * MIN, 100 * MIN, false, 7);
        assert_eq!(p, Progress { offset_ms: 40 * MIN, duration_ms: 100 * MIN, watched: false, size: 7 });
        assert_eq!(fraction(&p), Some(0.4));
    }

    #[test]
    fn a_stop_near_the_start_is_not_worth_resuming() {
        let p = after_stop(None, MIN_RESUME_MS - 1, 100 * MIN, false, 7);
        assert_eq!(p.offset_ms, 0);
        assert!(!p.watched);
        assert_eq!(fraction(&p), None);
    }

    #[test]
    fn the_credits_or_the_end_finish_it() {
        let credits = after_stop(None, 91 * MIN, 100 * MIN, false, 7);
        assert!(credits.watched && credits.offset_ms == 0);
        let end = after_stop(None, 5 * MIN, 0, true, 7);
        assert!(end.watched && end.offset_ms == 0, "an end-of-stream finishes even with no duration");
    }

    #[test]
    fn rewatching_the_start_keeps_the_tick_and_the_new_place() {
        let before = Progress { offset_ms: 0, duration_ms: 100 * MIN, watched: true, size: 7 };
        let p = after_stop(Some(before), 10 * MIN, 100 * MIN, false, 7);
        assert!(p.watched);
        assert_eq!(p.offset_ms, 10 * MIN);
    }

    #[test]
    fn a_record_follows_the_file_not_the_name() {
        let _serial = plx_base::testlock::serial();
        let dir = plx_base::testscratch::process_dir("local-progress-test");
        plx_base::paths::redirect_persistent_state_root_for_test(Some(dir.join("state")));
        let film = dir.join("film.mkv");
        record(&film, 10, 40 * MIN, 100 * MIN, false);
        assert_eq!(resume_ms(&film, 10), 40 * MIN);
        assert_eq!(resume_ms(&film, 11), 0, "a file of another size is another file");
        set_watched(&film, 10, true);
        assert!(get(&film, 10).unwrap().watched);
        assert_eq!(resume_ms(&film, 10), 0);
        set_watched(&film, 10, false);
        assert_eq!(get(&film, 10), None, "unwatched and unstarted is no record at all");
        let chosen = Choices { audio: Some(1), subtitle: Some(SubtitleChoice::File("film.de.srt".into())), subtitle_offset_ms: -500, size: 10 };
        set_choices(&film, chosen.clone());
        assert_eq!(choices(&film, 10), Some(chosen), "choices outlive the watch record");
        assert_eq!(choices(&film, 11), None, "…for this file only");
        plx_base::paths::redirect_persistent_state_root_for_test(None);
    }
}
