//! **How a local title is described in one line**, shared by every page that shows one (Films,
//! the title page, Home): its facts, what a file with no description is, and its watch mark. Not a
//! screen: the screens may not name each other, so what they say alike lives here.

use plx_data::local::Entry;
use plx_data::local_progress::Progress;
use plx_ui::widgets::PosterMark;

/// How many genres the facts line names.
const GENRES_SHOWN: usize = 3;

/// `MKV · 1.4 GB`: what a video with no description is.
pub fn file_facts(entry: &Entry) -> String {
    let ext = entry.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
    let size = plx_ui::fmt::bytes(i64::try_from(entry.size).unwrap_or(i64::MAX), (1, 0, 0));
    if ext.is_empty() { size } else { format!("{ext} · {size}") }
}

/// `2019 · 8 min · Animation, Fantasy`: the facts a description gives, each only when known.
pub fn facts(year: Option<u16>, runtime_min: Option<u32>, genres: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y.to_string());
    }
    if let Some(m) = runtime_min {
        parts.push(plx_ui::fmt::dur_long(i64::from(m) * 60_000));
    }
    if !genres.is_empty() {
        parts.push(genres.iter().take(GENRES_SHOWN).cloned().collect::<Vec<_>>().join(", "));
    }
    parts.join(" · ")
}

/// The card's watch state, as the item menu reads it: finished wins over a kept place (a finished
/// film being watched again is still finished until the viewer says otherwise).
pub fn mark_of(record: Option<Progress>) -> PosterMark {
    match record {
        Some(m) if m.watched => PosterMark::Watched,
        Some(m) if m.offset_ms > 0 => PosterMark::InProgress,
        _ => PosterMark::None,
    }
}
