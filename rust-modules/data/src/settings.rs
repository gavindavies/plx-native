//! **The install-wide player settings**: the subtitle tone, size and placement, how far one
//! Left/Right press jumps, the languages a file with no saved choice starts on, and the player's
//! read-ahead cache (how it reads ahead of the playhead, how much, how fast). A legible caption answers a fact about the screen and the distance to
//! it, not about one film, so these are one value each for the whole install; what the viewer
//! chooses per FILE (the audio track, the subtitle and its timing) is `local_progress`'s.
//!
//! Each setting is held in memory in one atomic the player reads every frame (the two default
//! languages in one mutex), read from one JSON
//! file under the persistent state root (`settings.json`) once at boot ([`load`], blocking) and
//! rewritten whole on a worker after every change ([`set_subtitle_tone`] and its siblings): the
//! new value is live at once and the write follows it, so a pick is never held up by the disk.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
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

/// **Which files the player reads ahead of the playhead** (Kodi's "buffer mode"): a file on a
/// network share is the one a bitrate spike can stall, so that is the default; a local disk keeps
/// up by itself.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadAhead {
    #[serde(rename = "off")]
    Off,
    #[default]
    #[serde(rename = "network")]
    NetworkShares,
    #[serde(rename = "all")]
    AllFiles,
}

impl ReadAhead {
    pub const LADDER: [ReadAhead; 3] = [ReadAhead::Off, ReadAhead::NetworkShares, ReadAhead::AllFiles];
    pub fn from_index(i: u8) -> ReadAhead {
        Self::LADDER.get(i as usize).copied().unwrap_or_default()
    }
    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&o| o == self).unwrap_or(1) as u8
    }
}

/// **How much the read-ahead holds** (Kodi's "memory size"). The buffer is filled up to this much
/// past the playhead and released behind it, so it is the most memory one playback spends on it.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheSize {
    #[serde(rename = "32mb")]
    Mb32,
    #[serde(rename = "64mb")]
    Mb64,
    #[default]
    #[serde(rename = "128mb")]
    Mb128,
    #[serde(rename = "256mb")]
    Mb256,
    #[serde(rename = "512mb")]
    Mb512,
}

impl CacheSize {
    pub const LADDER: [CacheSize; 5] = [CacheSize::Mb32, CacheSize::Mb64, CacheSize::Mb128, CacheSize::Mb256, CacheSize::Mb512];
    pub fn from_index(i: u8) -> CacheSize {
        Self::LADDER.get(i as usize).copied().unwrap_or_default()
    }
    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&o| o == self).unwrap_or(2) as u8
    }
    pub fn megabytes(self) -> u64 {
        match self {
            CacheSize::Mb32 => 32,
            CacheSize::Mb64 => 64,
            CacheSize::Mb128 => 128,
            CacheSize::Mb256 => 256,
            CacheSize::Mb512 => 512,
        }
    }
    pub fn bytes(self) -> u64 {
        self.megabytes() * 1024 * 1024
    }
}

/// **How fast the read-ahead fills** (Kodi's "read factor"): a multiple of the file's average
/// bitrate. Higher fills the buffer sooner after a seek; a cap keeps a share's link free for
/// whatever else uses it. `Unlimited` reads as fast as the source answers.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadFactor {
    #[serde(rename = "2x")]
    X2,
    #[default]
    #[serde(rename = "4x")]
    X4,
    #[serde(rename = "8x")]
    X8,
    #[serde(rename = "20x")]
    X20,
    #[serde(rename = "unlimited")]
    Unlimited,
}

impl ReadFactor {
    pub const LADDER: [ReadFactor; 5] = [ReadFactor::X2, ReadFactor::X4, ReadFactor::X8, ReadFactor::X20, ReadFactor::Unlimited];
    pub fn from_index(i: u8) -> ReadFactor {
        Self::LADDER.get(i as usize).copied().unwrap_or_default()
    }
    pub fn index(self) -> u8 {
        Self::LADDER.iter().position(|&o| o == self).unwrap_or(1) as u8
    }
    /// The multiple of the bitrate, `None` for unlimited.
    pub fn multiple(self) -> Option<u32> {
        match self {
            ReadFactor::X2 => Some(2),
            ReadFactor::X4 => Some(4),
            ReadFactor::X8 => Some(8),
            ReadFactor::X20 => Some(20),
            ReadFactor::Unlimited => None,
        }
    }
}

/// The languages the default-language pickers offer, as ISO 639-1 codes, most spoken first.
/// A track matches one through `metadata::lang_matches`, whatever spelling its file uses.
pub const LANGUAGES: [&str; 24] = [
    "en", "es", "fr", "de", "it", "pt", "nl", "sv", "no", "da", "fi", "pl",
    "cs", "hu", "ru", "uk", "be", "el", "tr", "ar", "he", "hi", "ja", "zh",
];

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
    /// The subtitle a file with no saved choice starts on: an ISO 639-1 code, empty for Off.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    subtitle_language: String,
    /// The audio a file with no saved choice starts on: an ISO 639-1 code, empty for the file's own
    /// first track.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    audio_language: String,
    #[serde(default)]
    read_ahead: ReadAhead,
    #[serde(default)]
    cache_size: CacheSize,
    #[serde(default)]
    read_factor: ReadFactor,
}

static SUBTITLE_TONE: AtomicU8 = AtomicU8::new(0);
static SUBTITLE_SIZE: AtomicU8 = AtomicU8::new(1);
static SUBTITLE_POSITION: AtomicU8 = AtomicU8::new(0);
static SKIP_INTERVAL: AtomicU8 = AtomicU8::new(1);
static READ_AHEAD: AtomicU8 = AtomicU8::new(1);
static CACHE_SIZE: AtomicU8 = AtomicU8::new(2);
static READ_FACTOR: AtomicU8 = AtomicU8::new(1);
/// The two default languages: `(subtitle, audio)`.
static LANGUAGES_CHOSEN: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
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
    READ_AHEAD.store(disk.read_ahead.index(), Relaxed);
    CACHE_SIZE.store(disk.cache_size.index(), Relaxed);
    READ_FACTOR.store(disk.read_factor.index(), Relaxed);
    *languages() = (disk.subtitle_language.clone(), disk.audio_language.clone());
    REVISION.fetch_add(1, Relaxed);
}

fn snapshot() -> Disk {
    Disk {
        subtitle_tone: subtitle_tone(),
        subtitle_size: subtitle_size(),
        subtitle_position: subtitle_position(),
        skip_interval: skip_interval(),
        subtitle_language: subtitle_language(),
        audio_language: audio_language(),
        read_ahead: read_ahead(),
        cache_size: cache_size(),
        read_factor: read_factor(),
    }
}

fn languages() -> std::sync::MutexGuard<'static, (String, String)> {
    LANGUAGES_CHOSEN.lock().unwrap_or_else(|e| e.into_inner())
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

pub fn read_ahead() -> ReadAhead {
    ReadAhead::from_index(READ_AHEAD.load(Relaxed))
}
pub fn cache_size() -> CacheSize {
    CacheSize::from_index(CACHE_SIZE.load(Relaxed))
}
pub fn read_factor() -> ReadFactor {
    ReadFactor::from_index(READ_FACTOR.load(Relaxed))
}
/// The default subtitle language (ISO 639-1), empty for Off.
pub fn subtitle_language() -> String {
    languages().0.clone()
}
/// The default audio language (ISO 639-1), empty for the file's own first track.
pub fn audio_language() -> String {
    languages().1.clone()
}

pub fn set_read_ahead(v: ReadAhead) {
    change(&READ_AHEAD, v.index());
}
pub fn set_cache_size(v: CacheSize) {
    change(&CACHE_SIZE, v.index());
}
pub fn set_read_factor(v: ReadFactor) {
    change(&READ_FACTOR, v.index());
}
pub fn set_subtitle_language(code: &str) {
    set_language(|l| &mut l.0, code);
}
pub fn set_audio_language(code: &str) {
    set_language(|l| &mut l.1, code);
}

fn set_language(slot: impl FnOnce(&mut (String, String)) -> &mut String, code: &str) {
    {
        let mut held = languages();
        let cell = slot(&mut held);
        if cell == code {
            return;
        }
        *cell = code.to_owned();
    }
    REVISION.fetch_add(1, Relaxed);
    plx_machine::idle::invalidate();
    save();
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
        assert!(ReadAhead::LADDER.iter().all(|&t| ReadAhead::from_index(t.index()) == t));
        assert!(CacheSize::LADDER.iter().all(|&t| CacheSize::from_index(t.index()) == t));
        assert!(ReadFactor::LADDER.iter().all(|&t| ReadFactor::from_index(t.index()) == t));
        assert_eq!(SubtitleSize::from_index(200), SubtitleSize::Medium, "out of range is the default");
        assert_eq!(CacheSize::from_index(200), CacheSize::Mb128);
    }

    #[test]
    fn a_saved_file_is_read_back_and_a_missing_key_is_its_default() {
        let disk: Disk = serde_json::from_str(r#"{"subtitle_size":"large","skip_interval":"30s"}"#).unwrap();
        assert_eq!(disk.subtitle_size, SubtitleSize::Large);
        assert_eq!(disk.skip_interval, SkipInterval::Seconds30);
        assert_eq!(disk.subtitle_tone, SubtitleTone::White);
        assert_eq!(disk.subtitle_position, SubtitlePosition::Low);
        assert_eq!((disk.read_ahead, disk.cache_size, disk.read_factor), (ReadAhead::NetworkShares, CacheSize::Mb128, ReadFactor::X4));
        assert!(disk.subtitle_language.is_empty() && disk.audio_language.is_empty());
    }

    #[test]
    fn the_cache_and_language_settings_are_written_by_name() {
        let disk: Disk = serde_json::from_str(r#"{"read_ahead":"all","cache_size":"512mb","read_factor":"unlimited","subtitle_language":"en"}"#).unwrap();
        assert_eq!((disk.read_ahead, disk.cache_size, disk.read_factor), (ReadAhead::AllFiles, CacheSize::Mb512, ReadFactor::Unlimited));
        assert_eq!(disk.subtitle_language, "en");
        assert_eq!(CacheSize::Mb512.bytes(), 512 * 1024 * 1024);
        assert_eq!(ReadFactor::Unlimited.multiple(), None);
        let json = serde_json::to_string(&disk).unwrap();
        assert!(json.contains(r#""cache_size":"512mb""#) && !json.contains("audio_language"), "{json}");
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
