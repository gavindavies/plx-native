//! **The install-wide player settings**: the subtitle tone, size and placement, and how far one
//! Left/Right press jumps. A legible caption answers a fact about the screen and the distance to
//! it, not about one film, so these are one value each for the whole install; what the viewer
//! chooses per FILE (the audio track, the subtitle and its timing) is `local_progress`'s.
//!
//! Each setting is held in memory in one atomic the player reads every frame, read from one JSON
//! file under the persistent state root (`settings.json`) once at boot ([`load`], blocking) and
//! rewritten whole on a worker after every change ([`set_subtitle_tone`] and its siblings): the
//! new value is live at once and the write follows it, so a pick is never held up by the disk.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

const FILE_NAME: &str = "settings.json";

/// The persisted subtitle tones, lightest first. The spelling on disk is
/// explicit: these strings are a file-format contract and must survive a variant rename.
///
/// A ladder of GREYS rather than a colour wheel, because the job is brightness: over an HDR
/// picture the panel maps graphics white far above where it sits in SDR, and the only thing that
/// makes a caption comfortable there is less light. What each rung looks like is `ui::theme`'s
/// (`subtitle_ink`); this type only names them.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitleTone {
    #[default]
    #[serde(rename = "white")]
    White,
    #[serde(rename = "grey_85")]
    Silver,
    #[serde(rename = "grey_70")]
    LightGrey,
    #[serde(rename = "grey_55")]
    Grey,
    #[serde(rename = "grey_40")]
    DarkGrey,
    #[serde(rename = "grey_28")]
    Charcoal,
}

impl SubtitleTone {
    /// Every rung, lightest first — the order the picker draws them in.
    pub const LADDER: [SubtitleTone; 6] = [
        SubtitleTone::White,
        SubtitleTone::Silver,
        SubtitleTone::LightGrey,
        SubtitleTone::Grey,
        SubtitleTone::DarkGrey,
        SubtitleTone::Charcoal,
    ];

    /// Canonical English tone label for diagnostics and compatibility callers.
    /// Localized picker rows map this typed tone through the UI catalog.
    pub fn label(self) -> &'static str {
        match self {
            SubtitleTone::White => "White",
            SubtitleTone::Silver => "Silver",
            SubtitleTone::LightGrey => "Light gray",
            SubtitleTone::Grey => "Gray",
            SubtitleTone::DarkGrey => "Dark gray",
            SubtitleTone::Charcoal => "Charcoal",
        }
    }

    /// An in-memory index back to a rung — out of range is `White`, never a neighbouring rung,
    /// for the reason `Quality::from_index` gives: the ladder can grow or shrink.
    pub fn from_index(i: u8) -> SubtitleTone {
        Self::LADDER.get(i as usize).copied().unwrap_or(SubtitleTone::White)
    }

    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&t| t == self).unwrap_or(0) as u8
    }
}

/// The persisted subtitle text sizes, smallest first — install-wide like [`SubtitleTone`]: a
/// legible caption size answers a fact about the PANEL and the couch distance from it, not about
/// whoever is watching. Absence is `Medium`, which is the caption face every build before this
/// preference drew (`appkit::player_hud`'s hardcoded `sz = 36`). Spelling on disk is explicit, like
/// every other persisted ladder here.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitleSize {
    #[serde(rename = "small")]
    Small,
    #[default]
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "large")]
    Large,
    #[serde(rename = "extra_large")]
    ExtraLarge,
}

impl SubtitleSize {
    /// Every rung, smallest first — the order the picker lists them in.
    pub const LADDER: [SubtitleSize; 4] = [
        SubtitleSize::Small,
        SubtitleSize::Medium,
        SubtitleSize::Large,
        SubtitleSize::ExtraLarge,
    ];

    /// An in-memory index back to a rung — out of range is `Medium`, never a neighbouring rung,
    /// for the reason [`SubtitleTone::from_index`] gives: the ladder can grow or shrink.
    pub fn from_index(i: u8) -> SubtitleSize {
        Self::LADDER.get(i as usize).copied().unwrap_or(SubtitleSize::Medium)
    }

    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&s| s == self).unwrap_or(1) as u8
    }
}

/// The persisted subtitle vertical placements, lowest first — install-wide like [`SubtitleTone`].
/// Absence is `Low`, which is where every build before this preference drew the caption
/// (`appkit::player_hud`'s fixed `SUB_BASE_Y`/`SUB_CEIL_Y` baseline). Only the plain-text caption draw
/// moves with this; image (PGS/VobSub) captions and native ASS/SSA keep their own placement
/// (`docs/ass-subtitles.md`).
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitlePosition {
    #[default]
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "middle")]
    Middle,
    #[serde(rename = "high")]
    High,
}

impl SubtitlePosition {
    /// Every rung, lowest first — the order the picker lists them in.
    pub const LADDER: [SubtitlePosition; 3] = [
        SubtitlePosition::Low,
        SubtitlePosition::Middle,
        SubtitlePosition::High,
    ];

    /// An in-memory index back to a rung — out of range is `Low`, never a neighbouring rung, for
    /// the reason [`SubtitleTone::from_index`] gives: the ladder can grow or shrink.
    pub fn from_index(i: u8) -> SubtitlePosition {
        Self::LADDER.get(i as usize).copied().unwrap_or(SubtitlePosition::Low)
    }

    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&p| p == self).unwrap_or(0) as u8
    }
}

/// How far one Left/Right press jumps in the player (and the trailer transport) — install-wide
/// like [`SubtitleTone`], the same for both directions. Absence is `Seconds10`, the fixed hop
/// every build before this preference made. A closed set rather than a free number, so a hand-edited file cannot ask
/// for a zero-length or hour-long hop. Holding a key still ramps by its own curve; this is only the tap.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipInterval {
    #[serde(rename = "5s")]
    Seconds5,
    #[default]
    #[serde(rename = "10s")]
    Seconds10,
    #[serde(rename = "15s")]
    Seconds15,
    #[serde(rename = "30s")]
    Seconds30,
    #[serde(rename = "60s")]
    Seconds60,
}

impl SkipInterval {
    /// Every option, shortest first — the order the picker lists them in.
    pub const LADDER: [SkipInterval; 5] = [
        SkipInterval::Seconds5,
        SkipInterval::Seconds10,
        SkipInterval::Seconds15,
        SkipInterval::Seconds30,
        SkipInterval::Seconds60,
    ];

    /// An in-memory index back to an option — out of range is the default, never a neighbouring
    /// option, for the reason [`SubtitleTone::from_index`] gives: the ladder can grow or shrink.
    pub fn from_index(i: u8) -> SkipInterval {
        Self::LADDER.get(i as usize).copied().unwrap_or_default()
    }

    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&o| o == self).unwrap_or(1) as u8
    }

    pub fn seconds(self) -> i64 {
        match self {
            SkipInterval::Seconds5 => 5,
            SkipInterval::Seconds10 => 10,
            SkipInterval::Seconds15 => 15,
            SkipInterval::Seconds30 => 30,
            SkipInterval::Seconds60 => 60,
        }
    }

    /// The hop in nanoseconds, the unit every scrub position is in.
    pub fn ns(self) -> i64 {
        self.seconds() * 1_000_000_000
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    subtitle_tone: SubtitleTone,
    #[serde(default)]
    subtitle_size: SubtitleSize,
    #[serde(default)]
    subtitle_position: SubtitlePosition,
    #[serde(default)]
    skip_interval: SkipInterval,
}

static SUBTITLE_TONE: AtomicU8 = AtomicU8::new(0);
static SUBTITLE_SIZE: AtomicU8 = AtomicU8::new(1);
static SUBTITLE_POSITION: AtomicU8 = AtomicU8::new(0);
static SKIP_INTERVAL: AtomicU8 = AtomicU8::new(1);
static REVISION: AtomicU64 = AtomicU64::new(0);
/// Serializes the file writes, so an older snapshot can never land after a newer one.
static WRITER: Mutex<()> = Mutex::new(());

fn file() -> PathBuf {
    plx_base::paths::persistent_state_root().join(FILE_NAME)
}

fn publish(disk: &Disk) {
    SUBTITLE_TONE.store(disk.subtitle_tone.index(), Relaxed);
    SUBTITLE_SIZE.store(disk.subtitle_size.index(), Relaxed);
    SUBTITLE_POSITION.store(disk.subtitle_position.index(), Relaxed);
    SKIP_INTERVAL.store(disk.skip_interval.index(), Relaxed);
    REVISION.fetch_add(1, Relaxed);
}

fn snapshot() -> Disk {
    Disk {
        subtitle_tone: subtitle_tone(),
        subtitle_size: subtitle_size(),
        subtitle_position: subtitle_position(),
        skip_interval: skip_interval(),
    }
}

/// Read the settings from disk. Blocking: call it at boot, before the frame loop. A missing or
/// unreadable file is every default, logged when it is not simply absent.
pub fn load() {
    let _block = plx_base::task::assert_may_block(const { &plx_base::task::BlockingLabel::new("settings load") });
    let disk = match std::fs::read(file()) {
        Ok(bytes) => serde_json::from_slice::<Disk>(&bytes).unwrap_or_else(|e| {
            plx_base::eventlog::log(&format!("settings: {FILE_NAME} unreadable ({e}); using the defaults"));
            Disk::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Disk::default(),
        Err(e) => {
            plx_base::eventlog::log(&format!("settings: {FILE_NAME} unreadable ({e}); using the defaults"));
            Disk::default()
        }
    };
    publish(&disk);
}

/// Moves whenever a setting changes.
pub fn revision() -> u64 {
    REVISION.load(Relaxed)
}

pub fn subtitle_tone() -> SubtitleTone {
    SubtitleTone::from_index(SUBTITLE_TONE.load(Relaxed))
}
pub fn subtitle_size() -> SubtitleSize {
    SubtitleSize::from_index(SUBTITLE_SIZE.load(Relaxed))
}
pub fn subtitle_position() -> SubtitlePosition {
    SubtitlePosition::from_index(SUBTITLE_POSITION.load(Relaxed))
}
pub fn skip_interval() -> SkipInterval {
    SkipInterval::from_index(SKIP_INTERVAL.load(Relaxed))
}

pub fn set_subtitle_tone(v: SubtitleTone) {
    change(&SUBTITLE_TONE, v.index());
}
pub fn set_subtitle_size(v: SubtitleSize) {
    change(&SUBTITLE_SIZE, v.index());
}
pub fn set_subtitle_position(v: SubtitlePosition) {
    change(&SUBTITLE_POSITION, v.index());
}
pub fn set_skip_interval(v: SkipInterval) {
    change(&SKIP_INTERVAL, v.index());
}

/// Publish one setting now, then write the whole file behind it.
fn change(cell: &AtomicU8, index: u8) {
    if cell.swap(index, Relaxed) == index {
        return;
    }
    REVISION.fetch_add(1, Relaxed);
    plx_machine::idle::invalidate();
    save();
}

/// Write the current settings on a worker: the latest values at the moment the write runs, under
/// one lock, through a temporary file renamed over the old one.
fn save() {
    let spawned = plx_base::task::spawn_small("settings save", || {
        let _writer = WRITER.lock().unwrap_or_else(|e| e.into_inner());
        let json = match serde_json::to_vec_pretty(&snapshot()) {
            Ok(j) => j,
            Err(e) => {
                plx_base::eventlog::log(&format!("settings: not saved ({e})"));
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
            plx_base::eventlog::log(&format!("settings: not saved to {} ({e})", path.display()));
        }
    });
    if !spawned {
        plx_base::eventlog::log("settings: could not start the save worker; not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rung_round_trips_through_its_index() {
        assert!(SubtitleTone::LADDER.iter().all(|&t| SubtitleTone::from_index(t.index()) == t));
        assert!(SubtitleSize::LADDER.iter().all(|&t| SubtitleSize::from_index(t.index()) == t));
        assert!(SubtitlePosition::LADDER.iter().all(|&t| SubtitlePosition::from_index(t.index()) == t));
        assert!(SkipInterval::LADDER.iter().all(|&t| SkipInterval::from_index(t.index()) == t));
        assert_eq!(SubtitleSize::from_index(200), SubtitleSize::Medium, "out of range is the default");
    }

    #[test]
    fn a_saved_file_is_read_back_and_a_missing_key_is_its_default() {
        let disk: Disk = serde_json::from_str(r#"{"subtitle_size":"large","skip_interval":"30s"}"#).unwrap();
        assert_eq!(disk.subtitle_size, SubtitleSize::Large);
        assert_eq!(disk.skip_interval, SkipInterval::Seconds30);
        assert_eq!(disk.subtitle_tone, SubtitleTone::White);
        assert_eq!(disk.subtitle_position, SubtitlePosition::Low);
    }

    #[test]
    fn a_change_is_live_at_once_and_reaches_the_file() {
        let _serial = plx_base::testlock::serial();
        let dir = plx_base::testscratch::process_dir("settings-test");
        plx_base::paths::redirect_persistent_state_root_for_test(Some(dir.join("state")));
        load();
        set_subtitle_size(SubtitleSize::ExtraLarge);
        assert_eq!(subtitle_size(), SubtitleSize::ExtraLarge);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let saved = loop {
            let read = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice::<Disk>(&b).ok());
            if let Some(d) = read.filter(|d| d.subtitle_size == SubtitleSize::ExtraLarge) {
                break Some(d);
            }
            if std::time::Instant::now() > deadline {
                break None;
            }
            std::thread::yield_now();
        };
        assert!(saved.is_some(), "the write follows the change");
        set_subtitle_size(SubtitleSize::Medium);
        plx_base::paths::redirect_persistent_state_root_for_test(None);
    }
}
