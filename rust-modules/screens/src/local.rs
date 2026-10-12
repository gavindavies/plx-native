//! **The local library page: one folder of the machine's own disk, as a grid of cards.**
//!
//! The Linux build's home and its folder browser (`plx_data::local` is the catalog). A page shows
//! ONE folder — the library's top level when its argument's path is empty — and opening a folder
//! card pushes another page for it, so BACK walks up the tree the way it walks back from any page.
//! OK on a video asks the loop to play it (`ContentReq::LocalPlay`): probing the file and starting
//! the engine need the playback session, which a screen may not name.
//!
//! The page is a `plx_ui::cards::Stack` of three sections, `collection.rs`'s shape: the head
//! (the folder's name and where it is), the card grid, and the status read-out (reading, empty,
//! unreadable). A listing lands whole — there is no paging — so the grid's content is the listing
//! itself, keyed by path ([`Page::keys`]) so a file added under the cursor never moves focus to
//! its neighbour.
//!
//! **What it reads is process state, not a store view.** `plx_data::local` is the one catalog in
//! the app that is not a `Store` machine: it has no server, no account and nothing to record, and
//! the Linux build does not run the recorder. The page re-reads it when `local::revision` moves.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plx_data::local::{Entry, EntryKind, Listing};
use plx_machine::machine::{Canon, Cx, Delivery, Edge, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx,
    GroupId, Handled, Host, InputEvent, InputKind, InstanceId, Key, LogicalState, Machine, MachineId, Measure};
use plx_machine::present::Provenance;
use plx_ui::cards::{CardEvent, CardSource, GridSpec, Kind, SectionSpec, Stack, StackEvent, StackPage, Tile, TileLabel};
use plx_ui::consts::*;
use plx_ui::label::{Label, VAlign};
use plx_ui::screen::{AxisMask, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, RenderStrategy, Screen, ScreenEvent, Seat};
use plx_ui::theme;
use plx_ui::widgets::{self, Art, PageGround, StatusKind, StatusOverlay};
use plx_ui::{Env, Painter, Rect};

use super::registry::{AppFx, ContentLike, ContentReq, LocalArg};

const RETRY_ELEM: u32 = 1;
const FIRST_CARD_ELEM: u32 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sec {
    Head,
    Items,
    Status,
}
const GRID_GROUP: GroupId = GroupId(0);
const HEADER_GROUP: GroupId = GroupId(1);
const STATUS_GROUP: GroupId = GroupId(2);

const CONTENT_TOP: f32 = MARGIN_X;
const HEADER_TOP: f32 = CONTENT_TOP;
/// The path line's cap top under the DISPLAY title's — the collection header's meta offset.
const META_DY: f32 = 65.0;
/// The first row's poster top: the header block and one XL gap.
const GRID_TOP: f32 = 236.0;
const STATUS_FRAME: Rect = Rect { x: MARGIN_X, y: 300.0, w: SCR_W - 2.0 * MARGIN_X, h: 475.0 };
/// The poster box local artwork is decoded into (`widgets::POSTER_RES`, the catalog's own).
const ART_RES: (std::os::raw::c_int, std::os::raw::c_int) = widgets::POSTER_RES;
/// The glyph a card with no artwork wears, centred, as a share of the card's width.
const GLYPH_SHARE: f32 = 0.34;

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
        EntryKind::Folder => plx_platform::i18n::msg::browse_local_folder().to_owned(),
        EntryKind::Video => {
            let ext = entry.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
            let size = plx_ui::fmt::bytes(i64::try_from(entry.size).unwrap_or(i64::MAX), (1, 0, 0));
            if ext.is_empty() { size } else { format!("{ext} · {size}") }
        }
    }
}

/// The grid's content: the listing, read through the page's path-keyed elements.
struct Items<'a> {
    entries: &'a [Entry],
    elems: &'a [u32],
    index: &'a HashMap<u32, usize>,
}

impl<H: Host<Elem = u32>> CardSource<H> for Items<'_> {
    fn len(&self) -> usize { self.entries.len() }
    fn elem(&self, i: usize) -> u32 { self.elems.get(i).copied().unwrap_or(RETRY_ELEM) }
    fn index_of(&self, e: &u32) -> Option<usize> { self.index.get(e).copied() }
    fn art(&self, i: usize) -> Art<'_> {
        match self.entries.get(i).and_then(|e| e.art.as_deref()) {
            Some(token) => Art::Thumb { sid: plx_data::local::ART_SRV, key: token, res: ART_RES },
            None => Art::Thumb { sid: plx_data::local::ART_SRV, key: "", res: ART_RES },
        }
    }
    fn label(&self, i: usize) -> TileLabel {
        self.entries.get(i).map_or_else(TileLabel::default, |e| TileLabel::titled(&e.title, &caption(e)))
    }
    fn overlay(&self, p: Painter, i: usize, tile: &Tile, measure: &dyn Measure) {
        let Some(entry) = self.entries.get(i) else { return };
        if entry.art.is_none() {
            let side = tile.rect.w * GLYPH_SHARE;
            let glyph = Rect::new(tile.rect.x + (tile.rect.w - side) / 2.0, tile.rect.y + (tile.rect.h - side) / 2.0 - side * 0.25, side, side);
            let icon = match entry.kind {
                EntryKind::Folder => plx_ui::icons::Icon::Collection,
                EntryKind::Video => plx_ui::icons::Icon::Play,
            };
            plx_ui::icons::draw(p, icon, glyph, theme::TEXT_SECONDARY);
        }
        // Local files rarely carry artwork, so every card names itself on the art.
        widgets::poster_label(p, tile.rect, tile.radius, &entry.title, measure);
    }
}

struct Page {
    entry: EntryId,
    /// The folder shown; empty for the library's top level.
    path: String,
    /// The listing as last read, and the catalog revision it was read at.
    entries: Arc<Vec<Entry>>,
    listing: Listing,
    synced: Option<u64>,
    /// One element per path, minted once and never reused for another path on this page.
    keys: HashMap<PathBuf, u32>,
    next: u32,
    /// `elems[i]` is card `i`'s element; `index` the inverse. Derived on each sync.
    elems: Vec<u32>,
    index: HashMap<u32, usize>,
    title_c: CString,
    path_c: CString,
}

impl Page {
    /// Re-read the listing if the catalog moved. `true` when it did: the page has new ground to
    /// present, and nothing else will ask for a frame on its behalf.
    fn sync(&mut self) -> bool {
        let revision = plx_data::local::revision();
        if self.synced == Some(revision) {
            return false;
        }
        self.synced = Some(revision);
        self.listing = listing_of(&self.path);
        if let Listing::Ready(entries) = &self.listing {
            self.entries = Arc::clone(entries);
        }
        self.elems.clear();
        self.index.clear();
        for (i, e) in self.entries.iter().enumerate() {
            let elem = *self.keys.entry(e.path.clone()).or_insert_with(|| {
                let n = self.next;
                self.next += 1;
                n
            });
            self.elems.push(elem);
            self.index.insert(elem, i);
        }
        true
    }

    fn items(&self) -> Items<'_> {
        Items { entries: &self.entries, elems: &self.elems, index: &self.index }
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
}

impl<H: ContentLike> StackPage<H> for Page {
    type Key = Sec;
    type Cards<'a> = Items<'a>;

    fn revision(&self, _cx: &Cx<'_, H>) -> u64 {
        let status = match self.listing {
            Listing::Loading => 1,
            Listing::Ready(_) => 2,
            Listing::Failed(_) => 3,
        };
        self.synced.unwrap_or(0) << 2 | status
    }

    fn sections(&self, cx: &Cx<'_, H>, out: &mut Vec<SectionSpec<Sec>>) {
        out.push(SectionSpec::new(Sec::Head, Kind::Custom { height: GRID_TOP, focusable: false }, HEADER_GROUP).ranked(1));
        out.push(SectionSpec::new(Sec::Items, Kind::Grid { spec: GridSpec::new(GRID_TOP, CONTENT_TOP) }, GRID_GROUP));
        let retry = if matches!(self.listing, Listing::Failed(_)) {
            self.status_overlay(cx.tick.ms).action_frame_measured(cx.measure)
        } else {
            None
        };
        out.push(SectionSpec::new(Sec::Status, Kind::Overlay { rect: retry.unwrap_or(STATUS_FRAME), focusable: retry.is_some() }, STATUS_GROUP).ranked(2));
    }

    fn fallback(&self, _cx: &Cx<'_, H>, out: &mut Vec<Sec>) {
        out.extend([Sec::Items, Sec::Status]);
    }

    fn cards<'a>(&'a self, _cx: &'a Cx<'_, H>, k: Sec) -> Option<Items<'a>> {
        (k == Sec::Items && self.ready()).then(|| self.items())
    }

    fn elem_of(&self, k: Sec) -> Option<u32> {
        (k == Sec::Status).then_some(RETRY_ELEM)
    }

    fn plain_group(&self, _cx: &Cx<'_, H>, _k: Sec, id: GroupId, extent: Rect) -> GroupSpec {
        GroupSpec { id, kind: GroupKind::Free, seat: Seat::First, reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop; 4], extent, len: 1, elem: ElemKind::Bare }
    }

    fn custom_draw(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, _r: Rect, scroll: f32) {
        let p = f.painter.alpha(f.page_alpha);
        match k {
            Sec::Head => self.draw_header(p, scroll),
            Sec::Status => if !self.ready() {
                let on = f.focus.current.is_some_and(|key| key.entry == self.entry && key.elem == RETRY_ELEM);
                self.status_overlay(f.cx.tick.ms).focused(on).draw_measured(&Env::inert(), p, f.measure);
            },
            Sec::Items => {}
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
    ground: PageGround,
}

plx_ui::focusable_via_view!(LocalScreen, H: [ContentLike], view);

impl LogicalState for LocalScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.page.entry.0).str(&self.page.path).u32(self.page.next);
        self.stack.write(c);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("local path={} cards={}", self.page.path, self.page.entries.len()));
    }
}

impl LocalScreen {
    pub const SHAPE: &'static str = "LocalScreen{entry:u32,path:String,next_elem:u32,stack:Stack{scroll:{pos:f32,vel:f32},target:f32,sections:[Grid{bands,pop}]}}";

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
            entries: Arc::new(Vec::new()),
            listing: Listing::Loading,
            synced: None,
            keys: HashMap::new(),
            next: FIRST_CARD_ELEM,
            elems: Vec::new(),
            index: HashMap::new(),
            title_c: CString::new(title.replace('\0', "")).unwrap_or_default(),
            path_c: CString::new(shown.replace('\0', "")).unwrap_or_default(),
        };
        let _ = page.sync();
        Self { page, drawn: None, instance, seated: false, stack: Stack::new(entry).home_when_unfocused(true), ground: PageGround::new() }
    }

    fn view<H: ContentLike>(&self) -> impl Focusable<H> + '_ {
        self.stack.view(&self.page)
    }

    fn entry_at(&self, elem: u32) -> Option<&Entry> {
        self.page.index.get(&elem).and_then(|&i| self.page.entries.get(i))
    }

    fn activate<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let Some(entry) = self.entry_at(elem) else { return };
        let path = entry.path.to_string_lossy().into_owned();
        let req = match entry.kind {
            EntryKind::Folder => ContentReq::LocalOpen(LocalArg { path }),
            EntryKind::Video => ContentReq::LocalPlay(path),
        };
        fx.push(plx_machine::machine::Fx::App(AppFx::Content(req)));
        fx.invalidate(Provenance::Input);
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
            let _ = self.page.sync();
            if self.drawn != self.page.synced {
                fx.invalidate(Provenance::Landing(fx.from()));
            }
            if !self.seated && self.page.ready() {
                self.seated = true;
                let ours = cx.focus.current.is_some_and(|key| key.entry == self.page.entry);
                if let (false, Some(&first)) = (ours, self.page.elems.first()) {
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
                fx.invalidate(Provenance::Nav);
                Handled::Yes
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
            // The library's top level is the app's root: BACK there is the container's to answer.
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. })
                if !self.page.path.is_empty() => {
                fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::Back)));
                Handled::Yes
            }
            ScreenEvent::Tick(t) => {
                let target = PageGround::page_target(None);
                self.ground.key_target(target, t.dt());
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
        self.drawn = self.page.synced;
        let p = f.painter.alpha(f.page_alpha);
        self.ground.draw(p, Rect::FULL);
        self.stack.view(&self.page).paint(f);
    }
    fn render(&self) -> RenderStrategy { RenderStrategy::Page }
    fn focus_source(&self) -> FocusSource { FocusSource::Engine }
    fn hit_source(&self) -> HitSource { HitSource::Engine }
    fn as_any(&self) -> Option<&dyn std::any::Any> { Some(self) }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> { Some(self) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, kind: EntryKind, size: u64) -> Entry {
        Entry { title: title.into(), path: PathBuf::from(format!("/m/{title}.mkv")), kind, art: None, size }
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
}
