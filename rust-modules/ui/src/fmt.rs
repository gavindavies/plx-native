//! Tiny shared display formatters — ONE home for the duration/clock strings so the same value
//! can't render differently across screens (the "2 hr 15 min" vs "2h 15m" vs "0 hr 45 min"
//! drift this replaces).

/// **Seconds-scale duration** — "4.5 s" / "12 s". The two formatters above are minute-scale and
/// round a fifteen-second interval to "0m", which is a read-out saying nothing; this is for the
/// short spans the diagnostics panel reports (an Original shortfall's elapsed time, N13).
///
/// One decimal below ten seconds and none above, because the interesting distinctions are at the
/// bottom of the range — the difference between 2 s and 4.5 s of shortfall is a decision, and the
/// difference between 40 s and 41 s is not.
pub fn secs_short(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 10_000 {
        plx_platform::i18n::msg::core_seconds(&plx_platform::i18n::current().decimal(ms / 100, 1))
    } else {
        plx_platform::i18n::msg::core_seconds(&plx_platform::i18n::current().number(ms / 1_000))
    }
}

#[cfg(test)]
#[test]
fn seconds_scale_durations_keep_the_distinctions_that_matter() {
    assert_eq!(secs_short(0), "0.0 s");
    assert_eq!(secs_short(4_500), "4.5 s");
    assert_eq!(secs_short(9_999), "9.9 s");
    assert_eq!(secs_short(12_000), "12 s");
    // Negative is not a duration; it reads as none rather than as a minus sign on screen.
    assert_eq!(secs_short(-1), "0.0 s");
}

/// Spelled-out duration — "2 hr 15 min" / "45 min" (detail hero + About "Run Time").
pub fn dur_long(ms: i64) -> String {
    let mins = (ms / 60_000).max(0);
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 {
        plx_platform::i18n::msg::core_duration_long_hours(h, m)
    } else {
        plx_platform::i18n::msg::core_duration_long_minutes(m)
    }
}

/// Time-remaining for a Continue-Watching item — "8 min left" / "1 hr 2 min left" (rounded up to
/// the next whole minute, floored at 1). `remaining_ms` is duration − viewOffset.
pub fn time_left(remaining_ms: i64) -> String {
    let mins = ((remaining_ms + 59_999) / 60_000).max(1);
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 {
        plx_platform::i18n::msg::core_time_left_hours(h, m)
    } else {
        plx_platform::i18n::msg::core_time_left_minutes(m)
    }
}

/// Playback clock — "1:23:45" past the hour, else "3:45" (scrubber clocks, chapter stamps).
pub fn clock(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// A collection's size — `"1 item"`, `"12 items"`. ONE formatter for the collection page's meta
/// line and every collection tile's caption, so the two cannot count one collection two ways.
pub fn item_count(n: i64) -> String {
    plx_platform::i18n::msg::browse_search_items(n)
}

/// The media badge for a video version — `"4K"` / `"1080p"` / `"SD"`, or None when the item has no
/// video (a show container, a music item, an unparsed response).
///
/// `res` is PMS's `Media.videoResolution` and is PREFERRED over the frame size, because the frame
/// size is the STORED one: a 2.35:1 1080p film is 1918x802 on the dev server, and a height rule
/// would badge it 720p. Verified values are `"4k"`, `"1080"`, `"720"`, `"576"`, `"sd"` — and PMS
/// sends every one of them as a numeric-looking STRING, which is exactly the shape this maps.
/// `width`/`height` are only the fallback for a version that omits the class.
pub fn resolution(res: &str, width: i64, height: i64) -> Option<String> {
    let r = res.trim().to_ascii_lowercase();
    if !r.is_empty() {
        // "4k"/"8k" are already the badge; a bare number is a scan-line count ("1080" → "1080p");
        // anything else (e.g. "sd") is a class name and reads as itself, upper-cased.
        return Some(match r.as_str() {
            _ if r.chars().all(|c| c.is_ascii_digit()) => format!("{r}p"),
            _ => r.to_ascii_uppercase(),
        });
    }
    // No class: fall back to the frame WIDTH, which (unlike height) is constant across aspect
    // ratios. Thresholds sit below each nominal width (3840/2560/1920/1280/1024) so a cropped or
    // anamorphic frame still lands in its own class, and the buckets are the SAME vocabulary the
    // class path emits — the two paths must not disagree about the same file.
    // saturating: these are wire values via the lenient `de_i64`, so a garbage 19-digit height
    // must not overflow (a debug-build panic, a silently wrapped badge in release)
    let w = if width > 0 {
        width
    } else {
        height.saturating_mul(16) / 9
    };
    Some(match w {
        w if w <= 0 => return None,
        w if w >= 3000 => "4K".to_string(),
        w if w >= 2400 => "1440p".to_string(),
        w if w >= 1700 => "1080p".to_string(),
        w if w >= 1100 => "720p".to_string(),
        w if w >= 900 => "576p".to_string(),
        _ => "SD".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::resolution;

    /// The mapping is fed PMS's own values, and PMS string-encodes them — including the ones that
    /// look like integers ("1080"), which is the case that must come out as "1080p" and not as a
    /// number or an empty badge.
    #[test]
    fn resolution_labels_pms_string_encoded_classes() {
        // verified live on the dev server: every videoResolution is a JSON string
        assert_eq!(resolution("4k", 3840, 2160).as_deref(), Some("4K"));
        assert_eq!(resolution("1080", 1920, 1080).as_deref(), Some("1080p"));
        assert_eq!(resolution("720", 1280, 720).as_deref(), Some("720p"));
        assert_eq!(resolution("576", 1024, 576).as_deref(), Some("576p"));
        assert_eq!(resolution("sd", 826, 452).as_deref(), Some("SD"));
        // whitespace/casing from the wire must not produce a second spelling of a badge
        assert_eq!(resolution(" 4K ", 0, 0).as_deref(), Some("4K"));
        // the class WINS over the frame size: a 2.35:1 1080p film is 1918x802 (a height rule
        // would call it 720p, a width rule 1080p — either way the server's own answer decides)
        assert_eq!(resolution("1080", 1918, 802).as_deref(), Some("1080p"));
    }

    #[test]
    fn resolution_falls_back_to_frame_width() {
        assert_eq!(resolution("", 3840, 2160).as_deref(), Some("4K"));
        assert_eq!(resolution("", 1918, 802).as_deref(), Some("1080p")); // scope 1080p, by WIDTH
        assert_eq!(resolution("", 1280, 720).as_deref(), Some("720p"));
        assert_eq!(resolution("", 720, 480).as_deref(), Some("SD"));
        // the fallback speaks the class path's vocabulary: the same file must not badge "576p"
        // with a videoResolution and "SD" without one
        assert_eq!(
            resolution("", 1024, 576).as_deref(),
            resolution("576", 1024, 576).as_deref()
        );
        assert_eq!(resolution("", 0, 1080).as_deref(), Some("1080p")); // width absent → from height
        assert_eq!(resolution("", 0, 0), None); // no video at all (a show container) → no badge
    }
}

/// A whole percentage in the UI locale: the catalog pattern owns the sign's placement (Spanish and
/// Belarusian set it off with a no-break space; English does not).
pub fn percent(value: i64) -> String {
    plx_platform::i18n::msg::core_percent(&plx_platform::i18n::current().number(value))
}

/// `value` rounded to `scale` fractional digits, in the UI locale's numerals and separators.
pub fn decimal(value: f64, scale: i16) -> String {
    let factor = 10i64.pow(u32::try_from(scale).unwrap_or(0)) as f64;
    plx_platform::i18n::current().decimal((value * factor).round() as i64, scale)
}

/// A byte count in binary units with the UI locale's numerals — `2.50 GB`, `12.3 MB`, `640 kB`.
/// `scales` is the number of fractional digits for GB, MB and kB respectively; a count below a
/// kilobyte is whole bytes (`B`).
pub fn bytes(bytes: i64, scales: (i16, i16, i16)) -> String {
    use plx_platform::i18n::msg;
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        msg::core_unit_gb(&decimal(b / (K * K * K), scales.0))
    } else if b >= K * K {
        msg::core_unit_mb(&decimal(b / (K * K), scales.1))
    } else if b >= K {
        msg::core_unit_kb(&decimal(b / K, scales.2))
    } else {
        msg::core_unit_b(&plx_platform::i18n::current().number(bytes))
    }
}

/// `2019 · 8 min · Animation, Fantasy`: a title's facts, each only when known — the line a
/// description gives under a title (the Films preview, the title page, the player's Info card).
pub fn title_facts(year: Option<u16>, runtime_min: Option<u32>, genres: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y.to_string());
    }
    if let Some(m) = runtime_min {
        parts.push(dur_long(i64::from(m) * 60_000));
    }
    if !genres.is_empty() {
        parts.push(genres.join(", "));
    }
    parts.join(" · ")
}
