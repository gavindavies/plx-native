//! **The player's Info card** (`PiTV Redesign.dc.html` 1d — the top menu's first item): what is
//! playing, read without leaving it. The title, its facts (`2012 · 12 min · Science Fiction`), its
//! synopsis and the picture and sound actually playing (`1920 × 1080 · 24 fps · Dolby Digital
//! 5.1`).
//!
//! A local file is described by its own `.nfo` through the library's listing
//! (`plx_data::local::entry_for`), which is read on every draw rather than captured at open: a
//! film played from Home may open the card before its folder has been listed, and the description
//! then arrives on a later frame instead of never.
//!
//! The card holds no focus: it is read, and BACK or OK puts it away
//! (`screens::player::overlay`).
use plx_data::metadata::MetadataView;
use plx_media::playback::PlaybackSession;
use plx_ui::consts::SCR_W;
use plx_ui::popover::Popover;
use plx_ui::text_view::TextView;
use plx_ui::{theme, Painter, Rect};
use std::ffi::CString;
use std::path::PathBuf;

/// The card's width, its corner and its inset.
const CARD_W: f32 = 1040.0;
const CARD_RAD: f32 = 28.0;
const PAD: f32 = 44.0;
/// The card's top edge: under the transport's top menu, with the track panel's air.
const CARD_TOP: f32 = crate::player_hud::MENU_BOTTOM + 16.0;
/// Air between the card's blocks, and the synopsis' line cap.
const BLOCK_AIR: f32 = 20.0;
const PLOT_LINES: usize = 6;
/// How many genres the facts line names.
const GENRES_SHOWN: usize = 3;

pub struct InfoCard {
    /// The file playing, when it is one on this machine's disk.
    path: Option<PathBuf>,
    /// The session's display title — the fallback when the file has no `.nfo` title.
    title: String,
    /// The picture and sound playing.
    tech: String,
}

impl InfoCard {
    pub fn new(ps: &PlaybackSession, meta: MetadataView<'_>) -> Self {
        let title = unsafe { std::ffi::CStr::from_ptr(plx_media::playback::title_cptr(ps)) }.to_string_lossy().into_owned();
        let item = meta.playing();
        let path = item.map(|i| PathBuf::from(&i.rk)).filter(|p| p.is_absolute());
        Self { path, title, tech: item.map(|i| tech_line(ps, i)).unwrap_or_default() }
    }

    /// The card's lines, resolved now: `(title, facts, synopsis)`.
    fn described(&self) -> (String, String, String) {
        let entry = self.path.as_deref().and_then(plx_data::local::entry_for);
        let Some(meta) = entry.as_ref().and_then(|e| e.meta.clone()) else {
            return (self.title.clone(), String::new(), String::new());
        };
        let title = if meta.title.is_empty() { self.title.clone() } else { meta.title.clone() };
        let facts = plx_ui::fmt::title_facts(meta.year, meta.runtime_min, &meta.genres[..meta.genres.len().min(GENRES_SHOWN)]);
        (title, facts, meta.plot.clone())
    }

    pub fn draw(&self, appear: f32, measure: &dyn plx_machine::machine::Measure) {
        let p = Painter::root().alpha(appear).translate(0.0, Popover::RISE * (1.0 - appear));
        let (title, facts, plot) = self.described();
        let x = (SCR_W - CARD_W) * 0.5 + PAD;
        let w = CARD_W - 2.0 * PAD;
        // Lay the blocks out first, so the card hugs them.
        let title_view = TextView::new(&title, theme::size::TITLE, theme::TEXT_PRIMARY).bold().max_lines(2).with_measure(measure);
        let plot_view = TextView::new(&plot, theme::size::LABEL, theme::TEXT_READING).max_lines(PLOT_LINES).with_measure(measure);
        let line_h = |sz| {
            let (top, bottom) = plx_gfx::text::text_cap_band(sz, 0);
            bottom - top
        };
        let mut h = PAD + title_view.measure_h(w);
        for (text, sz) in [(&facts, theme::size::BODY), (&self.tech, theme::size::CAPTION)] {
            if !text.is_empty() {
                h += BLOCK_AIR + line_h(sz);
            }
        }
        if !plot.is_empty() {
            h += BLOCK_AIR + plot_view.measure_h(w);
        }
        h += PAD;
        let card = Rect::new(x - PAD, CARD_TOP, CARD_W, h);
        p.rect(card, CARD_RAD, theme::PANEL_TOP, theme::PANEL_BOT, 0.0);

        let mut y = CARD_TOP + PAD;
        y += title_view.draw(p, Rect::new(x, y, w, 0.0));
        let line = |text: &str, sz, ink, y: &mut f32| {
            if text.is_empty() {
                return;
            }
            *y += BLOCK_AIR;
            let fitted = plx_gfx::text::elide_by(text, w, false, |t| measure.width_str(t, sz, false));
            if let Ok(cs) = CString::new(fitted) {
                let (top, _) = plx_gfx::text::text_cap_band(sz, 0);
                p.text(cs.as_ptr(), x, *y - top, sz, ink, 0, 0);
            }
            *y += line_h(sz);
        };
        line(&facts, theme::size::BODY, theme::TEXT_SECONDARY, &mut y);
        if !plot.is_empty() {
            y += BLOCK_AIR;
            y += plot_view.draw(p, Rect::new(x, y, w, 0.0));
        }
        line(&self.tech, theme::size::CAPTION, theme::TEXT_TERTIARY, &mut y);
    }
}

/// `1920 × 1080 · 24 fps · Dolby Digital 5.1`: the picture's size and rate, then the sound playing.
fn tech_line(ps: &PlaybackSession, item: &plx_data::metadata::PlayingItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if item.width > 0 && item.height > 0 {
        parts.push(format!("{} \u{d7} {}", item.width, item.height));
    }
    if item.video_fps > 0.0 {
        let rate = format!("{:.3}", item.video_fps);
        let rate = rate.trim_end_matches('0').trim_end_matches('.');
        parts.push(plx_platform::i18n::msg::widgets_tracks_fps(rate));
    }
    let line = crate::player_hud::TrackLine::now_in(ps, item);
    if let Some((layout, name)) = line.audio {
        parts.push([name, layout].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "));
    }
    parts.join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tech_line_names_the_picture_then_the_sound() {
        let mut item = plx_data::metadata::PlayingItem::with_subs(Vec::new());
        item.width = 1920;
        item.height = 1080;
        item.video_fps = 23.976;
        item.audio = vec![plx_data::metadata::Stream { id: 500, lang: "English".into(), codec: "ac3".into(), channels: 6, ..Default::default() }];
        let mut ps = PlaybackSession::IDLE;
        plx_media::playback::set_cur_audio(&mut ps, plx_media::playback::CarriedAudio::from_stream(&item.audio[0], 0));
        assert_eq!(tech_line(&ps, &item), "1920 \u{d7} 1080 \u{b7} 23.976 fps \u{b7} English Dolby Digital 5.1");
        item.video_fps = 24.0;
        item.audio.clear();
        assert_eq!(tech_line(&ps, &item), "1920 \u{d7} 1080 \u{b7} 24 fps");
    }
}
