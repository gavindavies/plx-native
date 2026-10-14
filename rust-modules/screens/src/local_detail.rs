//! **A local video's title page** (`PiTV Redesign.dc.html`, 1c): the film's backdrop full-bleed,
//! its logo (or its title), one line of facts with the file's format badges, its synopsis, and the
//! ways to play it — then a row of the other titles from its collection or folder.
//!
//! OK on a video card opens this page (`ContentReq::LocalDetail`); it took over the jobs the hold
//! menu used to be the only door to (play from the start, mark as watched), which the menu still
//! offers from a card.
//!
//! **What it shows is the catalog's own entry, not a second read of the disk.** The video is
//! looked up in its folder's listing — the grandparent's first (a film in its own folder is listed
//! there beside the other films, which is where its siblings are), else its own folder's — and
//! both are requested on mount, off the frame (`plx_data::local::request`). The entry carries the
//! `.nfo`, backdrop and logo the scan attached (`plx_data::local_meta`), so a page drawn before the
//! listing lands draws the file's name and fills in as it does.
//!
//! The controls are a [`Kind::Custom`] row of plain elements on a `cards::Stack`, so the row and
//! the shelf below share one focus engine, one reveal and one hit map:
//!
//! | control | press |
//! |---|---|
//! | **Resume m:ss** / **Play** | `ContentReq::LocalPlay`, resumed |
//! | ⟲ (only while part-watched) | `ContentReq::LocalPlay { from_start: true }` |
//! | ✓ | the item menu's own watched rows (`item_menu::Action`) |

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plx_data::local::{Entry, EntryKind, Listing};
use plx_data::local_progress::{self, Progress};
use plx_machine::machine::{Canon, Cx, Delivery, Edge, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx, GroupId, Handled,
    Host, InputEvent, InputKind, InstanceId, Key, LogicalState, Machine, MachineId, Measure};
use plx_machine::present::Provenance;
use plx_ui::art_ground::{ArtGround, Treatment};
use plx_ui::cards::{self, CardEvent, CardSource, Kind, RowStyle, SectionSpec, Stack, StackEvent, StackPage, TileLabel};
use plx_ui::consts::*;
use plx_ui::hero_logo::{HeroLogo, LogoRung};
use plx_ui::icons::Icon;
use plx_ui::label::{Label, VAlign};
use plx_ui::screen::{At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, RenderStrategy, Screen, ScreenEvent, Seat};
use plx_ui::theme;
use plx_ui::widgets::{self, Art, Button, CircleButton, CtlPop};
use plx_ui::{Painter, Rect};

use super::item_menu::Action;
use super::titles::{facts, file_facts};
use super::registry::{AppFx, ContentLike, ContentReq, ItemMenuReq, LocalArg, MenuKind, TrackKind, TrackRow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sec {
    Hero,
    More,
}
const HERO_GROUP: GroupId = GroupId(0);
const MORE_GROUP: GroupId = GroupId(1);

/// The controls' elements, in row order when present.
const PLAY_ELEM: u32 = 1;
const RESTART_ELEM: u32 = 2;
const WATCHED_ELEM: u32 = 3;
const SUBS_ELEM: u32 = 4;
const AUDIO_ELEM: u32 = 5;
const FIRST_CARD_ELEM: u32 = 0x1000;

/// The back crumb's cap top: the top chrome line.
const CRUMB_TOP: f32 = 54.0;
/// The title column: left edge, width, and where its logo band ends.
const COL_W: f32 = 780.0;
const LOGO_BOTTOM: f32 = 330.0;
/// The facts row's cap top, and the air from it to the synopsis.
const META_TOP: f32 = 360.0;
const META_TO_SYN: f32 = 30.0;
/// The air from the synopsis to the controls, and from the controls to the progress line.
const SYN_TO_CTRL: f32 = 38.0;
const CTRL_TO_PROGRESS: f32 = 24.0;
/// The controls: the mock's 76 px row (bigger than the 60 px control the browsing pages use — the
/// one thing on this page meant to be pressed, read from across a room), the air before the first
/// disc (the pill's own 20 and its 14 margin), and the progress line's bar.
const CTRL_H: f32 = 76.0;
const PILL_TO_DISC: f32 = 34.0;
const PROGRESS_W: f32 = 260.0;
const PROGRESS_H: f32 = 6.0;
/// The shelf of other titles: its heading's top at the least, and its heading band.
const MORE_TOP_MIN: f32 = 900.0;
const MORE_HEADING: f32 = 60.0;
/// The air between the facts and the format badges, and between badges.
const BADGE_LEAD: f32 = 20.0;
const BADGE_GAP: f32 = 8.0;
/// A landscape still's decode box: `STILL_SHELF`'s cell, popped.
const STILL_RES: (std::os::raw::c_int, std::os::raw::c_int) = (480, 270);
/// The most other titles the row offers.
const MORE_MAX: usize = 24;

/// The listings the video may be found in, nearest-with-siblings first: its folder's folder (a
/// film in its own folder is listed there among the other films), then its own folder.
fn homes(video: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = video.parent() {
        if let Some(up) = dir.parent() {
            out.push(up.to_path_buf());
        }
        out.push(dir.to_path_buf());
    }
    out
}

/// What the page knows of its video: the catalog entry, the listing it was found in, and the
/// other titles worth offering beside it.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    entry: Entry,
    /// The folder whose listing holds the entry: the crumb names it.
    home: PathBuf,
    /// The row's heading subject — the collection, else the folder — and its titles.
    more_of: String,
    more: Vec<Entry>,
}

/// Find `video` in the first of its `homes` whose listing (`listing` answers it) holds it.
fn find(video: &Path, listing: impl Fn(&Path) -> Option<Arc<Vec<Entry>>>) -> Option<Found> {
    homes(video).into_iter().find_map(|home| {
        let all = listing(&home)?;
        let entry = all.iter().find(|e| e.path == video)?.clone();
        let set = entry.collection();
        let (more_of, more): (String, Vec<Entry>) = if set.is_empty() {
            let name = home.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            (name, all.iter().filter(|e| e.kind == EntryKind::Video && e.path != video).take(MORE_MAX).cloned().collect())
        } else {
            (set.to_owned(), all.iter().filter(|e| e.collection() == set && e.path != video).take(MORE_MAX).cloned().collect())
        };
        Some(Found { entry, home, more_of, more })
    })
}

/// The format badges a title's facts end in: its container, a surround mix, and CC when it has
/// subtitles.
fn badges(entry: &Entry) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ext) = entry.path.extension() {
        out.push(ext.to_string_lossy().to_uppercase());
    }
    let meta = entry.meta.as_deref();
    match meta.and_then(|m| m.audio_channels) {
        Some(c) if c >= 8 => out.push("7.1".to_owned()),
        Some(c) if c >= 6 => out.push("5.1".to_owned()),
        _ => {}
    }
    if entry.sidecar_subs || meta.is_some_and(|m| m.has_subtitles) {
        out.push("CC".to_owned());
    }
    out
}

/// The controls present for a watch record, in row order: Play (or Resume), Restart while there
/// is a place to resume from, the subtitle and audio choices, and the watch mark (1c).
fn controls(mark: Option<Progress>) -> Vec<u32> {
    let resuming = mark.is_some_and(|m| !m.watched && m.offset_ms > 0);
    let mut out = vec![PLAY_ELEM];
    if resuming {
        out.push(RESTART_ELEM);
    }
    out.extend([SUBS_ELEM, AUDIO_ELEM, WATCHED_ELEM]);
    out
}

/// **A file's audio or subtitle choices**, as the title page's disc offers them: what each row
/// says and what choosing it keeps, and the one that will play — the viewer's saved choice, else
/// the file's own start (its first audio track the player carries; subtitles Off). Audio tracks the
/// player cannot carry are left out: a choice of one could not be kept.
fn track_rows(path: &str, item: &plx_data::metadata::PlayingItem, saved: Option<&local_progress::Choices>, kind: TrackKind) -> MenuKind {
    use plx_platform::i18n::msg;
    let name = |s: &plx_data::metadata::Stream, n: usize| {
        [s.lang.trim(), s.title.trim()].into_iter().find(|t| !t.is_empty()).map_or_else(|| msg::widgets_tracks_track_ordinal(n as i64 + 1), str::to_owned)
    };
    match kind {
        TrackKind::Audio => {
            let carried = |s: &plx_data::metadata::Stream| plx_media::player::engine::audio_payload_codec(&s.codec).is_some();
            let mut rows = Vec::new();
            let mut positions = Vec::new();
            for (i, a) in item.audio.iter().enumerate().filter(|(_, a)| carried(a)) {
                let mut detail = plx_appkit::track_menu::audio_descriptor(a);
                if !a.title.trim().is_empty() && a.title.trim() != name(a, i) {
                    detail = [detail, a.title.trim().to_owned()].into_iter().filter(|d| !d.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
                }
                rows.push(TrackRow { label: name(a, i), detail, act: Action::ChooseAudio(path.to_owned(), i as u32) });
                positions.push(i);
            }
            let chosen = saved.and_then(|c| c.audio).map(|i| i as usize).filter(|i| positions.contains(i));
            let current = chosen.or(positions.first().copied()).and_then(|i| positions.iter().position(|&p| p == i));
            MenuKind::Tracks { rows, current }
        }
        TrackKind::Subtitles => {
            use plx_data::local_progress::SubtitleChoice;
            let mut rows = vec![TrackRow { label: msg::widgets_tracks_off().to_owned(), detail: String::new(), act: Action::ChooseSubtitle(path.to_owned(), SubtitleChoice::Off) }];
            let choices = plx_media::localplay::subtitle_choices(&item.subs);
            for (i, (s, choice)) in item.subs.iter().zip(choices).enumerate() {
                let marks: Vec<&str> = [
                    s.forced.then(msg::widgets_badge_forced),
                    s.sdh.then(msg::widgets_badge_sdh),
                    s.external.then(msg::widgets_tracks_external_badge),
                ]
                .into_iter()
                .flatten()
                .collect();
                rows.push(TrackRow { label: name(s, i), detail: marks.join(" \u{b7} "), act: Action::ChooseSubtitle(path.to_owned(), choice) });
            }
            let current = match saved.and_then(|c| c.subtitle.as_ref()) {
                Some(want) => rows.iter().position(|r| matches!(&r.act, Action::ChooseSubtitle(_, c) if c == want)).or(Some(0)),
                None => Some(0),
            };
            MenuKind::Tracks { rows, current }
        }
    }
}

/// The other titles, as a row of landscape cards: each title's backdrop, else its poster.
struct More<'a> {
    entries: &'a [Entry],
    elems: &'a [u32],
    index: &'a HashMap<u32, usize>,
}

impl<H: Host<Elem = u32>> CardSource<H> for More<'_> {
    fn len(&self) -> usize { self.entries.len() }
    fn elem(&self, i: usize) -> u32 { self.elems.get(i).copied().unwrap_or(0) }
    fn index_of(&self, e: &u32) -> Option<usize> { self.index.get(e).copied() }
    fn art(&self, i: usize) -> Art<'_> {
        let key = self.entries.get(i).and_then(|e| e.backdrop.as_deref().or(e.art.as_deref())).unwrap_or("");
        Art::Thumb { sid: plx_data::local::ART_SRV, key, res: STILL_RES }
    }
    fn label(&self, i: usize) -> TileLabel {
        self.entries.get(i).map_or_else(TileLabel::default, |e| {
            let line = e.meta.as_deref().map_or_else(|| file_facts(e), |m| facts(m.year, m.runtime_min, &[]));
            TileLabel::titled(e.display_title(), &line)
        })
    }
}

struct Page {
    entry: EntryId,
    video: PathBuf,
    found: Option<Found>,
    mark: Option<Progress>,
    synced: Option<(u64, u64)>,
    changes: u64,
    /// The controls present, in row order.
    ctrls: Vec<u32>,
    /// The row of other titles, keyed by path.
    elems: Vec<u32>,
    index: HashMap<u32, usize>,
    keys: HashMap<PathBuf, u32>,
    next: u32,
}

/// Where everything in the title column sits, measured.
struct Flow {
    syn_top: f32,
    ctrl_top: f32,
    progress_top: f32,
    more_top: f32,
}

impl Page {
    fn sync(&mut self) -> bool {
        let now = (plx_data::local::revision(), local_progress::revision());
        if self.synced == Some(now) {
            return false;
        }
        self.synced = Some(now);
        let found = find(&self.video, |dir| match plx_data::local::listing(dir) {
            Listing::Ready(l) => Some(l),
            _ => None,
        });
        if found.is_some() || self.found.is_none() {
            self.found = found;
        }
        let size = self.found.as_ref().map_or_else(|| std::fs::metadata(&self.video).map_or(0, |m| m.len()), |f| f.entry.size);
        self.mark = local_progress::get(&self.video, size);
        self.ctrls = controls(self.mark);
        self.elems.clear();
        self.index.clear();
        for (i, e) in self.found.iter().flat_map(|f| f.more.iter()).enumerate() {
            let elem = *self.keys.entry(e.path.clone()).or_insert_with(|| {
                let k = self.next;
                self.next += 1;
                k
            });
            self.elems.push(elem);
            self.index.insert(elem, i);
        }
        self.changes += 1;
        true
    }

    fn title(&self) -> String {
        match &self.found {
            Some(f) => f.entry.display_title().to_owned(),
            None => self.video.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
        }
    }

    fn synopsis(&self) -> &str {
        self.found.as_ref().and_then(|f| f.entry.meta.as_deref()).map_or("", |m| m.plot.as_str())
    }

    fn resuming(&self) -> Option<Progress> {
        self.mark.filter(|m| !m.watched && m.offset_ms > 0)
    }

    fn play_label(&self) -> String {
        match self.resuming() {
            Some(m) => plx_platform::i18n::msg::browse_local_resume_time(&plx_ui::fmt::clock(m.offset_ms as i64)),
            None => plx_platform::i18n::msg::browse_detail_play().to_owned(),
        }
    }

    fn flow(&self) -> Flow {
        let badge_h = 34.0;
        let syn_top = META_TOP + badge_h + META_TO_SYN;
        let syn = self.synopsis();
        let syn_h = if syn.is_empty() { 0.0 } else { plx_ui::hero_synopsis(syn, "").measure_h(COL_W) + SYN_TO_CTRL };
        let ctrl_top = syn_top + syn_h;
        let progress_top = ctrl_top + CTRL_H + CTRL_TO_PROGRESS;
        let more_top = (progress_top + 2.0 * CTRL_TO_PROGRESS + 24.0).max(MORE_TOP_MIN);
        Flow { syn_top, ctrl_top, progress_top, more_top }
    }

    /// The controls' rects, in row order: the pill sized to its label, then the discs.
    fn ctrl_rects(&self, measure: &dyn Measure) -> Vec<Rect> {
        let top = self.flow().ctrl_top;
        let label = CString::new(self.play_label()).unwrap_or_default();
        let mut x = MARGIN_X;
        let mut out = Vec::with_capacity(self.ctrls.len());
        for (n, _) in self.ctrls.iter().enumerate() {
            if n == 0 {
                let w = Button::pill_w_measured(&label, theme::size::BODY, true, false, measure);
                out.push(Rect::new(x, top, w, CTRL_H));
                x += w + PILL_TO_DISC;
            } else {
                out.push(Rect::new(x, top, CTRL_H, CTRL_H));
                x += CTRL_H + widgets::CTRL_GAP;
            }
        }
        out
    }

    fn more(&self) -> Option<More<'_>> {
        let f = self.found.as_ref().filter(|f| !f.more.is_empty())?;
        Some(More { entries: &f.more, elems: &self.elems, index: &self.index })
    }
}

impl<H: ContentLike> StackPage<H> for Page {
    type Key = Sec;
    type Cards<'a> = More<'a>;

    fn revision(&self, _cx: &Cx<'_, H>) -> u64 {
        self.changes
    }

    fn sections(&self, _cx: &Cx<'_, H>, out: &mut Vec<SectionSpec<Sec>>) {
        let flow = self.flow();
        out.push(SectionSpec::new(Sec::Hero, Kind::Custom { height: flow.more_top, focusable: true }, HERO_GROUP));
        out.push(SectionSpec::new(Sec::More, Kind::Shelf { style: &RowStyle::STILL_SHELF, heading: MORE_HEADING }, MORE_GROUP));
    }

    fn fallback(&self, _cx: &Cx<'_, H>, out: &mut Vec<Sec>) {
        out.extend([Sec::Hero, Sec::More]);
    }

    fn cards<'a>(&'a self, _cx: &'a Cx<'_, H>, k: Sec) -> Option<More<'a>> {
        (k == Sec::More).then(|| self.more()).flatten()
    }

    fn plain_len(&self, k: Sec) -> usize {
        if k == Sec::Hero { self.ctrls.len() } else { 0 }
    }

    fn plain_elem(&self, k: Sec, n: usize) -> Option<u32> {
        (k == Sec::Hero).then(|| self.ctrls.get(n).copied()).flatten()
    }

    fn plain_step(&self, k: Sec, at: usize, dir: Dir) -> Option<usize> {
        match (k, dir) {
            (Sec::Hero, Dir::Left) => at.checked_sub(1),
            (Sec::Hero, Dir::Right) => (at + 1 < self.ctrls.len()).then_some(at + 1),
            _ => None,
        }
    }

    fn element_rect(&self, cx: &Cx<'_, H>, k: Sec, n: usize, section: Rect) -> Rect {
        let r = self.ctrl_rects(cx.measure).get(n).copied().unwrap_or(Rect::new(MARGIN_X, 0.0, CTRL_H, CTRL_H));
        // the flow is in document space; the section's rect carries the page's scroll
        if k == Sec::Hero { Rect::new(r.x, r.y + section.y, r.w, r.h) } else { r }
    }

    fn focus_rect(&self, cx: &Cx<'_, H>, k: Sec, section: Rect) -> Rect {
        let rects = self.ctrl_rects(cx.measure);
        let (Some(first), Some(last)) = (rects.first(), rects.last()) else { return section };
        let _ = k;
        Rect::new(first.x, first.y + section.y, last.x + last.w - first.x, CTRL_H)
    }

    fn plain_group(&self, _cx: &Cx<'_, H>, k: Sec, id: GroupId, extent: Rect) -> GroupSpec {
        GroupSpec { id, kind: GroupKind::Row { wrap: false }, seat: Seat::First, reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop, EdgeRule::Geometric, EdgeRule::Stop, EdgeRule::Stop], extent,
            len: if k == Sec::Hero { self.ctrls.len() } else { 0 }, elem: ElemKind::Bare }
    }

    fn draw_heading(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, r: Rect, lift: f32) {
        let (Sec::More, Some(found)) = (k, &self.found) else { return };
        let p = f.painter.alpha(f.page_alpha);
        let heading = plx_platform::i18n::msg::browse_local_more_in(&found.more_of);
        let directors = found.entry.meta.as_deref().map_or_else(Vec::new, |m| m.directors.clone());
        let by = if directors.is_empty() {
            String::new()
        } else {
            format!("{} {}", plx_platform::i18n::msg::browse_detail_directed_by(), directors.join(", "))
        };
        cards::draw_heading(p, &heading, &by, MARGIN_X, r.y - lift, SCR_W - 2.0 * MARGIN_X, f.measure);
    }
}

pub struct LocalDetailScreen {
    page: Page,
    instance: InstanceId,
    /// The page has seated focus on its first control: it opens with Play under the cursor.
    seated: bool,
    drawn: Option<u64>,
    stack: Stack<Sec>,
    ground: ArtGround,
    pop: CtlPop<5>,
}

plx_ui::focusable_via_view!(LocalDetailScreen, H: [ContentLike], view);

impl LogicalState for LocalDetailScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.page.entry.0).str(&self.page.video.to_string_lossy()).u32(self.page.next);
        self.stack.write(c);
        self.pop.write_motion(c);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("localdetail path={} found={}", self.page.video.display(), self.page.found.is_some()));
    }
}

impl LocalDetailScreen {
    pub const SHAPE: &'static str = "LocalDetailScreen{entry:u32,path:String,next_elem:u32,stack:Stack{scroll:{pos:f32,vel:f32},target:f32,sections:[Shelf{pop,scroll}]},pop:[{pos:f32,vel:f32}]}";

    pub fn new(entry: EntryId, instance: InstanceId, arg: &LocalArg) -> Self {
        let mut page = Page {
            entry,
            video: PathBuf::from(&arg.path),
            found: None,
            mark: None,
            synced: None,
            changes: 0,
            ctrls: controls(None),
            elems: Vec::new(),
            index: HashMap::new(),
            keys: HashMap::new(),
            next: FIRST_CARD_ELEM,
        };
        let _ = page.sync();
        Self { page, instance, seated: false, drawn: None, stack: Stack::new(entry), ground: ArtGround::new(Treatment::Sharp), pop: CtlPop::new() }
    }

    fn view<H: ContentLike>(&self) -> impl Focusable<H> + '_ {
        self.stack.view(&self.page)
    }

    fn request(&self) {
        for home in homes(&self.page.video) {
            plx_data::local::request(&home);
        }
        // the file's tracks, read now so the subtitle and audio discs open at once
        let _ = plx_media::localplay::tracks(&self.page.video);
    }

    /// **The menu a subtitle or audio disc opens**: the file and its choices. `None` while the
    /// file's tracks are still being read.
    pub fn track_menu(&self, kind: TrackKind) -> Option<(String, MenuKind)> {
        let item = plx_media::localplay::tracks(&self.page.video)?;
        let path = self.page.video.to_string_lossy().into_owned();
        let size = self.page.found.as_ref().map_or(0, |f| f.entry.size);
        let saved = local_progress::choices(&self.page.video, size);
        let menu = track_rows(&path, &item, saved.as_ref(), kind);
        Some((path, menu))
    }

    fn focused_ctrl<H: ContentLike>(&self, cx: &Cx<'_, H>) -> Option<usize> {
        let key = cx.focus.current.filter(|k| k.entry == self.page.entry)?;
        self.page.ctrls.iter().position(|&e| e == key.elem)
    }

    fn press<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let path = self.page.video.to_string_lossy().into_owned();
        let fxa = match elem {
            PLAY_ELEM => AppFx::Content(ContentReq::LocalPlay { path, from_start: false }),
            RESTART_ELEM => AppFx::Content(ContentReq::LocalPlay { path, from_start: true }),
            SUBS_ELEM => AppFx::Content(ContentReq::TrackMenu(TrackKind::Subtitles)),
            AUDIO_ELEM => AppFx::Content(ContentReq::TrackMenu(TrackKind::Audio)),
            WATCHED_ELEM => {
                let watched = self.page.mark.is_some_and(|m| m.watched);
                let act = if watched { Action::MarkUnwatched(path) } else { Action::MarkWatched(path) };
                AppFx::ItemMenu(ItemMenuReq { act })
            }
            card => {
                let Some(e) = self.page.index.get(&card).and_then(|&i| self.page.found.as_ref()?.more.get(i)) else { return };
                AppFx::Content(ContentReq::LocalDetail(LocalArg { path: e.path.to_string_lossy().into_owned() }))
            }
        };
        fx.push(Fx::App(fxa));
        fx.invalidate(Provenance::Input);
    }

    fn draw_crumb(&self, p: Painter, measure: &dyn Measure) {
        let Some(found) = &self.page.found else { return };
        let Some(name) = found.home.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        let Ok(c) = CString::new(name) else { return };
        let sz = theme::size::CAPTION;
        let cap = measure.cap_h(sz);
        let g = (cap * 1.2).round();
        plx_ui::icons::draw(p, Icon::ChevronLeft, Rect::new(MARGIN_X, CRUMB_TOP + (cap - g) * 0.5, g, g), theme::TEXT_SECONDARY);
        Label::new(c.as_ptr(), sz, theme::TEXT_SECONDARY).v(VAlign::CapTop).draw(p, Rect::new(MARGIN_X + g + 10.0, CRUMB_TOP, COL_W, 0.0));
    }

    fn draw_column<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, scroll: f32) {
        let p = f.painter.alpha(f.page_alpha).translate(0.0, -scroll);
        let measure = f.measure;
        let flow = self.page.flow();
        let title = self.page.title();
        let logo = self.page.found.as_ref().and_then(|f| f.entry.logo.clone()).unwrap_or_default();
        let band_h = plx_ui::hero_logo::band_h(LogoRung::Hero);
        HeroLogo::new(plx_data::local::ART_SRV, &logo, &title, LogoRung::Hero)
            .draw(p, Rect::new(MARGIN_X, LOGO_BOTTOM - band_h, COL_W, band_h), measure);
        // the facts, then the format badges centred on their cap band
        let (line, chips) = match &self.page.found {
            Some(found) => (
                found.entry.meta.as_deref().map_or_else(|| file_facts(&found.entry), |m| facts(m.year, m.runtime_min, &m.genres)),
                badges(&found.entry),
            ),
            None => (String::new(), Vec::new()),
        };
        let w = widgets::facts_line(p, MARGIN_X, META_TOP, COL_W, &line, None, measure);
        let cy = META_TOP + measure.cap_h(theme::size::BODY) * 0.5;
        let mut x = MARGIN_X + w + if w > 0.0 { BADGE_LEAD } else { 0.0 };
        for chip in &chips {
            x += widgets::keyline_chip(p, x, cy, chip, theme::TEXT_SECONDARY, measure) + BADGE_GAP;
        }
        let syn = self.page.synopsis();
        if !syn.is_empty() {
            plx_ui::hero_synopsis(syn, "").draw(p, Rect::new(MARGIN_X, flow.syn_top, COL_W, 0.0));
        }
        // the controls
        let focused = self.focused_ctrl(f.cx);
        let rects = self.page.ctrl_rects(measure);
        let label = CString::new(self.page.play_label()).unwrap_or_default();
        for (n, (&elem, &r)) in self.page.ctrls.iter().zip(rects.iter()).enumerate() {
            let on = focused == Some(n);
            let scale = self.pop.scale_with(n, f.press.dip_of(&FocusKey { entry: self.page.entry, elem }));
            if elem == PLAY_ELEM {
                plx_ui::View::draw(&Button::new(label.as_ptr(), theme::size::BODY, r).icon(Icon::Play).focused(on).scale(scale), &plx_ui::Env::inert(), p);
            } else {
                let icon = match elem {
                    RESTART_ELEM => Icon::Restart,
                    SUBS_ELEM => Icon::Cc,
                    AUDIO_ELEM => Icon::Audio,
                    _ if self.page.mark.is_some_and(|m| m.watched) => Icon::CheckCircleFill,
                    _ => Icon::Check,
                };
                plx_ui::View::draw(&CircleButton::new(std::ptr::null()).frame(r).icon(icon).focused(on).scale(scale), &plx_ui::Env::inert(), p);
            }
        }
        // how far in, and what is left
        if let Some(m) = self.page.resuming() {
            let frac = local_progress::fraction(&m).unwrap_or(0.0);
            let bar = Rect::new(MARGIN_X, flow.progress_top, PROGRESS_W, PROGRESS_H);
            widgets::progress_bar(p, bar, PROGRESS_H * 0.5, PROGRESS_H, frac);
            let left = plx_ui::fmt::time_left(m.duration_ms.saturating_sub(m.offset_ms) as i64);
            if let Ok(c) = CString::new(left) {
                Label::new(c.as_ptr(), theme::size::CAPTION, theme::TEXT_TERTIARY).v(VAlign::Middle)
                    .draw(p, Rect::new(MARGIN_X + PROGRESS_W + 22.0, flow.progress_top - 12.0, COL_W, PROGRESS_H + 24.0));
            }
        }
    }
}

impl<H: ContentLike> Machine<H> for LocalDetailScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        if let ScreenEvent::Tick(t) = ev {
            if self.page.found.is_none() {
                self.request();
            }
            let _ = self.page.sync();
            if self.drawn != Some(self.page.changes) {
                fx.invalidate(Provenance::Landing(fx.from()));
            }
            let art = self.page.found.as_ref().and_then(|f| f.entry.backdrop.clone().or_else(|| f.entry.art.clone()));
            self.ground.key(plx_data::local::ART_SRV, art.as_deref());
            self.ground.tick(t.dt());
            let focused = self.focused_ctrl(cx);
            self.pop.step(focused, t.dt());
            if !self.seated {
                self.seated = true;
                if !cx.focus.current.is_some_and(|k| k.entry == self.page.entry) {
                    fx.push(Fx::Deliver(MachineId::Instance(self.instance), Delivery::Screen(ScreenEvent::Enter(
                        Enter::Fresh { focus: FocusTarget::Elem(FocusKey { entry: self.page.entry, elem: PLAY_ELEM }) }))));
                }
            }
        }
        let card = match self.stack.on(&self.page, ev, cx, fx) {
            Some(StackEvent::Card(_, card)) => Some(card),
            _ => None,
        };
        match ev {
            ScreenEvent::Mount | ScreenEvent::Enter(_) | ScreenEvent::Uncover => {
                self.request();
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::Activate(elem) if self.page.ctrls.contains(elem) => {
                self.press(*elem, fx);
                Handled::Yes
            }
            ScreenEvent::PressCommit(_) => {
                if let Some(CardEvent::Activate(elem)) = card {
                    self.press(elem, fx);
                }
                Handled::Yes
            }
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. }) => {
                fx.push(Fx::App(AppFx::Content(ContentReq::Back)));
                Handled::Yes
            }
            ScreenEvent::Tick(_) => Handled::Yes,
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Screen<H> for LocalDetailScreen {
    fn redraw_focused(&self, f: &mut DrawFrame<'_, '_, H>, focus: Option<FocusKey<u32>>) {
        self.stack.view(&self.page).redraw_focused(f, focus);
    }
    fn name(&self) -> &'static str { super::registry::word::LOCAL_DETAIL }
    fn state(&self) -> &dyn LogicalState { self }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<std::borrow::Cow<'_, str>> { None }
    fn prepare(&mut self, _budget: &mut plx_ui::frame::Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        self.drawn = Some(self.page.changes);
        let p = f.painter.alpha(f.page_alpha);
        let scroll = self.stack.scroll();
        // The surface stands still under a page that has scrolled its backdrop up off the top.
        p.rrect(Rect::FULL, 0.0, 0.0, theme::SURFACE_APP);
        self.ground.draw(p.translate(0.0, -scroll));
        self.draw_crumb(p.translate(0.0, -scroll), f.measure);
        self.draw_column(f, scroll);
        self.stack.view(&self.page).paint(f);
    }
    fn render(&self) -> RenderStrategy { RenderStrategy::Page }
    fn focus_source(&self) -> FocusSource { FocusSource::Engine }
    fn hit_source(&self) -> HitSource { HitSource::Engine }
    fn focused_card_rect(&self, cx: &Cx<'_, H>, focus: Option<FocusKey<u32>>, at: At) -> Option<Rect> {
        let key = focus.filter(|k| k.entry == self.page.entry)?;
        Focusable::<H>::place(self, &key.elem, cx, at).map(|p| p.rect)
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> { Some(self) }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> { Some(self) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plx_data::local_meta::Meta;

    fn film(dir: &str, title: &str, set: &str) -> Entry {
        let mut e = Entry::new(title.into(), PathBuf::from(format!("{dir}/{title}/{title}.mkv")), EntryKind::Video, None, 1);
        e.meta = Some(Arc::new(Meta { collection: set.into(), ..Meta::default() }));
        e
    }

    #[test]
    fn a_film_is_found_beside_its_collection_in_its_folders_folder() {
        let films = Arc::new(vec![
            film("/m/Films", "Charge", "Blender Open Movies"),
            film("/m/Films", "Nosferatu", "Silent Classics"),
            film("/m/Films", "Spring", "Blender Open Movies"),
        ]);
        let video = PathBuf::from("/m/Films/Charge/Charge.mkv");
        let f = find(&video, |d| (d == Path::new("/m/Films")).then(|| Arc::clone(&films))).unwrap();
        assert_eq!(f.home, PathBuf::from("/m/Films"));
        assert_eq!(f.more_of, "Blender Open Movies");
        assert_eq!(f.more.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["Spring"], "its collection, not itself");
        assert_eq!(find(&video, |_| None), None, "nothing landed yet");
    }

    #[test]
    fn a_video_in_no_collection_offers_its_folders_other_videos() {
        let mut a = Entry::new("a".into(), PathBuf::from("/m/Show/S01/a.mkv"), EntryKind::Video, None, 1);
        a.meta = None;
        let b = Entry::new("b".into(), PathBuf::from("/m/Show/S01/b.mkv"), EntryKind::Video, None, 1);
        let dir = Arc::new(vec![a, b]);
        let f = find(Path::new("/m/Show/S01/a.mkv"), |d| (d == Path::new("/m/Show/S01")).then(|| Arc::clone(&dir))).unwrap();
        assert_eq!(f.more_of, "S01");
        assert_eq!(f.more.len(), 1);
    }

    #[test]
    fn the_badges_name_the_container_a_surround_mix_and_subtitles() {
        let mut e = film("/m", "Charge", "");
        assert_eq!(badges(&e), ["MKV"]);
        e.meta = Some(Arc::new(Meta { audio_channels: Some(6), ..Meta::default() }));
        e.sidecar_subs = true;
        assert_eq!(badges(&e), ["MKV", "5.1", "CC"]);
        e.meta = Some(Arc::new(Meta { audio_channels: Some(2), has_subtitles: true, ..Meta::default() }));
        e.sidecar_subs = false;
        assert_eq!(badges(&e), ["MKV", "CC"], "stereo is not worth a badge");
    }

    #[test]
    fn restart_is_offered_only_while_part_watched() {
        let p = |offset_ms, watched| Some(Progress { offset_ms, duration_ms: 100_000, watched, size: 1 });
        let has_restart = |c: Vec<u32>| c.contains(&RESTART_ELEM);
        assert!(!has_restart(controls(None)));
        assert!(has_restart(controls(p(30_000, false))));
        assert!(!has_restart(controls(p(30_000, true))), "a finished film plays from the start anyway");
    }

    fn stream(id: i64, lang: &str, codec: &str) -> plx_data::metadata::Stream {
        plx_data::metadata::Stream { id, lang: lang.into(), codec: codec.into(), channels: 6, ..Default::default() }
    }

    #[test]
    fn the_audio_choices_leave_out_what_cannot_play_and_check_the_saved_one() {
        let mut item = plx_data::metadata::PlayingItem::with_subs(Vec::new());
        item.audio = vec![stream(500, "English", "truehd"), stream(501, "English", "ac3"), stream(502, "French", "aac")];
        let MenuKind::Tracks { rows, current } = track_rows("/m/f.mkv", &item, None, TrackKind::Audio) else { panic!() };
        let acts: Vec<_> = rows.iter().map(|r| r.act.clone()).collect();
        let carried: Vec<u32> = (0..3).filter(|&i| plx_media::player::engine::audio_payload_codec(&item.audio[i as usize].codec).is_some()).collect();
        assert_eq!(acts, carried.iter().map(|&i| Action::ChooseAudio("/m/f.mkv".into(), i)).collect::<Vec<_>>());
        assert_eq!(current, Some(0), "unchosen: the first track that plays");
        let saved = local_progress::Choices { audio: Some(2), ..Default::default() };
        let MenuKind::Tracks { rows, current } = track_rows("/m/f.mkv", &item, Some(&saved), TrackKind::Audio) else { panic!() };
        assert_eq!(rows[current.unwrap()].act, Action::ChooseAudio("/m/f.mkv".into(), 2));
        assert_eq!(rows[current.unwrap()].label, "French");
    }

    #[test]
    fn the_subtitle_choices_start_with_off_and_name_a_file_beside_the_video_by_its_name() {
        use plx_data::local_progress::SubtitleChoice;
        let side = plx_data::metadata::Stream { id: 1000, lang: "German".into(), external: true, key: "/m/f.de.srt".into(), ..Default::default() };
        let item = plx_data::metadata::PlayingItem::with_subs(vec![stream(1, "English", "subrip"), side]);
        let MenuKind::Tracks { rows, current } = track_rows("/m/f.mkv", &item, None, TrackKind::Subtitles) else { panic!() };
        let acts: Vec<_> = rows.iter().map(|r| r.act.clone()).collect();
        assert_eq!(acts, [
            Action::ChooseSubtitle("/m/f.mkv".into(), SubtitleChoice::Off),
            Action::ChooseSubtitle("/m/f.mkv".into(), SubtitleChoice::Track(0)),
            Action::ChooseSubtitle("/m/f.mkv".into(), SubtitleChoice::File("f.de.srt".into())),
        ]);
        assert_eq!(current, Some(0), "unchosen: Off");
        assert!(!rows[2].detail.is_empty(), "a file beside the video says so");
        let saved = local_progress::Choices { subtitle: Some(SubtitleChoice::File("f.de.srt".into())), ..Default::default() };
        let MenuKind::Tracks { current, .. } = track_rows("/m/f.mkv", &item, Some(&saved), TrackKind::Subtitles) else { panic!() };
        assert_eq!(current, Some(2));
    }

    #[test]
    fn the_controls_offer_subtitles_and_audio_before_the_watch_mark() {
        let resuming = Progress { offset_ms: 5_000, duration_ms: 60_000, watched: false, size: 1 };
        assert_eq!(controls(Some(resuming)), [PLAY_ELEM, RESTART_ELEM, SUBS_ELEM, AUDIO_ELEM, WATCHED_ELEM]);
        assert_eq!(controls(None), [PLAY_ELEM, SUBS_ELEM, AUDIO_ELEM, WATCHED_ELEM]);
    }
}
