//! **The local library page: one folder of the machine's own disk, as shelves or as a grid.**
//!
//! The Linux build's home and its folder browser (`plx_data::local` is the catalog). A page shows
//! ONE folder — the library's top level when its argument's path is empty — and opening a folder
//! card pushes another page for it, so BACK walks up the tree the way it walks back from any page.
//! OK on a video opens its title page (`ContentReq::LocalDetail`, `screens::local_detail`), which
//! plays it.
//!
//! **Two layouts, chosen by what the folder holds** (`PiTV Redesign.dc.html`, 1b):
//!
//! - **Shelves**, when the folder groups its titles: by collection (an `.nfo`'s `<set>`) and by
//!   plain subfolder (a folder that is not a show — `Silent Classics/`, `Season 01/`). Each group
//!   is one horizontal [`Shelf`](plx_ui::cards::Shelf), in that order, and whatever is left
//!   (titles in no collection, shows) is a last shelf under the folder's own name. A subfolder's
//!   shelf shows that folder's listing, scanned off the frame like any other; a shelf of a season's
//!   episodes is drawn as landscape stills. Above the shelves stands a PREVIEW of the focused
//!   title — its name and one line of facts — and the focused shelf is pinned under it
//!   ([`SHELF_TOP`]): the page scrolls a whole shelf at a time, tvOS's rhythm, and a shelf leaving
//!   upward is cut at the preview's foot rather than sliding under its words.
//! - **A grid**, otherwise (a flat folder of videos, a folder of shows): the folder's name and
//!   where it is, over a [`Grid`](plx_ui::cards::Grid) of every card.
//!
//! Either way the page stands on the focused title's artwork, blurred
//! ([`plx_ui::art_ground::ArtGround`]).
//!
//! A listing lands whole — there is no paging — so a list's content is the listing itself, keyed
//! by path ([`Page::keys`]) so a file added under the cursor never moves focus to its neighbour.
//!
//! **What it reads is process state, not a store view.** `plx_data::local` is the one catalog in
//! the app that is not a `Store` machine: it has no server, no account and nothing to record, and
//! the Linux build does not run the recorder. The page re-reads it when `local::revision` moves.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plx_data::local::{Entry, EntryKind, Listing};
use plx_data::local_progress::{self, Progress};
use plx_machine::machine::{Canon, Cx, Delivery, Edge, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx,
    GroupId, Handled, Host, InputEvent, InputKind, InstanceId, Key, LogicalState, Machine, MachineId, Measure};
use plx_machine::present::Provenance;
use plx_ui::art_ground::{ArtGround, Treatment};
use plx_ui::cards::{self, CardEvent, CardSource, GridSpec, Kind, RowStyle, SectionSpec, Stack, StackEvent, StackPage, Tile, TileLabel};
use plx_ui::consts::*;
use plx_ui::label::{Label, VAlign};
use plx_ui::screen::{At, AxisMask, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, RenderStrategy, Screen, ScreenEvent, Seat};
use plx_ui::theme;
use plx_ui::widgets::{self, Art, PosterMark, StatusKind, StatusOverlay};
use plx_ui::{Env, Painter, Rect};

use super::registry::{AppFx, ContentLike, ContentReq, LocalArg};

const RETRY_ELEM: u32 = 1;
const FIRST_CARD_ELEM: u32 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sec {
    Head,
    /// The grid layout's one grid.
    Items,
    /// The shelves layout's shelf `n`.
    Shelf(u16),
    Status,
}
const GRID_GROUP: GroupId = GroupId(0);
const HEADER_GROUP: GroupId = GroupId(1);
const STATUS_GROUP: GroupId = GroupId(2);
/// Shelf `n`'s group is `SHELF_GROUP0 + n`: ids are the page's, stable per shelf position.
const SHELF_GROUP0: u32 = 10;

const CONTENT_TOP: f32 = MARGIN_X;
const HEADER_TOP: f32 = CONTENT_TOP;
/// The path line's cap top under the DISPLAY title's — the collection header's meta offset.
const META_DY: f32 = 65.0;
/// The first row's poster top: the header block and one XL gap.
const GRID_TOP: f32 = 236.0;
const STATUS_FRAME: Rect = Rect { x: MARGIN_X, y: 300.0, w: SCR_W - 2.0 * MARGIN_X, h: 475.0 };
/// The poster box local artwork is decoded into (`widgets::POSTER_RES`, the catalog's own).
const ART_RES: (std::os::raw::c_int, std::os::raw::c_int) = widgets::POSTER_RES;
/// A landscape still's decode box: `STILL_SHELF`'s cell, popped.
const STILL_RES: (std::os::raw::c_int, std::os::raw::c_int) = (480, 270);
/// The glyph a card with no artwork wears, centred, as a share of the card's shorter side.
const GLYPH_SHARE: f32 = 0.34;

/// Where the focused shelf's heading rests: under the preview (the mock's 440).
pub const SHELF_TOP: f32 = 440.0;
/// A shelf's heading band: the `HEADLINE` heading and the air down to the posters (32 + 30).
const SHELF_HEADING: f32 = 62.0;
/// The preview: the focused title's cap top, and its facts line's.
const PREVIEW_TITLE_TOP: f32 = 248.0;
const PREVIEW_META_TOP: f32 = 344.0;
/// The preview's column: the mock's 1100.
const PREVIEW_W: f32 = 1100.0;
/// Where a shelf leaving upward is cut: just above the pinned heading, below the preview's facts.
const SHELF_CUT: f32 = SHELF_TOP - 36.0;
/// How many genres the facts line names.
const GENRES_SHOWN: usize = 3;

/// The listing a page shows: the folder's own, or the library's top level.
fn listing_of(path: &str) -> Listing {
    if path.is_empty() {
        if let Some(top) = plx_data::local::top_level() {
            return Listing::Ready(top);
        }
        return match plx_data::local::roots().first() {
            Some(root) => plx_data::local::listing(root),
            None => Listing::Failed(String::new()),
        };
    }
    plx_data::local::listing(Path::new(path))
}

/// Ask for a scan of what the page shows (a no-op while one is in flight or fresh).
fn request_scan(path: &str) {
    if path.is_empty() {
        if plx_data::local::top_level().is_none() {
            if let Some(root) = plx_data::local::roots().first() {
                plx_data::local::request(root);
            }
        }
        return;
    }
    plx_data::local::request(Path::new(path));
}

/// Where a folder is, as the reader walked to it: its path below the library root that holds it,
/// one name per step (`Movies › Alien (1979)`), with the root's own name first when the library
/// has several. A folder under no root (a path handed over from elsewhere) reads as its path.
fn trail(path: &str, roots: &[PathBuf]) -> String {
    let path = Path::new(path);
    let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
        return path.display().to_string();
    };
    let mut steps: Vec<String> = Vec::new();
    if roots.len() > 1 {
        steps.push(root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned()));
    }
    if let Ok(rest) = path.strip_prefix(root) {
        steps.extend(rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    }
    steps.join(" › ")
}

/// The caption under a focused card: what the entry is.
fn caption(entry: &Entry) -> String {
    match entry.kind {
        EntryKind::Folder => match entry.meta.as_deref() {
            Some(m) if m.year.is_some() || !m.genres.is_empty() => facts(m.year, None, &m.genres),
            _ => plx_platform::i18n::msg::browse_local_folder().to_owned(),
        },
        EntryKind::Video => match entry.meta.as_deref() {
            Some(m) if m.year.is_some() || m.runtime_min.is_some() => facts(m.year, m.runtime_min, &[]),
            _ => file_facts(entry),
        },
    }
}

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

// ---- grouping ---------------------------------------------------------------------------------

/// One shelf of the shelves layout, before the page keys it.
#[derive(Debug, Clone, PartialEq)]
struct Group {
    heading: String,
    entries: Vec<Entry>,
    /// Drawn as landscape stills: a season's episodes.
    stills: bool,
}

/// What a folder's shelves are, or `None` when it is a grid: a shelf per collection (in the
/// order the listing first names each), a shelf per plain subfolder whose listing has landed
/// (`sub` answers it; a folder still being read has no shelf yet), then the rest under `name`.
/// A folder with no collection and no plain subfolder is a grid.
fn groups(entries: &[Entry], name: &str, sub: impl Fn(&Path) -> Option<Arc<Vec<Entry>>>) -> Option<Vec<Group>> {
    let plain = |e: &Entry| e.kind == EntryKind::Folder && !e.show;
    if !entries.iter().any(|e| !e.collection().is_empty() || plain(e)) {
        return None;
    }
    let mut out: Vec<Group> = Vec::new();
    for e in entries.iter().filter(|e| !e.collection().is_empty()) {
        match out.iter_mut().find(|g| g.heading == e.collection()) {
            Some(g) => g.entries.push(e.clone()),
            None => out.push(Group { heading: e.collection().to_owned(), entries: vec![e.clone()], stills: false }),
        }
    }
    for e in entries.iter().filter(|e| plain(e)) {
        let Some(inner) = sub(&e.path).filter(|l| !l.is_empty()) else { continue };
        let season = e.path.file_name().is_some_and(|n| plx_data::local::is_season_folder(&n.to_string_lossy()));
        let stills = season && inner.iter().all(|i| i.kind == EntryKind::Video);
        out.push(Group { heading: e.display_title().to_owned(), entries: inner.to_vec(), stills });
    }
    let rest: Vec<Entry> = entries.iter().filter(|e| e.collection().is_empty() && !plain(e)).cloned().collect();
    if !rest.is_empty() {
        out.push(Group { heading: name.to_owned(), entries: rest, stills: false });
    }
    Some(out)
}

/// The scroll that pins shelf `n`'s heading at [`SHELF_TOP`]: the settled height of every shelf
/// above it (each collapsed, since only the focused one holds its band open).
fn pinned_scroll(lists: &[List], n: usize) -> f32 {
    lists.iter().take(n).map(|l| SHELF_HEADING + l.style.h + cards::under_band(0.0)).sum()
}

// ---- content ----------------------------------------------------------------------------------

/// One list of cards — the grid, or one shelf — keyed for the engine.
struct List {
    heading: String,
    style: &'static RowStyle,
    entries: Vec<Entry>,
    /// `elems[i]` is card `i`'s element; `index` the inverse.
    elems: Vec<u32>,
    index: HashMap<u32, usize>,
    /// Card `i`'s watch record (`local_progress`), read when the page last synced.
    marks: Vec<Option<Progress>>,
}

impl List {
    fn cards(&self) -> Items<'_> {
        Items { entries: &self.entries, elems: &self.elems, index: &self.index, marks: &self.marks, stills: self.style.w > self.style.h }
    }
}

/// A list's content, read through the page's path-keyed elements.
pub struct Items<'a> {
    entries: &'a [Entry],
    elems: &'a [u32],
    index: &'a HashMap<u32, usize>,
    marks: &'a [Option<Progress>],
    stills: bool,
}

impl<H: Host<Elem = u32>> CardSource<H> for Items<'_> {
    fn len(&self) -> usize { self.entries.len() }
    fn elem(&self, i: usize) -> u32 { self.elems.get(i).copied().unwrap_or(RETRY_ELEM) }
    fn index_of(&self, e: &u32) -> Option<usize> { self.index.get(e).copied() }
    fn art(&self, i: usize) -> Art<'_> {
        let res = if self.stills { STILL_RES } else { ART_RES };
        let key = self.entries.get(i).and_then(|e| e.art.as_deref()).unwrap_or("");
        Art::Thumb { sid: plx_data::local::ART_SRV, key, res }
    }
    fn label(&self, i: usize) -> TileLabel {
        self.entries.get(i).map_or_else(TileLabel::default, |e| TileLabel::titled(e.display_title(), &caption(e)))
    }
    fn progress(&self, i: usize) -> Option<f32> {
        self.marks.get(i).copied().flatten().as_ref().and_then(local_progress::fraction)
    }
    fn overlay(&self, p: Painter, i: usize, tile: &Tile, measure: &dyn Measure) {
        let Some(entry) = self.entries.get(i) else { return };
        if self.marks.get(i).copied().flatten().is_some_and(|m| m.watched) {
            widgets::watched_mark(p, tile.rect, tile.radius);
        }
        if entry.art.is_none() {
            let side = tile.rect.w.min(tile.rect.h) * GLYPH_SHARE;
            let glyph = Rect::new(tile.rect.x + (tile.rect.w - side) / 2.0, tile.rect.y + (tile.rect.h - side) / 2.0 - side * 0.25, side, side);
            let icon = match entry.kind {
                EntryKind::Folder => plx_ui::icons::Icon::Collection,
                EntryKind::Video => plx_ui::icons::Icon::Play,
            };
            plx_ui::icons::draw(p, icon, glyph, theme::TEXT_SECONDARY);
            // A card with no artwork names itself on its ground; artwork carries its own title (a
            // poster, a folder.jpg), and the focused card's caption names it either way.
            widgets::poster_label(p, tile.rect, tile.radius, entry.display_title(), measure);
        }
    }
}

/// The page's layout of the moment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    Grid,
    Shelves,
}

struct Page {
    entry: EntryId,
    /// The folder shown; empty for the library's top level.
    path: String,
    /// The folder's name, the last shelf's heading.
    name: String,
    /// The listing as last read.
    entries: Arc<Vec<Entry>>,
    listing: Listing,
    /// The catalog revision the lists were built at, and the progress revision the marks were.
    synced: Option<u64>,
    marks_synced: Option<u64>,
    layout: Layout,
    /// The grid's one list, or one per shelf.
    lists: Vec<List>,
    /// Which list and card an element is.
    at: HashMap<u32, (usize, usize)>,
    /// Moves whenever anything a section draws moved: the page's own revision.
    changes: u64,
    /// One element per path, minted once and never reused for another path on this page.
    keys: HashMap<PathBuf, u32>,
    next: u32,
    title_c: CString,
    path_c: CString,
}

impl Page {
    /// Re-read the listing (and its subfolders') if the catalog moved, and the marks if progress
    /// did. `true` when anything did: the page has new ground to present.
    fn sync(&mut self) -> bool {
        let revision = plx_data::local::revision();
        let marks = local_progress::revision();
        if self.synced == Some(revision) && self.marks_synced == Some(marks) {
            return false;
        }
        if self.synced != Some(revision) {
            self.synced = Some(revision);
            self.listing = listing_of(&self.path);
            if let Listing::Ready(entries) = &self.listing {
                self.entries = Arc::clone(entries);
            }
        }
        self.marks_synced = Some(marks);
        self.rebuild();
        self.changes += 1;
        true
    }

    fn rebuild(&mut self) {
        let sub = |p: &Path| match plx_data::local::listing(p) {
            Listing::Ready(l) => Some(l),
            _ => None,
        };
        let (layout, groups) = match groups(&self.entries, &self.name, sub) {
            Some(g) => (Layout::Shelves, g),
            None => (Layout::Grid, vec![Group { heading: self.name.clone(), entries: self.entries.to_vec(), stills: false }]),
        };
        self.layout = layout;
        self.lists.clear();
        self.at.clear();
        for (n, g) in groups.into_iter().enumerate() {
            let style = match (layout, g.stills) {
                (Layout::Grid, _) => &cards::GRID_STYLE,
                (Layout::Shelves, true) => &RowStyle::STILL_SHELF,
                (Layout::Shelves, false) => &RowStyle::SHELF,
            };
            let mut list = List { heading: g.heading, style, entries: g.entries, elems: Vec::new(), index: HashMap::new(), marks: Vec::new() };
            for (i, e) in list.entries.iter().enumerate() {
                let elem = *self.keys.entry(e.path.clone()).or_insert_with(|| {
                    let k = self.next;
                    self.next += 1;
                    k
                });
                list.elems.push(elem);
                list.index.insert(elem, i);
                self.at.insert(elem, (n, i));
            }
            list.marks = list.entries.iter()
                .map(|e| if e.kind == EntryKind::Video { local_progress::get(&e.path, e.size) } else { None })
                .collect();
            self.lists.push(list);
        }
    }

    /// The plain subfolders whose listings the shelves are waiting for.
    fn pending_subfolders(&self) -> Vec<PathBuf> {
        self.entries.iter()
            .filter(|e| e.kind == EntryKind::Folder && !e.show)
            .filter(|e| matches!(plx_data::local::listing(&e.path), Listing::Loading))
            .map(|e| e.path.clone())
            .collect()
    }

    fn card(&self, elem: u32) -> Option<(&Entry, Option<Progress>)> {
        let &(n, i) = self.at.get(&elem)?;
        let list = self.lists.get(n)?;
        Some((list.entries.get(i)?, list.marks.get(i).copied().flatten()))
    }

    fn ready(&self) -> bool {
        matches!(self.listing, Listing::Ready(_)) && !self.entries.is_empty()
    }

    /// The read-out for a page with nothing to show.
    fn status_overlay<'a>(&self, tick: u32) -> StatusOverlay<'a> {
        match &self.listing {
            // placeholder-exempt: builds the value only; counted where drawn (StatusOverlay::draw_geometry, Working)
            Listing::Loading => StatusOverlay::new(STATUS_FRAME, plx_platform::i18n::msg::browse_local_loading_c(), StatusKind::Working).phase(tick),
            Listing::Ready(_) => StatusOverlay::new(STATUS_FRAME, plx_platform::i18n::msg::browse_local_empty_c(), StatusKind::Empty),
            Listing::Failed(_) => StatusOverlay::new(STATUS_FRAME, plx_platform::i18n::msg::browse_local_failed_c(), StatusKind::Failed)
                .action(plx_platform::i18n::msg::browse_action_retry_c()),
        }
    }

    /// The grid layout's header: the folder's name and where it is, scrolling away with the grid.
    fn draw_header(&self, p: Painter, scroll: f32) {
        let alpha = (1.0 - scroll / (GRID_TOP - CONTENT_TOP)).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }
        let p = p.alpha(alpha);
        let w = SCR_W - 2.0 * MARGIN_X;
        Label::new(self.title_c.as_ptr(), theme::size::DISPLAY, theme::TEXT_PRIMARY).bold().v(VAlign::CapTop)
            .draw(p, Rect::new(MARGIN_X, HEADER_TOP - scroll, w, 0.0));
        Label::new(self.path_c.as_ptr(), theme::size::LABEL, theme::TEXT_SECONDARY).v(VAlign::CapTop)
            .draw(p, Rect::new(MARGIN_X, HEADER_TOP + META_DY - scroll, w, 0.0));
    }

    /// The shelves layout's preview of the focused title: its name at `HERO`, and one line of
    /// facts ending, for a part-watched video, in where it would resume.
    fn draw_preview(&self, p: Painter, focused: Option<u32>, measure: &dyn Measure) {
        let Some((entry, mark)) = focused.and_then(|e| self.card(e)) else { return };
        let title = plx_gfx::text::elide_by(entry.display_title(), PREVIEW_W, false, |t| measure.width_str(t, theme::size::HERO, true));
        let Ok(title_c) = CString::new(title) else { return };
        Label::new(title_c.as_ptr(), theme::size::HERO, theme::TEXT_PRIMARY).bold().v(VAlign::CapTop)
            .draw(p, Rect::new(MARGIN_X, PREVIEW_TITLE_TOP, PREVIEW_W, 0.0));
        let line = match entry.meta.as_deref() {
            Some(m) => facts(m.year, m.runtime_min, &m.genres),
            None => caption(entry),
        };
        let resume = mark.filter(|m| !m.watched && m.offset_ms > 0)
            .map(|m| plx_platform::i18n::msg::browse_local_resume_at(&plx_ui::fmt::clock(m.offset_ms as i64)));
        widgets::facts_line(p, MARGIN_X, PREVIEW_META_TOP, PREVIEW_W, &line, resume.as_deref(), measure);
    }
}

impl<H: ContentLike> StackPage<H> for Page {
    type Key = Sec;
    type Cards<'a> = Items<'a>;

    /// A video's hold opens the item menu (Play from Start, the watched rows); a folder has none.
    fn card_has_menu(&self, _cx: &Cx<'_, H>, _k: Sec, elem: &u32) -> bool {
        self.card(*elem).is_some_and(|(e, _)| e.kind == EntryKind::Video)
    }

    fn revision(&self, _cx: &Cx<'_, H>) -> u64 {
        let status = match self.listing {
            Listing::Loading => 1,
            Listing::Ready(_) => 2,
            Listing::Failed(_) => 3,
        };
        self.changes << 2 | status
    }

    fn sections(&self, cx: &Cx<'_, H>, out: &mut Vec<SectionSpec<Sec>>) {
        match self.layout {
            Layout::Grid => {
                out.push(SectionSpec::new(Sec::Head, Kind::Custom { height: GRID_TOP, focusable: false }, HEADER_GROUP).ranked(1));
                out.push(SectionSpec::new(Sec::Items, Kind::Grid { spec: GridSpec::new(GRID_TOP, CONTENT_TOP) }, GRID_GROUP));
            }
            Layout::Shelves => {
                out.push(SectionSpec::new(Sec::Head, Kind::Custom { height: SHELF_TOP, focusable: false }, HEADER_GROUP).ranked(1));
                for (n, l) in self.lists.iter().enumerate() {
                    let id = GroupId(SHELF_GROUP0 + n as u32);
                    out.push(SectionSpec::new(Sec::Shelf(n as u16), Kind::Shelf { style: l.style, heading: SHELF_HEADING }, id));
                }
            }
        }
        let retry = if matches!(self.listing, Listing::Failed(_)) {
            self.status_overlay(cx.tick.ms).action_frame_measured(cx.measure)
        } else {
            None
        };
        out.push(SectionSpec::new(Sec::Status, Kind::Overlay { rect: retry.unwrap_or(STATUS_FRAME), focusable: retry.is_some() }, STATUS_GROUP).ranked(2));
    }

    fn fallback(&self, _cx: &Cx<'_, H>, out: &mut Vec<Sec>) {
        match self.layout {
            Layout::Grid => out.push(Sec::Items),
            Layout::Shelves => out.extend((0..self.lists.len()).map(|n| Sec::Shelf(n as u16))),
        }
        out.push(Sec::Status);
    }

    fn cards<'a>(&'a self, _cx: &'a Cx<'_, H>, k: Sec) -> Option<Items<'a>> {
        if !self.ready() {
            return None;
        }
        match (k, self.layout) {
            (Sec::Items, Layout::Grid) => self.lists.first().map(List::cards),
            (Sec::Shelf(n), Layout::Shelves) => self.lists.get(n as usize).map(List::cards),
            _ => None,
        }
    }

    fn elem_of(&self, k: Sec) -> Option<u32> {
        (k == Sec::Status).then_some(RETRY_ELEM)
    }

    fn plain_group(&self, _cx: &Cx<'_, H>, _k: Sec, id: GroupId, extent: Rect) -> GroupSpec {
        GroupSpec { id, kind: GroupKind::Free, seat: Seat::First, reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop; 4], extent, len: 1, elem: ElemKind::Bare }
    }

    fn reveal_margin(&self, _k: Sec) -> f32 {
        SHELF_TOP
    }

    fn draw_heading(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, r: Rect, lift: f32) {
        let Sec::Shelf(n) = k else { return };
        let Some(list) = self.lists.get(n as usize) else { return };
        let p = f.painter.alpha(f.page_alpha);
        cards::draw_heading(p, &list.heading, "", MARGIN_X, r.y - lift, SCR_W - 2.0 * MARGIN_X, f.measure);
    }

    fn custom_draw(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, _r: Rect, scroll: f32) {
        let p = f.painter.alpha(f.page_alpha);
        match k {
            Sec::Head if self.layout == Layout::Grid => self.draw_header(p, scroll),
            Sec::Status => if !self.ready() {
                let on = f.focus.current.is_some_and(|key| key.entry == self.entry && key.elem == RETRY_ELEM);
                self.status_overlay(f.cx.tick.ms).focused(on).draw_measured(&Env::inert(), p, f.measure);
            },
            _ => {}
        }
    }
}

pub struct LocalScreen {
    page: Page,
    /// The listing revision the page last DREW. A landing can be presented inside a transition
    /// that draws a frozen snapshot of the page, so the page asks for frames until its own draw
    /// has shown what it holds.
    drawn: Option<u64>,
    instance: InstanceId,
    /// The page has seated focus on its first card (or the reader got there first): a folder
    /// opens with a card under the cursor, as soon as it has one, and never again after that.
    seated: bool,
    stack: Stack<Sec>,
    ground: ArtGround,
}

plx_ui::focusable_via_view!(LocalScreen, H: [ContentLike], view);

impl LogicalState for LocalScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.page.entry.0).str(&self.page.path).u32(self.page.next);
        self.stack.write(c);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("local path={} cards={} layout={:?}", self.page.path, self.page.entries.len(), self.page.layout));
    }
}

impl LocalScreen {
    pub const SHAPE: &'static str = "LocalScreen{entry:u32,path:String,next_elem:u32,stack:Stack{scroll:{pos:f32,vel:f32},target:f32,sections:[Grid{bands,pop}|Shelf{pop,scroll}]}}";

    pub fn new(entry: EntryId, instance: InstanceId, arg: &LocalArg) -> Self {
        let title = if arg.path.is_empty() {
            plx_platform::i18n::msg::browse_local_title().to_owned()
        } else {
            Path::new(&arg.path).file_name().map_or_else(|| arg.path.clone(), |n| n.to_string_lossy().into_owned())
        };
        let shown = if arg.path.is_empty() {
            plx_data::local::roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join("  ·  ")
        } else {
            trail(&arg.path, plx_data::local::roots())
        };
        let mut page = Page {
            entry,
            path: arg.path.clone(),
            name: title.clone(),
            entries: Arc::new(Vec::new()),
            listing: Listing::Loading,
            synced: None,
            marks_synced: None,
            layout: Layout::Grid,
            lists: Vec::new(),
            at: HashMap::new(),
            changes: 0,
            keys: HashMap::new(),
            next: FIRST_CARD_ELEM,
            title_c: CString::new(title.replace('\0', "")).unwrap_or_default(),
            path_c: CString::new(shown.replace('\0', "")).unwrap_or_default(),
        };
        let _ = page.sync();
        let stack = Stack::new(entry).home_when_unfocused(true).reveal_on_move(true);
        Self { page, drawn: None, instance, seated: false, stack, ground: ArtGround::new(Treatment::Blurred) }
    }

    fn view<H: ContentLike>(&self) -> impl Focusable<H> + '_ {
        self.stack.view(&self.page)
    }

    /// The video the engine's `focus` is on, for the item menu a hold opens: its path, its size
    /// (the identity its watch record is kept under) and its mark, as the card shows it.
    pub fn focused_video(&self, focus: Option<FocusKey<u32>>) -> Option<(PathBuf, u64, PosterMark)> {
        let key = focus.filter(|k| k.entry == self.page.entry)?;
        let (entry, mark) = self.page.card(key.elem)?;
        (entry.kind == EntryKind::Video).then(|| (entry.path.clone(), entry.size, mark_of(mark)))
    }

    fn activate<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let Some((entry, _)) = self.page.card(elem) else { return };
        let path = entry.path.to_string_lossy().into_owned();
        let req = match entry.kind {
            EntryKind::Folder => ContentReq::LocalOpen(LocalArg { path }),
            EntryKind::Video => ContentReq::LocalDetail(LocalArg { path }),
        };
        fx.push(plx_machine::machine::Fx::App(AppFx::Content(req)));
        fx.invalidate(Provenance::Input);
    }

    /// Key the ground to the focused card's backdrop (its poster when it has none).
    fn key_ground(&mut self, focus: Option<FocusKey<u32>>) {
        let art = focus.filter(|k| k.entry == self.page.entry)
            .and_then(|k| self.page.card(k.elem))
            .and_then(|(e, _)| e.backdrop.clone().or_else(|| e.art.clone()));
        self.ground.key(plx_data::local::ART_SRV, art.as_deref());
    }

    /// Pin the focused shelf under the preview — the page moves a whole shelf at a time.
    fn pin<H: ContentLike>(&mut self, cx: &Cx<'_, H>) {
        if self.page.layout != Layout::Shelves {
            return;
        }
        let focus = cx.focus.current.filter(|k| k.entry == self.page.entry);
        if let Some(&(n, _)) = focus.and_then(|k| self.page.at.get(&k.elem)) {
            self.stack.scroll_to(pinned_scroll(&self.page.lists, n));
        }
    }
}

impl<H: ContentLike> Machine<H> for LocalScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        if let ScreenEvent::Tick(_) = ev {
            // The boot root is mounted without an `Enter`, so a page still waiting asks on its
            // ticks too; `local::request` is a no-op while a scan is in flight or fresh.
            if matches!(self.page.listing, Listing::Loading) {
                request_scan(&self.page.path);
            }
            for dir in self.page.pending_subfolders() {
                plx_data::local::request(&dir);
            }
            let _ = self.page.sync();
            if self.drawn != Some(self.page.changes) {
                fx.invalidate(Provenance::Landing(fx.from()));
            }
            if !self.seated && self.page.ready() {
                self.seated = true;
                let ours = cx.focus.current.is_some_and(|key| key.entry == self.page.entry);
                let first = self.page.lists.first().and_then(|l| l.elems.first()).copied();
                if let (false, Some(first)) = (ours, first) {
                    fx.push(Fx::Deliver(MachineId::Instance(self.instance), Delivery::Screen(ScreenEvent::Enter(
                        Enter::Fresh { focus: FocusTarget::Elem(FocusKey { entry: self.page.entry, elem: first }) }))));
                }
            }
        }
        let card = match self.stack.on(&self.page, ev, cx, fx) {
            Some(StackEvent::Card(_, card)) => Some(card),
            _ => None,
        };
        match ev {
            ScreenEvent::Enter(_) | ScreenEvent::Uncover => {
                request_scan(&self.page.path);
                self.pin(cx);
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { .. } => {
                self.pin(cx);
                Handled::No
            }
            ScreenEvent::Activate(elem) if *elem == RETRY_ELEM => {
                request_scan(&self.page.path);
                Handled::Yes
            }
            ScreenEvent::PressCommit(_) => {
                if let Some(CardEvent::Activate(elem)) = card {
                    self.activate(elem, fx);
                }
                Handled::Yes
            }
            ScreenEvent::PressHold(_) => {
                if matches!(card, Some(CardEvent::Hold(_))) {
                    fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::ItemMenu)));
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            // The library's top level is the app's root: BACK there is the container's to answer.
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. })
                if !self.page.path.is_empty() => {
                fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::Back)));
                Handled::Yes
            }
            ScreenEvent::Tick(t) => {
                self.key_ground(cx.focus.current);
                self.ground.tick(t.dt());
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Screen<H> for LocalScreen {
    fn redraw_focused(&self, f: &mut DrawFrame<'_, '_, H>, focus: Option<plx_machine::machine::FocusKey<u32>>) {
        self.stack.view(&self.page).redraw_focused(f, focus);
    }
    fn name(&self) -> &'static str { super::registry::word::LOCAL }
    fn state(&self) -> &dyn LogicalState { self }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<std::borrow::Cow<'_, str>> { None }
    fn prepare(&mut self, _budget: &mut plx_ui::frame::Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        self.drawn = Some(self.page.changes);
        let p = f.painter.alpha(f.page_alpha);
        self.ground.draw(p);
        if self.page.layout == Layout::Shelves && self.page.ready() {
            let focused = f.focus.current.filter(|k| k.entry == self.page.entry).map(|k| k.elem);
            self.page.draw_preview(p, focused, f.measure);
            let cut = f.painter;
            let _clip = f.clip(cut, Rect::new(0.0, SHELF_CUT, SCR_W, SCR_H - SHELF_CUT));
            self.stack.view(&self.page).paint(f);
        } else {
            self.stack.view(&self.page).paint(f);
        }
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

    fn entry(title: &str, kind: EntryKind, size: u64) -> Entry {
        Entry::new(title.into(), PathBuf::from(format!("/m/{title}.mkv")), kind, None, size)
    }

    fn film(title: &str, set: &str) -> Entry {
        let mut e = Entry::new(title.into(), PathBuf::from(format!("/m/{title}/{title}.mkv")), EntryKind::Video, None, 1);
        e.meta = Some(Arc::new(Meta { collection: set.into(), ..Meta::default() }));
        e
    }

    fn folder(title: &str, show: bool) -> Entry {
        let mut e = Entry::new(title.into(), PathBuf::from(format!("/m/{title}")), EntryKind::Folder, None, 0);
        e.show = show;
        e
    }

    #[test]
    fn a_record_reads_as_the_mark_its_menu_offers() {
        let p = |offset_ms, watched| Some(Progress { offset_ms, duration_ms: 100_000, watched, size: 1 });
        assert_eq!(mark_of(None), PosterMark::None);
        assert_eq!(mark_of(p(0, false)), PosterMark::None);
        assert_eq!(mark_of(p(40_000, false)), PosterMark::InProgress);
        assert_eq!(mark_of(p(40_000, true)), PosterMark::Watched, "finished wins over a kept place");
    }

    #[test]
    fn a_folder_is_named_by_its_walk_from_the_library_root() {
        let one = [PathBuf::from("/srv/media")];
        assert_eq!(trail("/srv/media/Movies/Alien (1979)", &one), "Movies › Alien (1979)");
        let two = [PathBuf::from("/srv/films"), PathBuf::from("/srv/tv")];
        assert_eq!(trail("/srv/tv/Show A/Season 1", &two), "tv › Show A › Season 1");
        assert_eq!(trail("/elsewhere/x", &one), "/elsewhere/x");
    }

    #[test]
    fn a_video_caption_names_its_container_and_size_and_a_folder_says_folder() {
        assert_eq!(caption(&entry("Film", EntryKind::Video, 1_500_000_000)), format!("MKV · {}", plx_ui::fmt::bytes(1_500_000_000, (1, 0, 0))));
        assert_eq!(caption(&entry("Shows", EntryKind::Folder, 0)), plx_platform::i18n::msg::browse_local_folder());
    }

    #[test]
    fn a_described_title_reads_as_its_facts() {
        let genres = ["Animation".to_owned(), "Fantasy".to_owned(), "Short".to_owned(), "Family".to_owned()];
        assert_eq!(facts(Some(2019), Some(8), &genres), format!("2019 · {} · Animation, Fantasy, Short", plx_ui::fmt::dur_long(8 * 60_000)));
        assert_eq!(facts(None, None, &[]), "");
        let mut e = entry("Spring", EntryKind::Video, 1);
        e.meta = Some(Arc::new(Meta { year: Some(2019), ..Meta::default() }));
        assert_eq!(caption(&e), "2019", "a card's caption is its year and runtime, not its file");
    }

    #[test]
    fn a_folder_of_ungrouped_titles_is_a_grid() {
        let flat = [entry("a", EntryKind::Video, 1), folder("Caminandes", true)];
        assert_eq!(groups(&flat, "TV Shows", |_| None), None, "videos and shows alone are a grid");
    }

    #[test]
    fn collections_then_subfolders_then_the_rest_make_the_shelves() {
        let listing = vec![
            film("Charge", "Blender Open Movies"),
            film("Night of the Living Dead", ""),
            folder("Silent Classics", false),
            film("Spring", "Blender Open Movies"),
            folder("Still Loading", false),
            folder("Caminandes", true),
        ];
        let silent = Arc::new(vec![film("Nosferatu", "")]);
        let sub = |p: &Path| (p == Path::new("/m/Silent Classics")).then(|| Arc::clone(&silent));
        let g = groups(&listing, "Films", sub).unwrap();
        let shape: Vec<(&str, Vec<&str>)> = g.iter().map(|g| (g.heading.as_str(), g.entries.iter().map(|e| e.title.as_str()).collect())).collect();
        assert_eq!(shape, [
            ("Blender Open Movies", vec!["Charge", "Spring"]),
            ("Silent Classics", vec!["Nosferatu"]),
            ("Films", vec!["Night of the Living Dead", "Caminandes"]),
        ], "a subfolder still being read has no shelf yet; a show is a card, not a shelf");
        assert!(g.iter().all(|g| !g.stills));
    }

    #[test]
    fn a_seasons_episodes_are_a_shelf_of_stills() {
        let show = [folder("Season 01", false)];
        let eps = Arc::new(vec![entry("S01E01", EntryKind::Video, 1), entry("S01E02", EntryKind::Video, 1)]);
        let g = groups(&show, "Caminandes", |_| Some(Arc::clone(&eps))).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g[0].stills);
        assert_eq!(g[0].heading, "Season 01");
    }

    #[test]
    fn the_focused_shelf_is_pinned_a_whole_shelf_at_a_time() {
        let list = |style: &'static RowStyle| List { heading: String::new(), style, entries: Vec::new(), elems: Vec::new(), index: HashMap::new(), marks: Vec::new() };
        let lists = [list(&RowStyle::SHELF), list(&RowStyle::STILL_SHELF), list(&RowStyle::SHELF)];
        assert_eq!(pinned_scroll(&lists, 0), 0.0, "the first shelf rests where the page opens");
        let poster = SHELF_HEADING + RowStyle::SHELF.h + cards::under_band(0.0);
        let still = SHELF_HEADING + RowStyle::STILL_SHELF.h + cards::under_band(0.0);
        assert_eq!(pinned_scroll(&lists, 1), poster);
        assert_eq!(pinned_scroll(&lists, 2), poster + still);
    }
}
