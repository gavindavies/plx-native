//! **Home** (`PiTV Redesign.dc.html`, 1a): the app's root and the tab bar's first pill.
//!
//! The page stands on the newest title's backdrop, full-bleed: an eyebrow (*RECENTLY ADDED*), the
//! title's logo (or its name), one line of facts, its synopsis, and two controls — **Play** (or
//! **Resume m:ss**) and **More Info**, which opens its title page. Under the hero peek two shelves:
//!
//! - **Continue Watching** — every unfinished place (`local_progress::continue_watching`), most
//!   recently played first, as landscape stills with their resume bar; the caption says how long
//!   is left. A card NAVIGATES to its title page (it carries no ▶, `ui/src/CLAUDE.md`'s card marks).
//! - **Recently Added** — the library's titles (`tabs::libraries`' folders), newest first.
//!
//! What it shows is the catalog's own entries, never a second read of the disk: a library folder,
//! and a continued file's folder and its folder's folder, are requested off the frame on mount
//! (`plx_data::local::request`) and the page fills in as their listings land.
//!
//! The tab bar is the container's (`tabs`): a pill's press reaches this page as an `Activate` of a
//! strip element and goes on as `ContentReq::SelectTab`; DOWN from the bar lands on the hero's
//! controls ([`Screen::links`]).

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};

use plx_data::local::{Entry, EntryKind, Listing};
use plx_data::local_progress::{self, Progress};
use plx_machine::machine::{Canon, Cx, Delivery, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx, GroupId, Handled,
    Host, InstanceId, LogicalState, Machine, MachineId, Measure};
use plx_machine::present::Provenance;
use plx_ui::art_ground::{ArtGround, Treatment};
use plx_ui::cards::{self, CardEvent, CardSource, Kind, RowStyle, SectionSpec, Stack, StackEvent, StackPage, TileLabel};
use plx_ui::consts::*;
use plx_ui::hero_logo::{HeroLogo, LogoRung};
use plx_ui::icons::Icon;
use plx_ui::label::{Label, VAlign};
use plx_ui::screen::{At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, Link, RenderStrategy, Screen, ScreenEvent, Seat};
use plx_ui::theme;
use plx_ui::widgets::{self, Art, Button, CtlPop};
use plx_ui::Rect;

use super::registry::{AppFx, ContentLike, ContentReq, LocalArg};
use super::titles::{facts, file_facts};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sec {
    Hero,
    Continue,
    Recent,
}
const HERO_GROUP: GroupId = GroupId(0);
const CONTINUE_GROUP: GroupId = GroupId(1);
const RECENT_GROUP: GroupId = GroupId(2);

const PLAY_ELEM: u32 = 1;
const INFO_ELEM: u32 = 2;
const CTRLS: [u32; 2] = [PLAY_ELEM, INFO_ELEM];
/// Continue Watching's cards and Recently Added's are keyed apart: one film can be in both.
const CONTINUE_ELEM0: u32 = 0x1000;
const RECENT_ELEM0: u32 = 0x8000;

/// The hero column: the eyebrow's cap top, the logo band's foot, the facts line's cap top, and
/// the column's width (the mock's 760).
const EYEBROW_TOP: f32 = 236.0;
const LOGO_BOTTOM: f32 = 412.0;
const META_TOP: f32 = 440.0;
const COL_W: f32 = 760.0;
const META_TO_SYN: f32 = 50.0;
const SYN_TO_CTRL: f32 = 40.0;
/// The controls: the title page's 76 px row, and the air between them.
const CTRL_H: f32 = 76.0;
const CTRL_GAP: f32 = 32.0;
/// Where the first shelf's heading rests when the page opens: low enough that only the heading and
/// the tops of its cards peek under the hero (the mock's 846).
const SHELVES_TOP: f32 = 846.0;
const SHELF_HEADING: f32 = 62.0;
const SHELF_FOOT: f32 = 30.0;
/// The most cards each shelf offers.
const SHELF_MAX: usize = 20;
/// Decode boxes: a landscape still (`STILL_SHELF`, popped) and a poster.
const STILL_RES: (std::os::raw::c_int, std::os::raw::c_int) = (480, 270);
const POSTER_RES: (std::os::raw::c_int, std::os::raw::c_int) = widgets::POSTER_RES;

/// The listing a library pill's folder shows (its path argument, `""` for the single root's own).
fn library_listing(path: &str) -> Listing {
    if path.is_empty() {
        return plx_data::local::roots().first().map_or(Listing::Failed(String::new()), |r| plx_data::local::listing(r));
    }
    plx_data::local::listing(Path::new(path))
}

fn library_dir(path: &str) -> Option<PathBuf> {
    if path.is_empty() { plx_data::local::roots().first().cloned() } else { Some(PathBuf::from(path)) }
}

/// The folders a file may be listed in: its own folder's folder first (a film in its own folder
/// is listed there, among the other films), then its own folder.
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

/// **Recently Added**: the titles of the libraries' listings — a video, or a show — newest first.
fn recently_added(listings: &[std::sync::Arc<Vec<Entry>>]) -> Vec<Entry> {
    let mut all: Vec<Entry> = listings.iter()
        .flat_map(|l| l.iter())
        .filter(|e| e.kind == EntryKind::Video || e.show)
        .cloned()
        .collect();
    all.sort_by(|a, b| b.added.cmp(&a.added).then_with(|| plx_data::local::natural_cmp(a.display_title(), b.display_title())));
    all.truncate(SHELF_MAX);
    all
}

/// The hero: the newest title that has a backdrop to stand on, else the newest title.
fn hero_of(recent: &[Entry]) -> Option<Entry> {
    recent.iter().take(5).find(|e| e.backdrop.is_some()).or_else(|| recent.first()).cloned()
}

/// Continue Watching: each unfinished file, found in the listing of one of its [`homes`].
fn continued(places: &[(PathBuf, Progress)], listing: impl Fn(&Path) -> Option<std::sync::Arc<Vec<Entry>>>) -> Vec<(Entry, Progress)> {
    places.iter().filter_map(|(path, p)| {
        let entry = homes(path).iter().find_map(|h| listing(h)?.iter().find(|e| &e.path == path).cloned())?;
        Some((entry, *p))
    }).take(SHELF_MAX).collect()
}

/// One of Home's shelves, keyed by path for the engine.
struct Row {
    entries: Vec<Entry>,
    /// Continue Watching's places, beside its entries; empty for Recently Added.
    places: Vec<Progress>,
    elems: Vec<u32>,
    index: HashMap<u32, usize>,
    keys: HashMap<PathBuf, u32>,
    next: u32,
}

impl Row {
    fn new(first: u32) -> Self {
        Self { entries: Vec::new(), places: Vec::new(), elems: Vec::new(), index: HashMap::new(), keys: HashMap::new(), next: first }
    }

    fn set(&mut self, entries: Vec<Entry>, places: Vec<Progress>) {
        self.entries = entries;
        self.places = places;
        self.elems.clear();
        self.index.clear();
        for (i, e) in self.entries.iter().enumerate() {
            let elem = *self.keys.entry(e.path.clone()).or_insert_with(|| {
                let k = self.next;
                self.next += 1;
                k
            });
            self.elems.push(elem);
            self.index.insert(elem, i);
        }
    }

    fn at(&self, elem: u32) -> Option<&Entry> {
        self.index.get(&elem).and_then(|&i| self.entries.get(i))
    }
}

/// A shelf's cards: Continue Watching's landscape stills, or Recently Added's posters.
struct Cards<'a> {
    row: &'a Row,
    stills: bool,
}

impl<H: Host<Elem = u32>> CardSource<H> for Cards<'_> {
    fn len(&self) -> usize { self.row.entries.len() }
    fn elem(&self, i: usize) -> u32 { self.row.elems.get(i).copied().unwrap_or(0) }
    fn index_of(&self, e: &u32) -> Option<usize> { self.row.index.get(e).copied() }
    fn art(&self, i: usize) -> Art<'_> {
        let e = self.row.entries.get(i);
        let (key, res) = if self.stills {
            (e.and_then(|e| e.backdrop.as_deref().or(e.art.as_deref())), STILL_RES)
        } else {
            (e.and_then(|e| e.art.as_deref()), POSTER_RES)
        };
        Art::Thumb { sid: plx_data::local::ART_SRV, key: key.unwrap_or(""), res }
    }
    fn label(&self, i: usize) -> TileLabel {
        let Some(e) = self.row.entries.get(i) else { return TileLabel::default() };
        let line = match self.row.places.get(i) {
            Some(p) => plx_ui::fmt::time_left(p.duration_ms.saturating_sub(p.offset_ms) as i64),
            None => e.meta.as_deref().map_or_else(|| file_facts(e), |m| facts(m.year, m.runtime_min, &[])),
        };
        TileLabel::titled(e.display_title(), &line)
    }
    fn progress(&self, i: usize) -> Option<f32> {
        self.row.places.get(i).and_then(local_progress::fraction)
    }
}

struct Page {
    entry: EntryId,
    hero: Option<Entry>,
    /// The hero's watch record, read when the page last synced.
    hero_mark: Option<Progress>,
    cont: Row,
    recent: Row,
    synced: Option<(u64, u64)>,
    changes: u64,
}

impl Page {
    /// Re-read what the page shows if the catalog or the watch records moved.
    fn sync(&mut self) -> bool {
        let now = (plx_data::local::revision(), local_progress::revision());
        if self.synced == Some(now) {
            return false;
        }
        self.synced = Some(now);
        let listings: Vec<_> = super::tabs::libraries().iter()
            .filter_map(|(_, path)| match library_listing(path) {
                Listing::Ready(l) => Some(l),
                _ => None,
            })
            .collect();
        let recent = recently_added(&listings);
        self.hero = hero_of(&recent);
        self.hero_mark = self.hero.as_ref().and_then(|h| local_progress::get(&h.path, h.size));
        let places = local_progress::continue_watching();
        let found = continued(&places, |dir| match plx_data::local::listing(dir) {
            Listing::Ready(l) => Some(l),
            _ => None,
        });
        let (entries, marks): (Vec<Entry>, Vec<Progress>) = found.into_iter().unzip();
        self.cont.set(entries, marks);
        self.recent.set(recent, Vec::new());
        self.changes += 1;
        true
    }

    /// Ask for every listing the page reads (a no-op for one in flight or fresh).
    fn request(&self) {
        for (_, path) in super::tabs::libraries() {
            if let Some(dir) = library_dir(&path) {
                plx_data::local::request(&dir);
            }
        }
        for (path, _) in local_progress::continue_watching().iter().take(SHELF_MAX) {
            for home in homes(path) {
                plx_data::local::request(&home);
            }
        }
    }

    fn resuming(&self) -> Option<Progress> {
        self.hero_mark.filter(|m| !m.watched && m.offset_ms > 0)
    }

    fn play_label(&self) -> String {
        match self.resuming() {
            Some(m) => plx_platform::i18n::msg::browse_local_resume_time(&plx_ui::fmt::clock(m.offset_ms as i64)),
            None => plx_platform::i18n::msg::browse_detail_play().to_owned(),
        }
    }

    fn synopsis(&self) -> &str {
        self.hero.as_ref().and_then(|h| h.meta.as_deref()).map_or("", |m| m.plot.as_str())
    }

    /// The controls' top, below the synopsis.
    fn ctrl_top(&self) -> f32 {
        let syn = self.synopsis();
        let syn_h = if syn.is_empty() { 0.0 } else { plx_ui::hero_synopsis(syn, "").measure_h(COL_W) };
        META_TOP + META_TO_SYN + syn_h + SYN_TO_CTRL
    }

    /// The controls' rects (document space): Play, then More Info.
    fn ctrl_rects(&self, measure: &dyn Measure) -> [Rect; 2] {
        let top = self.ctrl_top();
        let play = CString::new(self.play_label()).unwrap_or_default();
        let info = CString::new(plx_platform::i18n::msg::browse_local_more_info()).unwrap_or_default();
        let pw = Button::pill_w_measured(&play, theme::size::BODY, true, false, measure);
        let iw = Button::pill_w_measured(&info, theme::size::BODY, false, false, measure);
        [Rect::new(MARGIN_X, top, pw, CTRL_H), Rect::new(MARGIN_X + pw + CTRL_GAP, top, iw, CTRL_H)]
    }

    fn hero_h(&self) -> f32 {
        if self.hero.is_none() { 0.0 } else { (self.ctrl_top() + CTRL_H + 60.0).max(SHELVES_TOP) }
    }

    fn row(&self, k: Sec) -> Option<&Row> {
        match k {
            Sec::Continue => Some(&self.cont),
            Sec::Recent => Some(&self.recent),
            Sec::Hero => None,
        }
    }

    fn card(&self, elem: u32) -> Option<&Entry> {
        self.cont.at(elem).or_else(|| self.recent.at(elem))
    }
}

impl<H: ContentLike> StackPage<H> for Page {
    type Key = Sec;
    type Cards<'a> = Cards<'a>;

    fn revision(&self, _cx: &Cx<'_, H>) -> u64 {
        self.changes
    }

    fn sections(&self, _cx: &Cx<'_, H>, out: &mut Vec<SectionSpec<Sec>>) {
        out.push(SectionSpec::new(Sec::Hero, Kind::Custom { height: self.hero_h(), focusable: self.hero.is_some() }, HERO_GROUP));
        out.push(SectionSpec::new(Sec::Continue, Kind::Shelf { style: &RowStyle::STILL_SHELF, heading: SHELF_HEADING }, CONTINUE_GROUP));
        out.push(SectionSpec::new(Sec::Recent, Kind::Shelf { style: &RowStyle::SHELF, heading: SHELF_HEADING }, RECENT_GROUP));
    }

    fn fallback(&self, _cx: &Cx<'_, H>, out: &mut Vec<Sec>) {
        out.extend([Sec::Hero, Sec::Continue, Sec::Recent]);
    }

    fn cards<'a>(&'a self, _cx: &'a Cx<'_, H>, k: Sec) -> Option<Cards<'a>> {
        let row = self.row(k).filter(|r| !r.entries.is_empty())?;
        Some(Cards { row, stills: k == Sec::Continue })
    }

    fn plain_len(&self, k: Sec) -> usize {
        if k == Sec::Hero && self.hero.is_some() { CTRLS.len() } else { 0 }
    }

    fn plain_elem(&self, k: Sec, n: usize) -> Option<u32> {
        (k == Sec::Hero).then(|| CTRLS.get(n).copied()).flatten()
    }

    fn plain_step(&self, k: Sec, at: usize, dir: Dir) -> Option<usize> {
        match (k, dir) {
            (Sec::Hero, Dir::Left) => at.checked_sub(1),
            (Sec::Hero, Dir::Right) => (at + 1 < CTRLS.len()).then_some(at + 1),
            _ => None,
        }
    }

    fn element_rect(&self, cx: &Cx<'_, H>, _k: Sec, n: usize, section: Rect) -> Rect {
        let r = self.ctrl_rects(cx.measure)[n.min(1)];
        Rect::new(r.x, r.y + section.y, r.w, r.h)
    }

    fn focus_rect(&self, cx: &Cx<'_, H>, _k: Sec, section: Rect) -> Rect {
        let [a, b] = self.ctrl_rects(cx.measure);
        Rect::new(a.x, a.y + section.y, b.x + b.w - a.x, CTRL_H)
    }

    fn plain_group(&self, _cx: &Cx<'_, H>, k: Sec, id: GroupId, extent: Rect) -> GroupSpec {
        GroupSpec { id, kind: GroupKind::Row { wrap: false }, seat: Seat::First, reachable: AxisMask::BOTH,
            edge: [EdgeRule::Geometric, EdgeRule::Geometric, EdgeRule::Stop, EdgeRule::Stop], extent,
            len: if k == Sec::Hero && self.hero.is_some() { CTRLS.len() } else { 0 }, elem: ElemKind::Control }
    }

    fn shelf_foot(&self, _k: Sec) -> f32 {
        SHELF_FOOT
    }

    fn draw_heading(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, r: Rect, lift: f32) {
        let heading = match k {
            Sec::Continue => plx_platform::i18n::msg::browse_home_continue_watching(),
            Sec::Recent => plx_platform::i18n::msg::browse_library_hub_recently_added(),
            Sec::Hero => return,
        };
        let p = f.painter.alpha(f.page_alpha);
        cards::draw_heading(p, heading, "", MARGIN_X, r.y - lift, SCR_W - 2.0 * MARGIN_X, f.measure);
    }
}

pub struct HomeScreen {
    page: Page,
    instance: InstanceId,
    seated: bool,
    drawn: Option<u64>,
    stack: Stack<Sec>,
    ground: ArtGround,
    pop: CtlPop<2>,
    tab_seat: super::tabs::TabSeat,
}

plx_ui::focusable_via_view!(HomeScreen, H: [ContentLike], view);

impl LogicalState for HomeScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.page.entry.0).u32(self.page.cont.next).u32(self.page.recent.next);
        self.stack.write(c);
        self.pop.write_motion(c);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("home hero={} continue={} recent={}", self.page.hero.is_some(),
            self.page.cont.entries.len(), self.page.recent.entries.len()));
    }
}

impl HomeScreen {
    pub const SHAPE: &'static str = "HomeScreen{entry:u32,continue_next:u32,recent_next:u32,stack:Stack{scroll:{pos:f32,vel:f32},target:f32,sections:[Shelf{pop,scroll}]},pop:[{pos:f32,vel:f32}]}";

    pub fn new(entry: EntryId, instance: InstanceId) -> Self {
        let mut page = Page {
            entry,
            hero: None,
            hero_mark: None,
            cont: Row::new(CONTINUE_ELEM0),
            recent: Row::new(RECENT_ELEM0),
            synced: None,
            changes: 0,
        };
        let _ = page.sync();
        Self { page, instance, seated: false, drawn: None, stack: Stack::new(entry).home_when_unfocused(true),
            ground: ArtGround::new(Treatment::Sharp), pop: CtlPop::new(), tab_seat: Default::default() }
    }

    fn view<H: ContentLike>(&self) -> impl Focusable<H> + '_ {
        self.stack.view(&self.page)
    }

    fn focused_ctrl<H: ContentLike>(&self, cx: &Cx<'_, H>) -> Option<usize> {
        let key = cx.focus.current.filter(|k| k.entry == self.page.entry)?;
        CTRLS.iter().position(|&e| e == key.elem)
    }

    fn press<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let req = if elem >= plx_ui::dispatch::STRIP_BASE {
            ContentReq::SelectTab((elem - plx_ui::dispatch::STRIP_BASE) as usize)
        } else {
            let target = match elem {
                PLAY_ELEM | INFO_ELEM => self.page.hero.as_ref(),
                card => self.page.card(card),
            };
            let Some(e) = target else { return };
            let path = e.path.to_string_lossy().into_owned();
            match (elem, e.kind) {
                (PLAY_ELEM, EntryKind::Video) => ContentReq::LocalPlay { path, from_start: false },
                (_, EntryKind::Video) => ContentReq::LocalDetail(LocalArg { path }),
                (_, EntryKind::Folder) => ContentReq::LocalOpen(LocalArg { path }),
            }
        };
        fx.push(Fx::App(AppFx::Content(req)));
        fx.invalidate(Provenance::Input);
    }

    fn draw_hero<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, scroll: f32) {
        let Some(hero) = &self.page.hero else { return };
        let p = f.painter.alpha(f.page_alpha).translate(0.0, -scroll);
        let measure = f.measure;
        let eyebrow = plx_platform::i18n::msg::browse_library_hub_recently_added().to_uppercase();
        if let Ok(c) = CString::new(eyebrow) {
            Label::new(c.as_ptr(), theme::size::CAPTION, theme::TEXT_SECONDARY).bold().v(VAlign::CapTop)
                .draw(p, Rect::new(MARGIN_X, EYEBROW_TOP, COL_W, 0.0));
        }
        let band_h = plx_ui::hero_logo::band_h(LogoRung::Hero);
        let logo = hero.logo.clone().unwrap_or_default();
        HeroLogo::new(plx_data::local::ART_SRV, &logo, hero.display_title(), LogoRung::Hero)
            .draw(p, Rect::new(MARGIN_X, LOGO_BOTTOM - band_h, COL_W, band_h), measure);
        let line = hero.meta.as_deref().map_or_else(|| file_facts(hero), |m| facts(m.year, m.runtime_min, &m.genres));
        widgets::facts_line(p, MARGIN_X, META_TOP, COL_W, &line, None, measure);
        let syn = self.page.synopsis();
        if !syn.is_empty() {
            plx_ui::hero_synopsis(syn, "").draw(p, Rect::new(MARGIN_X, META_TOP + META_TO_SYN, COL_W, 0.0));
        }
        let focused = self.focused_ctrl(f.cx);
        let rects = self.page.ctrl_rects(measure);
        let play = CString::new(self.page.play_label()).unwrap_or_default();
        let info = CString::new(plx_platform::i18n::msg::browse_local_more_info()).unwrap_or_default();
        for (n, &elem) in CTRLS.iter().enumerate() {
            let scale = self.pop.scale_with(n, f.press.dip_of(&FocusKey { entry: self.page.entry, elem }));
            let on = focused == Some(n);
            let b = if elem == PLAY_ELEM {
                Button::new(play.as_ptr(), theme::size::BODY, rects[n]).icon(Icon::Play)
            } else {
                Button::new(info.as_ptr(), theme::size::BODY, rects[n])
            };
            plx_ui::View::draw(&b.focused(on).scale(scale), &plx_ui::Env::inert(), p);
        }
    }
}

impl<H: ContentLike> Machine<H> for HomeScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        if let ScreenEvent::Tick(t) = ev {
            self.page.request();
            let _ = self.page.sync();
            if self.drawn != Some(self.page.changes) {
                fx.invalidate(Provenance::Landing(fx.from()));
            }
            let art = self.page.hero.as_ref().and_then(|h| h.backdrop.clone().or_else(|| h.art.clone()));
            self.ground.key(plx_data::local::ART_SRV, art.as_deref());
            self.ground.tick(t.dt());
            let focused = self.focused_ctrl(cx);
            self.pop.step(focused, t.dt());
            // Home opens on Play, as soon as there is a hero to play.
            if !self.seated && self.page.hero.is_some() {
                self.seated = true;
                if !cx.focus.current.is_some_and(|k| k.entry == self.page.entry) {
                    fx.push(Fx::Deliver(MachineId::Instance(self.instance), Delivery::Screen(ScreenEvent::Enter(
                        Enter::Fresh { focus: FocusTarget::Elem(FocusKey { entry: self.page.entry, elem: PLAY_ELEM }) }))));
                }
            }
        }
        self.tab_seat.on(&super::registry::AppArg::Home, self.page.entry, self.instance, ev, cx, fx);
        let card = match self.stack.on(&self.page, ev, cx, fx) {
            Some(StackEvent::Card(_, card)) => Some(card),
            _ => None,
        };
        match ev {
            ScreenEvent::Mount | ScreenEvent::Enter(_) | ScreenEvent::Uncover => {
                self.page.request();
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::Activate(elem) if CTRLS.contains(elem) || *elem >= plx_ui::dispatch::STRIP_BASE => {
                self.press(*elem, fx);
                Handled::Yes
            }
            // OK on a control (a hero button, a tab pill) arms a press like a card's does, and its
            // commit names nothing: the element is the one the engine has focused.
            ScreenEvent::PressCommit(_) => {
                let control = cx.focus.current.filter(|k| k.entry == self.page.entry)
                    .map(|k| k.elem)
                    .filter(|e| CTRLS.contains(e) || *e >= plx_ui::dispatch::STRIP_BASE);
                match (card, control) {
                    (Some(CardEvent::Activate(elem)), _) | (None, Some(elem)) => self.press(elem, fx),
                    _ => {}
                }
                Handled::Yes
            }
            ScreenEvent::Tick(_) => Handled::Yes,
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Screen<H> for HomeScreen {
    fn redraw_focused(&self, f: &mut DrawFrame<'_, '_, H>, focus: Option<FocusKey<u32>>) {
        self.stack.view(&self.page).redraw_focused(f, focus);
    }
    fn name(&self) -> &'static str { super::registry::word::HOME }
    fn state(&self) -> &dyn LogicalState { self }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<std::borrow::Cow<'_, str>> { None }
    fn prepare(&mut self, _budget: &mut plx_ui::frame::Budget, _cx: &Cx<'_, H>) {}
    /// DOWN from the tab bar lands on the hero's controls.
    fn links(&self, out: &mut Vec<Link>) {
        out.push(Link { from: plx_ui::containers::tabs::STRIP, dir: Dir::Down, to: HERO_GROUP });
    }
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        self.drawn = Some(self.page.changes);
        let p = f.painter.alpha(f.page_alpha);
        let scroll = self.stack.scroll();
        p.rrect(Rect::FULL, 0.0, 0.0, theme::SURFACE_APP);
        self.ground.draw(p.translate(0.0, -scroll));
        self.draw_hero(f, scroll);
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
    use std::sync::Arc;

    fn title(path: &str, added: u64, backdrop: bool) -> Entry {
        let mut e = Entry::new(Path::new(path).file_stem().unwrap().to_string_lossy().into_owned(), PathBuf::from(path), EntryKind::Video, None, 1);
        e.added = added;
        e.backdrop = backdrop.then(|| "L1".to_owned());
        e
    }

    #[test]
    fn recently_added_is_titles_newest_first_and_the_hero_has_a_backdrop() {
        let mut show = Entry::new("Caminandes".into(), PathBuf::from("/tv/Caminandes"), EntryKind::Folder, None, 0);
        show.show = true;
        show.added = 50;
        let collection = Entry::new("Silent".into(), PathBuf::from("/f/Silent"), EntryKind::Folder, None, 0);
        let films = Arc::new(vec![title("/f/a.mkv", 10, true), title("/f/b.mkv", 99, false), collection]);
        let tv = Arc::new(vec![show]);
        let recent = recently_added(&[films, tv]);
        assert_eq!(recent.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["b", "Caminandes", "a"],
            "a show is a title; a plain folder is not");
        assert_eq!(hero_of(&recent).unwrap().title, "a", "the newest with a backdrop to stand on");
        assert_eq!(hero_of(&recent[..1]).unwrap().title, "b", "else the newest");
        assert!(hero_of(&[]).is_none());
    }

    #[test]
    fn a_continued_file_is_found_in_its_folders_folder_or_its_own() {
        let mut film = title("/f/Charge (2022)/Charge.mkv", 0, true);
        film.meta = Some(Arc::new(Meta { title: "Charge".into(), ..Meta::default() }));
        let ep = title("/tv/Show/Season 01/e1.mkv", 0, false);
        let films = Arc::new(vec![film]);
        let season = Arc::new(vec![ep]);
        let p = Progress { offset_ms: 1, duration_ms: 10, watched: false, size: 1 };
        let places = [
            (PathBuf::from("/f/Charge (2022)/Charge.mkv"), p),
            (PathBuf::from("/tv/Show/Season 01/e1.mkv"), p),
            (PathBuf::from("/gone/x.mkv"), p),
        ];
        let found = continued(&places, |d| match d.to_str().unwrap() {
            "/f" => Some(Arc::clone(&films)),
            "/tv/Show/Season 01" => Some(Arc::clone(&season)),
            _ => None,
        });
        assert_eq!(found.iter().map(|(e, _)| e.display_title()).collect::<Vec<_>>(), ["Charge", "e1"],
            "a file whose folder has not landed waits");
    }
}
