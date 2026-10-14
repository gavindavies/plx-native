//! The item that is PLAYING: its audio and subtitle tracks, chapters, skip markers and the facts
//! the player declares it with, plus the language matching the track menus group by.
//!
//! One owner holds it (`stores::metadata::MetadataStore`), installed when a playback starts and
//! retired when it ends, so every in-player panel reads the played file's own description.
pub mod languages;
pub mod sub_layout;
pub mod track_label;
pub mod track_names;

/// A borrowed read handle onto one [`MetadataState`], produced only from its owning
/// `stores::metadata::MetadataStore` (`MetadataStore::view`).
#[derive(Clone, Copy)]
pub struct MetadataView<'a> {
    state: &'a MetadataState,
}

impl<'a> MetadataView<'a> {
    pub fn new(owner: &'a crate::stores::metadata::MetadataStore) -> Self {
        Self { state: owner.state() }
    }
    pub fn playing(&self) -> Option<&'a PlayingItem> {
        self.state.playing.as_ref()
    }
    pub fn playing_chapters(&self) -> &'a [Chapter] {
        self.playing().map(|p| p.chapters.as_slice()).unwrap_or(&[])
    }
}


/// One Metadata owner's main-thread state.
#[derive(Default)]
pub struct MetadataState {
    playing: Option<PlayingItem>,
}


/// Plex's resume rule, in ONE place (home Continue-Watching, the detail Play button, and the
/// pitv-play harness all apply it): resume only past 10s and before 95% watched, else start
/// from the beginning. Both args are MILLISECONDS; the returned position is NANOSECONDS
/// (what `player::begin_resume` takes).
pub fn resume_ns(resume_ms: i64, dur_ms: i64) -> i64 {
    if resume_ms > 10_000 && (dur_ms <= 0 || (resume_ms as f64) < 0.95 * dur_ms as f64) {
        resume_ms * 1_000_000
    } else {
        0
    }
}

/// Friendly display name for an audio/subtitle codec id — the ONE codec→name map (the track
/// menu's section accessory and the Info card's track line both read it, so the same track
/// can't be named two ways).
pub fn friendly_codec(codec: &str) -> String {
    match codec.to_lowercase().as_str() {
        "truehd" => "Dolby TrueHD".to_string(),
        "eac3" | "ec-3" => "Dolby Digital Plus".to_string(),
        "ac3" => "Dolby Digital".to_string(),
        "dts" | "dca" => "DTS".to_string(),
        "aac" => "AAC".to_string(),
        "flac" => "FLAC".to_string(),
        "opus" => "Opus".to_string(),
        "mp3" => "MP3".to_string(),
        other if other.is_empty() => String::new(),
        other => other.to_uppercase(),
    }
}

/// One audio or subtitle track of the playing file.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Stream {
    /// The track's id within its playback (0 is "Off"; see `localplay::playing_item`).
    pub id: i64,
    /// Container order: the ordinal mapping sorts by it.
    pub index: i64,
    /// Display name ("English").
    pub lang: String,
    /// ISO code ("eng"), which the language preference matches.
    pub lang_code: String,
    /// The BCP-47 tag ("es-419", "en-GB"), region and all, where `lang_code` is only the primary
    /// subtag. `track_label`'s region fallback is the one reader.
    pub language_tag: String,
    pub codec: String,
    pub channels: i64,
    pub title: String,
    pub sdh: bool,
    pub forced: bool,
    /// The file's default track.
    pub default: bool,
    /// A subtitle file beside the video rather than a track inside it. The demuxer cannot reach
    /// it; `player::sidecar` reads and draws a TEXT one ([`Stream::sidecar_renderable`]).
    pub external: bool,
    /// A sidecar's path. Empty for every embedded track.
    pub key: String,
}

impl Stream {
    /// Can the player draw this sidecar? Only a TEXT format: `player::sidecar` keeps ASS/SSA
    /// scripts for styled rendering and reads SubRip, WebVTT and SAMI as cues. An image sidecar
    /// (`.idx`/`.sub` VobSub, `.sup` PGS) has no such path. An allow-list rather than "not an
    /// image codec", so a format nobody here has met is hidden instead of offered and then
    /// silently blank.
    pub fn sidecar_renderable(&self) -> bool {
        self.external
            && !self.key.is_empty()
            && matches!(
                self.codec.to_ascii_lowercase().as_str(),
                "srt" | "subrip" | "ass" | "ssa" | "vtt" | "webvtt" | "smi" | "sami"
            )
    }
}

/// One chapter of the playing file.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    /// 1-based chapter number.
    pub index: i64,
    /// Where it starts: the seek target and the timestamp label.
    pub start_ms: i64,
    /// Its name; empty shows "Chapter {index}".
    pub title: String,
}

// ---- the PLAYING-item store — the in-player source of truth ---------------------------------

/// The file that is playing, as its own tracks describe it: what the track menus list, the
/// Chapters tab reads and the Info card names. Installed when a playback starts, retired when the
/// next one does, so a panel never shows the previous file's tracks.
#[derive(Clone)]
pub struct PlayingItem {
    /// The file's path, which is also the key its saved choices are kept under.
    pub rk: String,
    pub audio: Vec<Stream>,
    pub subs: Vec<Stream>,
    /// The video frame rate (0 = unknown).
    pub video_fps: f64,
    /// The stored frame size (0 = unknown).
    pub width: i64,
    pub height: i64,
    /// Chapter boundaries — the in-player Chapters tab and strip.
    pub chapters: Vec<Chapter>,
}

#[cfg(any(test, feature = "test-support"))]
impl PlayingItem {
    /// A playing item carrying only `subs` — the fixture the track-menu and overlay tests
    /// install, where nothing but the subtitle list is read.
    pub fn with_subs(subs: Vec<Stream>) -> Self {
        PlayingItem {
            rk: "rk".into(),
            audio: Vec::new(),
            subs,
            video_fps: 0.0,
            width: 0,
            height: 0,
            chapters: Vec::new(),
        }
    }
}


/// Retire the playing item: a new playback is starting, and until its own item is installed no
/// panel may list the previous file's tracks or seek to its chapter offsets.
fn retire_playing_item(state: &mut MetadataState) {
    state.playing = None;
}

/// Install the item a playback starts with.
fn install_playing(state: &mut MetadataState, pt: Option<PlayingItem>) {
    if let Some(pt) = &pt {
        plx_base::eventlog::log(&format!(
            "playing item: {} audio={} subs={} chapters={}",
            pt.rk,
            pt.audio.len(),
            pt.subs.len(),
            pt.chapters.len()
        ));
    }
    state.playing = pt;
}

// ---- list-position → demuxer-ordinal conversion --------------------------------------------
// The demuxer selects "the Nth stream of its type" in CONTAINER order; the menu/metadata lists
// are in PMS document order. These convert a list position to that container ordinal by sorting
// on PMS `Stream.index` (stable tie-break on list position, so an index-less response degrades
// to document order — the previous behavior).

/// Container-audio ordinal of `audio[i]` — what `player::set_audio_track`/`request_audio_track`
/// (→ ff's nth_audio_stream) consume.
pub fn audio_ordinal(audio: &[Stream], i: usize) -> i32 {
    if i >= audio.len() {
        return i as i32;
    }
    let me = (audio[i].index, i);
    audio
        .iter()
        .enumerate()
        .filter(|(j, s)| (s.index, *j) < me)
        .count() as i32
}

/// Container ordinal of `subs[i]` among the EMBEDDED subtitle streams (all ff.rs enumerates —
/// sidecars are not in the container), or -1 when `subs[i]` is itself external (nothing to
/// client-render on direct-play; only a server transcode can burn it).
pub fn sub_render_ordinal(subs: &[Stream], i: usize) -> i32 {
    let s0 = match subs.get(i) {
        Some(s) if !s.external => s,
        _ => return -1,
    };
    let me = (s0.index, i);
    subs.iter()
        .enumerate()
        .filter(|(j, s)| !s.external && (s.index, *j) < me)
        .count() as i32
}

// ---- language matching (moved from `route::plan`, §1) -------------------------------------
// `route::plan`'s audio/subtitle preference ladder and `metadata::sub_layout`'s "yours"/"Other
// languages" grouping both need to know whether two language tags name the same language, so
// this lives in the data layer both sides already name rather than in either one's own module.

/// ISO 639-1 / 639-2 bibliographic and terminology spellings from Debian iso-codes:
/// https://salsa.debian.org/iso-codes-team/iso-codes/-/raw/main/data/iso_639-2.json
/// (retrieved 2026-09-27). Plex preferences use 639-1; PMS commonly uses either 639-2 form.
const LANG_SPELLINGS: &[&[&str]] = &[
    &["aa", "aar"],
    &["ab", "abk"],
    &["af", "afr"],
    &["ak", "aka"],
    &["am", "amh"],
    &["ar", "ara"],
    &["an", "arg"],
    &["as", "asm"],
    &["av", "ava"],
    &["ae", "ave"],
    &["ay", "aym"],
    &["az", "aze"],
    &["ba", "bak"],
    &["bm", "bam"],
    &["be", "bel"],
    &["bn", "ben"],
    &["bi", "bis"],
    &["bo", "bod", "tib"],
    &["bs", "bos"],
    &["br", "bre"],
    &["bg", "bul"],
    &["ca", "cat"],
    &["cs", "ces", "cze"],
    &["ch", "cha"],
    &["ce", "che"],
    &["cu", "chu"],
    &["cv", "chv"],
    &["kw", "cor"],
    &["co", "cos"],
    &["cr", "cre"],
    &["cy", "cym", "wel"],
    &["da", "dan"],
    &["de", "deu", "ger"],
    &["dv", "div"],
    &["dz", "dzo"],
    &["el", "ell", "gre"],
    &["en", "eng"],
    &["eo", "epo"],
    &["et", "est"],
    &["eu", "eus", "baq"],
    &["ee", "ewe"],
    &["fo", "fao"],
    &["fa", "fas", "per"],
    &["fj", "fij"],
    &["fi", "fin"],
    &["fr", "fra", "fre"],
    &["fy", "fry"],
    &["ff", "ful"],
    &["gd", "gla"],
    &["ga", "gle"],
    &["gl", "glg"],
    &["gv", "glv"],
    &["gn", "grn"],
    &["gu", "guj"],
    &["ht", "hat"],
    &["ha", "hau"],
    &["he", "heb"],
    &["hz", "her"],
    &["hi", "hin"],
    &["ho", "hmo"],
    &["hr", "hrv", "scr"],
    &["hu", "hun"],
    &["hy", "hye", "arm"],
    &["ig", "ibo"],
    &["io", "ido"],
    &["ii", "iii"],
    &["iu", "iku"],
    &["ie", "ile"],
    &["ia", "ina"],
    &["id", "ind"],
    &["ik", "ipk"],
    &["is", "isl", "ice"],
    &["it", "ita"],
    &["jv", "jav"],
    &["ja", "jpn"],
    &["kl", "kal"],
    &["kn", "kan"],
    &["ks", "kas"],
    &["ka", "kat", "geo"],
    &["kr", "kau"],
    &["kk", "kaz"],
    &["km", "khm"],
    &["ki", "kik"],
    &["rw", "kin"],
    &["ky", "kir"],
    &["kv", "kom"],
    &["kg", "kon"],
    &["ko", "kor"],
    &["kj", "kua"],
    &["ku", "kur"],
    &["lo", "lao"],
    &["la", "lat"],
    &["lv", "lav"],
    &["li", "lim"],
    &["ln", "lin"],
    &["lt", "lit"],
    &["lb", "ltz"],
    &["lu", "lub"],
    &["lg", "lug"],
    &["mh", "mah"],
    &["ml", "mal"],
    &["mr", "mar"],
    &["mk", "mkd", "mac"],
    &["mg", "mlg"],
    &["mt", "mlt"],
    &["mn", "mon"],
    &["mi", "mri", "mao"],
    &["ms", "msa", "may"],
    &["my", "mya", "bur"],
    &["na", "nau"],
    &["nv", "nav"],
    &["nr", "nbl"],
    &["nd", "nde"],
    &["ng", "ndo"],
    &["ne", "nep"],
    &["nl", "nld", "dut"],
    &["nn", "nno"],
    &["nb", "nob"],
    &["no", "nor", "nb", "nob"],
    &["ny", "nya"],
    &["oc", "oci"],
    &["oj", "oji"],
    &["or", "ori"],
    &["om", "orm"],
    &["os", "oss"],
    &["pa", "pan"],
    &["pi", "pli"],
    &["pl", "pol"],
    &["pt", "por"],
    &["ps", "pus"],
    &["qu", "que"],
    &["rm", "roh"],
    &["ro", "ron", "rum"],
    &["rn", "run"],
    &["ru", "rus"],
    &["sg", "sag"],
    &["sa", "san"],
    &["si", "sin"],
    &["sk", "slk", "slo"],
    &["sl", "slv"],
    &["se", "sme"],
    &["sm", "smo"],
    &["sn", "sna"],
    &["sd", "snd"],
    &["so", "som"],
    &["st", "sot"],
    &["es", "spa"],
    &["sq", "sqi", "alb"],
    &["sc", "srd"],
    &["sr", "srp"],
    &["ss", "ssw"],
    &["su", "sun"],
    &["sw", "swa"],
    &["sv", "swe"],
    &["ty", "tah"],
    &["ta", "tam"],
    &["tt", "tat"],
    &["te", "tel"],
    &["tg", "tgk"],
    &["tl", "tgl"],
    &["th", "tha"],
    &["ti", "tir"],
    &["to", "ton"],
    &["tn", "tsn"],
    &["ts", "tso"],
    &["tk", "tuk"],
    &["tr", "tur"],
    &["tw", "twi"],
    &["ug", "uig"],
    &["uk", "ukr"],
    &["ur", "urd"],
    &["uz", "uzb"],
    &["ve", "ven"],
    &["vi", "vie"],
    &["vo", "vol"],
    &["wa", "wln"],
    &["wo", "wol"],
    &["xh", "xho"],
    &["yi", "yid"],
    &["yo", "yor"],
    &["za", "zha"],
    &["zh", "zho", "chi"],
    &["zu", "zul"],
];

/// The ISO 639-1 (TWO-letter) spelling of a tag's language, read from the same rows
/// [`lang_key`] folds by, or `None` when the language has no two-letter code.
///
/// **Not [`lang_key`].** That key is the LEXICOGRAPHICALLY smallest spelling of a row — Dutch's
/// `["nl", "nld", "dut"]` keys to `"dut"` — chosen so grouping is stable, never as a wire format.
/// The subtitle search needs this one: PMS answers a 3-letter `language` with an HTTP 500
/// (`docs/pms-api.md` §8), so the code it is sent must be the two-letter spelling.
pub fn two_letter_code(tag: &str) -> Option<String> {
    let primary = tag.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    if primary.len() == 2 && primary.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Some(primary);
    }
    LANG_SPELLINGS
        .iter()
        .filter(|spellings| spellings.contains(&primary.as_str()))
        .flat_map(|spellings| spellings.iter().copied())
        .find(|spelling| spelling.len() == 2)
        .map(str::to_string)
}

/// **The one canonical spelling of a language tag's LANGUAGE**, the key [`lang_matches`] compares
/// by: the primary subtag, lower-cased, folded onto the first spelling of its [`LANG_SPELLINGS`]
/// row(s) when it has one (`"fr-CA"`, `"fre"` and `"fra"` all answer `"fr"`), or itself when it
/// has none. `None` for an empty tag, which names no language and so matches nothing. A caller that
/// groups many tracks computes this ONCE per track and buckets by it, rather than asking
/// `lang_matches` pairwise.
pub fn lang_key(tag: &str) -> Option<std::borrow::Cow<'static, str>> {
    let primary = tag.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    if primary.is_empty() {
        return None;
    }
    // The smallest spelling over EVERY row that lists it, not the first row's head: Norwegian is
    // listed both as `nb`/`nob` and as `no`/`nor`/`nb`/`nob`, and all four must share one key.
    let canonical = LANG_SPELLINGS
        .iter()
        .filter(|spellings| spellings.contains(&primary.as_str()))
        .flat_map(|spellings| spellings.iter().copied())
        .min();
    Some(match canonical {
        Some(spelling) => std::borrow::Cow::Borrowed(spelling),
        None => std::borrow::Cow::Owned(primary),
    })
}

/// Do two language tags name the same language? Either side may be a Plex preference
/// (`"hu-HU"`, `"pt-BR"`) or a stream's ISO-639-2 `languageCode` (`"hun"`, `"ger"`/`"deu"`), so the
/// same test serves a preference against a stream AND a picked stream against its siblings (a
/// `fre` pick and a `fra` sibling are one language). Only the primary subtag counts — a stream
/// says "Portuguese", never "Brazilian". An empty tag matches nothing. Same [`lang_key`] = same
/// language.
pub fn lang_matches(a: &str, b: &str) -> bool {
    matches!((lang_key(a), lang_key(b)), (Some(a), Some(b)) if a == b)
}

#[cfg(test)]
mod lang_matches_tests {
    use super::lang_matches;

    #[test]
    fn a_regional_bcp47_tag_matches_the_iso_639_2_bibliographic_and_terminology_forms() {
        assert!(lang_matches("fr-CA", "fra"));
        assert!(lang_matches("fr-CA", "fre"));
        assert!(!lang_matches("fr-CA", "eng"));
    }

    /// **`lang_key` agrees with the pairwise row rule it replaced** — two spellings share a key
    /// exactly when some [`LANG_SPELLINGS`](super::LANG_SPELLINGS) row lists both (Norwegian's
    /// overlapping rows included).
    #[test]
    fn lang_key_agrees_with_the_pairwise_row_rule() {
        let rows = super::LANG_SPELLINGS;
        // every pair a row lists is one language…
        for row in rows {
            for a in row.iter() {
                for b in row.iter() {
                    assert!(lang_matches(a, b), "{a} vs {b}");
                }
            }
        }
        // …and two rows that share no spelling are two
        for r1 in rows {
            for r2 in rows {
                if !r1.iter().any(|a| r2.contains(a)) {
                    assert!(!lang_matches(r1[0], r2[0]), "{} vs {}", r1[0], r2[0]);
                }
            }
        }
        assert_eq!(super::lang_key("fre").as_deref(), Some("fr"));
        assert_eq!(super::lang_key("xx-YY").as_deref(), Some("xx"));
        assert_eq!(super::lang_key("  "), None);
    }
}

/// Apply one [`MetadataCmd`](crate::stores::metadata::MetadataCmd) to `state`; whether it changed
/// anything.
pub fn run(state: &mut MetadataState, cmd: crate::stores::metadata::MetadataCmd) -> bool {
    use crate::stores::metadata::MetadataCmd;
    match cmd {
        MetadataCmd::InstallPlaying(item) => install_playing(state, item),
        MetadataCmd::RetirePlayingItem => retire_playing_item(state),
    }
    true
}
