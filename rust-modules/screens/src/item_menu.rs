//! The **item context menu** — a registered `Style::Compact` surface on the shared `ModalStack`.
//! Two kinds ([`crate::registry::MenuKind`]): **Watch**, opened by a press-and-hold on a video in
//! the library (the watch-state rows, then Play from Start — described below), and **Tracks**,
//! opened by the local title page's subtitle and audio discs: one row per choice, the one that
//! will play checked, the pick kept with the file's `local_progress::Choices`.
//!
//! A panel anchored BESIDE the focused card (the card and the rest of the shelf stay where they
//! are, visible behind it), rows of `[leading icon] [label]`, the focused row a filled light pill
//! spanning the panel.
//!
//! The rows are the watch-state group: the watch-state row (or rows), then Play from Start. Each
//! row states the OUTCOME its press produces, so a finished video offers only *Mark as Unwatched*
//! and one never started offers only *Mark as Watched*; a part-watched one is at neither end of
//! the range and gets both.
//!
//! It only **reports** the chosen [`Action`]: `AppFx::ItemMenu` carries it to the application,
//! which plays the file or writes its watch record. It never mutates playback or the record itself.

use std::borrow::Cow;
use std::convert::Infallible;

use crate::registry::{RepeatGate, PANEL_REPEAT_MS};
use crate::registry::{AppFx, AppLike, ItemMenuArg, ItemMenuReq};
use plx_ui::consts::*;
use plx_ui::form::{Activation, Form, FormId, FormSection, FormTable, RowKey, RowKind};
use plx_ui::frame::Budget;
use plx_ui::icons::Icon;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, Key,
    LogicalState, Machine, NavOp,
};
use plx_ui::screen::{
    Activate, At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, Focusable, GroupKind, GroupSpec,
    Hover, Placed, RenderStrategy, Screen, ScreenEvent, Scrim, Seat, Step, Stop,
};
use plx_ui::table::{Row, MENU_MAX_W, MENU_MIN_W};
use plx_ui::widgets::PosterMark;
use plx_ui::{theme, Rect};

/// What the highlighted row does on OK. Every variant carries the file it acts on, captured when
/// the menu opened, so nothing here is an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The ✓ row. **The verb is the ROW's, never the item's state read back**: a part-watched
    /// video offers BOTH rows and has no single "other end" to derive.
    MarkWatched(String),
    /// The − row. Twin of [`Action::MarkWatched`].
    MarkUnwatched(String),
    /// Play this file ignoring its resume point.
    PlayFromStart(String),
    /// Play this file with its audio track at this position (file order) — a title-page choice,
    /// kept with the file's other choices (`local_progress::Choices`).
    ChooseAudio(String, u32),
    /// …and with this subtitle.
    ChooseSubtitle(String, plx_data::local_progress::SubtitleChoice),
}

impl Action {
    /// The file this action acts on.
    pub fn path(&self) -> &str {
        match self {
            Action::MarkWatched(p) | Action::MarkUnwatched(p) | Action::PlayFromStart(p) => p,
            Action::ChooseAudio(p, _) | Action::ChooseSubtitle(p, _) => p,
        }
    }

    /// The watched state this action writes, if it is one of the two that do.
    pub fn watch_write(&self) -> Option<bool> {
        match self {
            Action::MarkWatched(_) => Some(true),
            Action::MarkUnwatched(_) => Some(false),
            Action::PlayFromStart(_) | Action::ChooseAudio(..) | Action::ChooseSubtitle(..) => None,
        }
    }

    fn write(&self, c: &mut Canon) {
        let tag = match self {
            Action::MarkWatched(_) => 3,
            Action::MarkUnwatched(_) => 4,
            Action::PlayFromStart(_) => 5,
            Action::ChooseAudio(_, i) => 0x100 + *i,
            Action::ChooseSubtitle(..) => 6,
        };
        c.u32(tag).str(self.path());
        if let Action::ChooseSubtitle(_, choice) = self {
            match choice {
                plx_data::local_progress::SubtitleChoice::Off => c.u32(0),
                plx_data::local_progress::SubtitleChoice::Track(n) => c.u32(1).u32(*n),
                plx_data::local_progress::SubtitleChoice::File(name) => c.u32(2).str(name),
            };
        }
    }
}

/// The pinned ~20px corner radius.
const PANEL_RAD: f32 = 20.0;
/// Air between the focused card's drawn edge and the panel — one `space` rung, like every other gap.
const CARD_GAP: f32 = theme::space::MD;
/// Keep-out from the screen edges, so a shelf near the bottom still gets a fully-visible panel.
///
/// Per AXIS, because the overscan frame is: `space::XL` 64 already cleared `MARGIN_Y` 54 vertically,
/// but the horizontal clamp is what a panel opened over the LAST column lands on, and 64 is 32px
/// outside `MARGIN_X`.
const EDGE: f32 = theme::space::XL;
const EDGE_X: f32 = plx_ui::consts::MARGIN_X;

pub const SHAPE: &str =
    "ItemMenu{arg:ItemMenuArg,acts:[Option<Action{tag:u32,path:str}>],sel:i32,table:TableViewMotion}";

/// A menu row's identity: which row it is. Hand-assigned keys, never a position — the row SET
/// varies by watch state, so a position would name a different row in every menu. A row is
/// declared once, with its action, so a row cannot exist without one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemRow {
    MarkWatched,
    MarkUnwatched,
    PlayFromStart,
    /// The `n`th choice of a track menu.
    Track(u32),
}

impl ItemRow {
    /// The focus element of this row (for a test, or a probe, that addresses it by identity).
    #[cfg(any(test, feature = "test-support"))]
    pub fn focus_key(self) -> u32 {
        self.key().0
    }
}

impl FormId for ItemRow {
    fn key(&self) -> RowKey {
        RowKey(match self {
            ItemRow::MarkWatched => 3,
            ItemRow::MarkUnwatched => 4,
            ItemRow::PlayFromStart => 5,
            ItemRow::Track(n) => 100 + n,
        })
    }
}

/// What the builder returns: a one-section form of button rows whose action is the whole of what a
/// press means (no row pushes a page — `Dest` is uninhabited).
type ItemForm = Form<ItemRow, Action, Infallible>;

/// The rows for a video in watch state `mark`: the watch-state row (or ROWS), then Play from Start.
///
/// **Both glyphs are FILLED discs**: [`Icon::CheckCircleFill`] for the row that marks watched,
/// [`Icon::MinusCircleFill`] for the one that takes it away — filled is what separates an ACTION
/// from a STATE. *Mark as Unwatched* throws the watch record away, so it is destructive and a menu
/// never OPENS on it (`TableView::opening_row`).
fn build(path: &str, mark: PosterMark) -> ItemForm {
    let sec = FormSection::new("")
        .item_if(
            mark != PosterMark::Watched,
            ItemRow::MarkWatched,
            RowKind::Button,
            Action::MarkWatched(path.to_string()),
            Row::new(plx_ui::widgets::mark_watched_verb()).licon(Icon::CheckCircleFill),
        )
        .item_if(
            mark != PosterMark::None,
            ItemRow::MarkUnwatched,
            RowKind::Button,
            Action::MarkUnwatched(path.to_string()),
            Row::new(plx_ui::widgets::mark_unwatched_verb())
                .licon(Icon::MinusCircleFill)
                .destructive(true),
        )
        .item(
            ItemRow::PlayFromStart,
            RowKind::Button,
            Action::PlayFromStart(path.to_string()),
            Row::new(plx_ui::widgets::play_from_start_verb()).licon(Icon::PlayStart),
        );
    Form::new().section(sec)
}

/// The rows of a track menu: one per choice, the one that will play checked. Nothing here is
/// destructive, so the menu opens on its first row.
fn build_tracks(rows: &[crate::registry::TrackRow], current: Option<usize>) -> ItemForm {
    let mut sec = FormSection::new("");
    for (i, row) in rows.iter().enumerate() {
        let mut r = Row::new(row.label.clone()).checked(current == Some(i));
        if !row.detail.is_empty() {
            r = r.detail(row.detail.clone());
        }
        sec = sec.item(ItemRow::Track(i as u32), RowKind::Button, row.act.clone(), r);
    }
    Form::new().section(sec)
}

/// Beside the card, never over it: to its RIGHT by default, flipped to its LEFT when that would run
/// past the screen's keep-out. Vertically it hangs off the card's top edge, pulled back inside the
/// safe band so a bottom shelf still gets a whole panel. Pure (anchor + measured height in, rect
/// out), which is what makes the placement rules host-testable.
fn panel_at(a: Rect, content_w: f32, content_h: f32) -> Rect {
    let width = content_w.clamp(MENU_MIN_W, MENU_MAX_W);
    let h = content_h.clamp(120.0, SCR_H - 2.0 * EDGE); // same floor the profile popover uses
    let right = a.x + a.w + CARD_GAP;
    let x = if right + width <= SCR_W - EDGE_X {
        right
    } else {
        a.x - CARD_GAP - width
    };
    let x = x.clamp(EDGE_X, (SCR_W - EDGE_X - width).max(EDGE_X));
    let y = a.y.clamp(EDGE, (SCR_H - EDGE - h).max(EDGE));
    Rect::new(x, y, width, h)
}

/// The anchor a menu with no rect to hang off falls back to — a centred card. The headless trigger
/// and a host with nothing focused both land here.
///
/// Resolved by the PRESENTER, once, rather than every frame inside [`ItemMenuScreen::frame`], so
/// the panel cannot drift if a host's rect stops resolving while the menu is up — the same
/// argument the legacy `present` made for resolving it before it stored `OPENER`.
pub fn fallback_anchor() -> Rect {
    Rect::new(
        (SCR_W - CARD_W) * 0.5 - MENU_MIN_W * 0.5,
        (SCR_H - CARD_H) * 0.5,
        CARD_W,
        CARD_H,
    )
}

pub struct ItemMenuScreen {
    entry: EntryId,
    arg: ItemMenuArg,
    /// The rows and the action each one commits, declared together ([`ItemForm`]) — the row SET
    /// varies by item kind, so a row is found by its identity ([`ItemRow`]), never by a position.
    form: FormTable<ItemRow, Action, Infallible>,
    /// The cadence a HELD direction walks this panel's list at. `app/run.rs`'s client-side repeat
    /// timer (`App::held_key`) did this at 110 ms for exactly this menu and, before phase 9, for
    /// the player's four panels; the dispatcher delivers the hardware's ~50 ms `Edge::Repeat`
    /// instead, so the surface applies the cadence itself — the same [`PANEL_REPEAT_MS`] those
    /// four take, reused rather than copied.
    repeat: RepeatGate,
    built: bool,
}


impl ItemMenuScreen {
    pub fn new(entry: EntryId, arg: ItemMenuArg) -> Self {
        Self {
            entry,
            arg,
            form: FormTable::new(crate::registry::BAND),
            repeat: RepeatGate::IDLE,
            built: false,
        }
    }

    /// The rows, built ONCE at `Mount`.
    fn build_rows(&mut self) {
        if self.built {
            return;
        }
        self.built = true;
        // a short list of one-line actions — BODY labels, not menu-size HEADLINE
        self.form.table.compact = true;
        let (form, keep) = match &self.arg.kind {
            crate::registry::MenuKind::Watch(mark) => (build(&self.arg.path, *mark), None),
            // a track menu opens on the choice that will play
            crate::registry::MenuKind::Tracks { rows, current } => (build_tracks(rows, *current), current.map(|i| ItemRow::Track(i as u32))),
        };
        self.form.set(form, keep.as_ref());
    }

    /// Where focus starts, and where it falls back to when the key it had is gone — the table's
    /// opening row, which skips the separator and never lands on a destructive row while any other
    /// is on offer.
    fn opening(&self) -> u32 {
        self.form.opening_key().map_or(0, |k| k.0)
    }

    fn frame(&self, measure: &dyn plx_machine::machine::Measure) -> Rect {
        let [x, y, w, h] = self.arg.anchor.map(f32::from_bits);
        panel_at(Rect::new(x, y, w, h), self.form.table.measured_width(measure), self.form.table.measured_height())
    }

    /// The rows a focus stop exists for — every bound row. The separator is unfocusable, exactly
    /// as `TableView::move_sel` skips it.
    fn focusable(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.form.table.n_rows() as usize).filter_map(|i| self.form.key_at(i).map(|k| k.0))
    }

    fn step_focus(&self, from: u32, delta: i32) -> Option<u32> {
        self.form.step_key(RowKey(from), delta).map(|k| k.0)
    }

    /// Commit the focused row: report the action, then dismiss. The legacy `on_ok` closed first and
    /// returned the action second; the two are one drain here, and the order is what keeps the
    /// loop's dispatch reading a REQUEST rather than a static a frame after the close.
    fn activate<H: AppLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let act = self
            .form
            .index_of_key(RowKey(elem))
            .and_then(|i| self.form.activate(i))
            .map(|a| match a {
                Activation::Action(act) => act,
                Activation::Push(never) => match never {},
            });
        // A key that names no row only dismisses.
        if let Some(act) = act.filter(|act| !act.path().is_empty()) {
            fx.push(Fx::App(AppFx::ItemMenu(ItemMenuReq { act })));
        }
        fx.push(Fx::Nav(NavOp::Dismiss(self.entry)));
    }

    /// The highlighted row, for the focus probe — a READ of the cursor the engine moves.
    pub fn sel(&self) -> i32 {
        self.form.table.sel
    }


    /// The page this menu hangs off, and the element on it the panel is anchored beside and lifted
    /// back out of the dim. ONE owner: the entry's own argument, captured when the menu was
    /// presented. It was `Bridge::menu_opener`, a second copy of the same pair on the side.
    pub fn opener(&self) -> (EntryId, Option<FocusKey<u32>>) {
        (self.arg.host, self.arg.focus)
    }
}

impl<H: AppLike> Machine<H> for ItemMenuScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Mount => {
                self.build_rows();
                // The menu is the thing the Home hold hint teaches; once ANY card screen has
                // opened one the lesson is learned and the hint retires (`ui::hold_hint::learned`).
                // A title page's track menu is opened by a press, not a hold, and teaches nothing.
                if matches!(self.arg.kind, crate::registry::MenuKind::Watch(_)) {
                    plx_ui::hold_hint::mark_learned();
                }
            }
            ScreenEvent::Tick(tick) => {
                self.form.table.sel = cx
                    .focus
                    .current
                    .filter(|key| key.entry == self.entry)
                    .and_then(|key| self.form.index_of_key(RowKey(key.elem)))
                    .map_or(self.form.table.sel, |i| i as i32);
                self.form.table.update(tick.dt(), self.frame(cx.measure).h);
            }
            ScreenEvent::FocusMoved { to, .. } => {
                if let Some(i) = self.form.index_of_key(RowKey(to.elem)) {
                    self.form.table.sel = i as i32;
                }
            }
            ScreenEvent::Activate(elem) => self.activate(*elem, fx),
            ScreenEvent::PressCommit(_) => {
                if let Some(key) = cx.focus.current {
                    self.activate(key.elem, fx);
                }
            }
            ScreenEvent::Input(input) => {
                let InputKind::Key { key, edge, .. } = input.kind else {
                    return Handled::No;
                };
                if key == Key::Back && edge == Edge::Down {
                    fx.push(Fx::Nav(NavOp::Dismiss(self.entry)));
                    return Handled::Yes;
                }
                // **The hold-to-move cadence, applied HERE and nowhere else.** It was
                // `app/run.rs`'s per-frame `App::held_key` timer, whose LAST consumer this menu
                // was; the dispatcher hands the surface the hardware's own `Edge::Repeat` at
                // ~50 ms, which walks a five-row list faster than anybody can read it. A FRESH
                // press is never swallowed by the cadence of the press before it — it is acted on
                // unconditionally by falling through to `Handled::No`, and `rearm` records it as
                // the step it is.
                if matches!(key, Key::Up | Key::Down) {
                    match edge {
                        Edge::Down => self.repeat.rearm(input.at.ms),
                        Edge::Repeat if !self.repeat.ready_every(input.at.ms, PANEL_REPEAT_MS) => {
                            return Handled::Yes
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Handled::No
    }
}

impl<H: AppLike> Focusable<H> for ItemMenuScreen {
    fn groups(&self, cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        out.push(GroupSpec {
            id: GroupId(0),
            kind: GroupKind::Column,
            seat: Seat::Remembered,
            reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop; 4],
            extent: self.frame(cx.measure),
            len: self.focusable().count(),
            elem: ElemKind::Bare,
        });
    }
    fn group_of(&self, elem: &u32, _: &Cx<'_, H>) -> Option<GroupId> {
        self.form.index_of_key(RowKey(*elem)).map(|_| GroupId(0))
    }
    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, _: &Cx<'_, H>) -> Step<u32> {
        let delta = match dir {
            Dir::Up => -1,
            Dir::Down => 1,
            _ => return Step::Edge,
        };
        match self.step_focus(key.elem, delta) {
            Some(elem) => Step::Move(FocusKey {
                entry: self.entry,
                elem,
            }),
            None => Step::Edge,
        }
    }
    fn place(&self, elem: &u32, cx: &Cx<'_, H>, _: At) -> Option<Placed> {
        <Self as Focusable<H>>::group_of(self, elem, cx)?;
        let index = self.form.index_of_key(RowKey(*elem))?;
        let rect = self.form.table.row_frame(self.frame(cx.measure), index as i32)?;
        Some(Placed {
            rect,
            rest_rect: rect,
            clip: self.frame(cx.measure),
            index: Some(index as u32),
        })
    }
    fn reconcile(&self, want: FocusKey<u32>, cx: &Cx<'_, H>) -> FocusKey<u32> {
        if <Self as Focusable<H>>::group_of(self, &want.elem, cx).is_some() {
            want
        } else {
            FocusKey {
                entry: self.entry,
                elem: self.opening(),
            }
        }
    }
    fn seat(&self, _: GroupId, _: Placed, _: &Cx<'_, H>) -> FocusKey<u32> {
        FocusKey {
            entry: self.entry,
            elem: self.form.selected_key().map_or_else(|| self.opening(), |k| k.0),
        }
    }
}

impl<H: AppLike> Screen<H> for ItemMenuScreen {
    fn opener(&self) -> Option<(EntryId, Option<FocusKey<u32>>)> {
        Some(ItemMenuScreen::opener(self))
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn name(&self) -> &'static str {
        crate::registry::word::ITEM_MENU
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    /// The modal dim. **The TILE is lifted back out of it, and the lift is the HOST PAGE's to
    /// draw** — only the screen that drew an element knows where it landed, and a `Scrim::lift` is
    /// a bare `fn()` with nothing to borrow a page through. So this asks for the dim alone, and
    /// `app/bridge.rs`'s `redraw_opener` — the ONE owner, reading this surface's own
    /// [`ItemMenuScreen::opener`] rather than a copy kept on the side — repaints the focused
    /// element immediately after the container's page pass, above the dim it just laid down.
    ///
    /// That tile is the panel's whole subject: the design's stated point is that "the card and the
    /// rest of the shelf stay where they are, visible behind it", which a scrim over the card
    /// itself quietly contradicts. The un-dimmed copy is also what the host snapshot holds.
    fn scrim(&self) -> Scrim {
        // The COMPACT role — the lightest dim in `theme::underlay`: the design's whole point is that
        // the card and the shelf stay legible behind the popover, so this recesses them rather
        // than blanking them.
        Scrim::dim(theme::underlay::DIM_COMPACT)
    }
    fn prepare(&mut self, _: &mut Budget, _: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter.alpha(f.page_alpha);
        let r = self.frame(f.measure);
        plx_ui::widgets::panel_ground(p, r, PANEL_RAD, f.underlay);
        self.form.table.draw(p, r, f.measure);
        for elem in self.focusable().collect::<Vec<_>>() {
            if let Some(placed) = <Self as Focusable<H>>::place(self, &elem, f.cx, At::Drawn) {
                f.stop(
                    p,
                    Stop {
                        key: FocusKey {
                            entry: self.entry,
                            elem,
                        },
                        rect: placed.rect,
                        rest_rect: placed.rest_rect,
                        clip: placed.clip,
                        hover: Hover::Focus,
                        activate: Activate::Immediate,
                    },
                );
            }
        }
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
}

impl LogicalState for ItemMenuScreen {
    fn write(&self, c: &mut Canon) {
        self.arg.write(c);
        let n = self.form.table.n_rows() as usize;
        c.seq(n);
        for i in 0..n {
            c.option(self.form.binding_at(i).map(|b| &b.action), |c, a| a.write(c));
        }
        c.u32(self.form.table.sel as u32);
        self.form.table.write_motion(c);
    }
    fn probe(&self, out: &mut String) {
        out.push_str("item_menu");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A built menu: the form run through a [`FormTable`], so a test reads rows back the way the
    /// screen does — by identity.
    struct Built(FormTable<ItemRow, Action, Infallible>);
    fn built(path: &str, mark: PosterMark) -> Built {
        let mut table = FormTable::new(crate::registry::BAND);
        table.set(super::build(path, mark), None);
        Built(table)
    }
    impl Built {
        fn action(&self, id: ItemRow) -> Action {
            let i = self.0.index_of(&id).unwrap_or_else(|| panic!("{id:?} is not on offer"));
            self.0.binding_at(i).expect("bound").action.clone()
        }
        fn actions(&self) -> Vec<Action> {
            (0..self.0.table.n_rows() as usize)
                .filter_map(|i| self.0.binding_at(i).map(|b| b.action.clone()))
                .collect()
        }
    }
    fn labels(menu: &Built) -> Vec<String> {
        menu.0.table.sections[0].rows.iter().map(|r| r.label.clone()).collect()
    }

    /// The row SET tracks the state: each row states the outcome its press produces, and a
    /// part-watched video gets both ends of the range.
    #[test]
    fn the_rows_follow_the_watch_state() {
        assert_eq!(labels(&built("/a.mkv", PosterMark::None)), ["Mark as Watched", "Play from Start"]);
        assert_eq!(labels(&built("/a.mkv", PosterMark::InProgress)),
            ["Mark as Watched", "Mark as Unwatched", "Play from Start"]);
        assert_eq!(labels(&built("/a.mkv", PosterMark::Watched)), ["Mark as Unwatched", "Play from Start"]);
    }

    /// Every row carries the file the menu was opened on, and every watch row performs the verb
    /// its label names.
    #[test]
    fn every_row_acts_on_its_own_file_with_the_verb_its_label_names() {
        let menu = built("/films/b.mkv", PosterMark::InProgress);
        for a in menu.actions() {
            assert_eq!(a.path(), "/films/b.mkv");
        }
        assert_eq!(menu.action(ItemRow::MarkWatched).watch_write(), Some(true));
        assert_eq!(menu.action(ItemRow::MarkUnwatched).watch_write(), Some(false));
        assert_eq!(menu.action(ItemRow::PlayFromStart).watch_write(), None);
    }

    /// **A menu never opens with its focus on a destructive action.** *Mark as Unwatched* throws
    /// the watch record away, so a watched video's menu opens on *Play from Start*.
    #[test]
    fn a_menu_never_opens_on_a_destructive_row() {
        let menu = built("/a.mkv", PosterMark::Watched);
        let id = *menu.0.id_at(menu.0.table.sel as usize).expect("the opening row acts");
        assert!(matches!(menu.action(id), Action::PlayFromStart(_)));
    }

    #[test]
    fn the_panel_sits_beside_the_card_and_never_leaves_the_screen() {
        let h = 5.0 * 60.0; // a five-row menu, roughly
                            // a card on the left of the shelf: the panel sits to its RIGHT, clear of the card
        let r = panel_at(Rect::new(MARGIN_X, 300.0, CARD_W, CARD_H), MENU_MIN_W, h);
        assert!(
            r.x >= MARGIN_X + CARD_W,
            "expected the panel right of the card, got x={}",
            r.x
        );
        // a card at the right edge: it flips LEFT rather than running off screen — and lands clear
        // of the card it belongs to, which is the whole point of anchoring beside it
        let a = Rect::new(SCR_W - 300.0, 300.0, CARD_W, CARD_H);
        let r = panel_at(a, MENU_MIN_W, h);
        assert!(
            r.x + r.w <= SCR_W - EDGE + 0.5,
            "panel ran off the right edge: x={} w={}",
            r.x,
            r.w
        );
        assert!(
            r.x + r.w <= a.x,
            "flipped panel overlaps its card: x={} w={} card.x={}",
            r.x,
            r.w,
            a.x
        );
        assert!(r.x >= EDGE - 0.5);
        // a card near the bottom keeps the whole panel on screen
        let low = panel_at(Rect::new(MARGIN_X, SCR_H - 120.0, CARD_W, CARD_H), MENU_MIN_W, h);
        assert!(
            low.y + low.h <= SCR_H - EDGE + 0.5,
            "panel ran off the bottom: y={} h={}",
            low.y,
            low.h
        );
        assert!(low.y >= EDGE - 0.5);

        // …and "on screen" means inside the OVERSCAN frame, which is why the keep-out is per axis:
        // `space::XL` 64 clears `MARGIN_Y` and misses `MARGIN_X` by 32. A panel placed against an
        // anchor has no fixed rect a table could carry, so the frame is graded on its extremes here.
        let tall = panel_at(Rect::new(MARGIN_X, 300.0, CARD_W, CARD_H), MENU_MIN_W, 4000.0);
        for (what, p) in [("flipped", r), ("low", low), ("tall", tall)] {
            assert!(
                plx_ui::consts::inside_safe(p),
                "the {what} panel leaves the safe area: ({}, {}) {}x{}",
                p.x,
                p.y,
                p.w,
                p.h
            );
        }
    }

    /// **A held direction walks the list at the panel's own cadence, not the hardware's.**
    ///
    /// `app/run.rs` did this from a per-frame `App::held_key` timer at 110 ms, and this menu was
    /// its LAST consumer — the block, the field and `HeldKey::arm` all go with it. The dispatcher
    /// hands the surface `Edge::Repeat` at the remote's own ~50 ms, so the cadence has to be
    /// applied by the surface that receives it: the same `RepeatGate` / [`PANEL_REPEAT_MS`] the
    /// player's four panels take, reused rather than copied.
    ///
    /// A swallowed repeat is `Handled::Yes` (the engine never sees it); an admitted one is
    /// `Handled::No`, which is what hands the direction to the focus engine. Red first, observed:
    /// with the `RepeatGate` arm deleted from `step`, every repeat falls through and the engine
    /// walks the list at the hardware rate — 12 admitted steps in the window below against 4.
    #[test]
    fn the_item_menu_hold_repeats_through_the_surface_not_the_loop() {
        let mut screen = screen_with("/a.mkv", PosterMark::None);
        // the fresh press: acted on unconditionally, and it re-arms the cadence from itself
        assert_eq!(feed(&mut screen, Key::Down, Edge::Down, 1000), Handled::No);
        // the hardware's own repeats, 50 ms apart
        let mut admitted = 0;
        for i in 1..=12u32 {
            if feed(&mut screen, Key::Down, Edge::Repeat, 1000 + i * 50) == Handled::No {
                admitted += 1;
            }
        }
        // Four, not the 5.5 the ratio suggests: the gate admits a step at the first repeat AT OR
        // PAST the cadence, and the hardware's 50 ms lattice means that is every third one — the
        // same rounding the loop's own 110 ms timer had against a 16 ms frame.
        assert_eq!(
            admitted, 4,
            "600 ms of hardware repeat is 4 steps at {PANEL_REPEAT_MS} ms, not 12 at the remote's 50"
        );
        // …and a FRESH press is never swallowed by the cadence of the press before it
        assert_eq!(feed(&mut screen, Key::Up, Edge::Down, 1620), Handled::No);
    }

    // ---- fixtures ---------------------------------------------------------------------------

    use crate::registry::{AppMsg, PageMemory};
    use plx_ui::fixture::FixtureMeasure;
    use plx_machine::machine::{FocusRead, Host, InputEvent, InputOwner, PressRead, Source, Tick};
    use plx_ui::screen::ScreenArg;

    #[derive(Clone)]
    struct Arg;
    impl LogicalState for Arg {
        fn write(&self, _: &mut Canon) {}
        fn probe(&self, _: &mut String) {}
    }
    impl ScreenArg for Arg {
        fn chrome(&self) -> plx_machine::machine::Chrome {
            plx_machine::machine::Chrome::None
        }
        fn id(&self) -> plx_machine::machine::ScreenId {
            plx_machine::machine::ScreenId(1)
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn same_instance(&self, _: &Self) -> bool {
            true
        }
    }
    struct HostFixture;
    impl Host for HostFixture {
        type Arg = Arg;
        type Fx = AppFx;
        type Msg = AppMsg;
        type Elem = u32;
        type Views<'a> = ();
        type Init = Arg;
        type Memory = PageMemory;
    }

    fn with_cx<R>(test: impl FnOnce(&Cx<'_, HostFixture>) -> R) -> R {
        let measure = FixtureMeasure;
        test(&Cx {
            views: (),
            tick: Tick::default(),
            measure: &measure,
            focus: FocusRead {
                current: None,
                ..Default::default()
            },
            press: PressRead::default(),
            owner: InputOwner::Entry(EntryId(7)),
        })
    }

    fn arg(path: &str, mark: PosterMark) -> ItemMenuArg {
        ItemMenuArg { path: path.into(), kind: crate::registry::MenuKind::Watch(mark), host: EntryId(1), focus: None, anchor: [0; 4] }
    }

    fn screen_with(path: &str, mark: PosterMark) -> ItemMenuScreen {
        let mut s = ItemMenuScreen::new(EntryId(7), arg(path, mark));
        s.build_rows();
        s
    }

    /// The focus key of the first row whose action satisfies `want`.
    fn first_action(s: &ItemMenuScreen, want: impl Fn(&Action) -> bool) -> u32 {
        (0..s.form.table.n_rows() as usize)
            .find(|i| s.form.binding_at(*i).is_some_and(|b| want(&b.action)))
            .and_then(|i| s.form.key_at(i))
            .expect("the row set offers this action")
            .0
    }

    /// Commit a row and return the ONE request it emitted.
    fn commit(s: &mut ItemMenuScreen, elem: u32) -> ItemMenuReq {
        let mut out = Vec::new();
        let mut present = plx_machine::present::Present::new();
        let mut fx = Effects::new(
            &mut out,
            plx_machine::machine::MachineId::Instance(plx_machine::machine::InstanceId(8)),
            &mut present,
        );
        s.activate::<HostFixture>(elem, &mut fx);
        let mut req = None;
        let mut dismissed = false;
        for e in out {
            match e.fx {
                Fx::App(AppFx::ItemMenu(r)) => req = Some(r),
                Fx::Nav(NavOp::Dismiss(_)) => dismissed = true,
                _ => {}
            }
        }
        assert!(
            dismissed,
            "every commit dismisses, exactly as the legacy `on_ok` closed first"
        );
        req.expect("a row that acts reports its action")
    }

    /// Step the screen with one event through a throwaway `Effects`.
    fn step_ev(s: &mut ItemMenuScreen, ev: &ScreenEvent<HostFixture>) -> Handled {
        let mut out = Vec::new();
        let mut present = plx_machine::present::Present::new();
        let mut fx = Effects::new(
            &mut out,
            plx_machine::machine::MachineId::Instance(plx_machine::machine::InstanceId(8)),
            &mut present,
        );
        with_cx(|cx| <ItemMenuScreen as Machine<HostFixture>>::step(s, ev, cx, &mut fx))
    }

    fn feed(s: &mut ItemMenuScreen, key: Key, edge: Edge, ms: u32) -> Handled {
        let ev = ScreenEvent::<HostFixture>::Input(InputEvent {
            at: Tick { ms, dt_us: 0 },
            source: Source::Sdl,
            kind: InputKind::Key {
                key,
                sym: 0,
                wcode: 0,
                edge,
                at_edge: false,
            },
        });
        step_ev(s, &ev)
    }

    /// A title page's track menu opens on the choice that will play, commits the row pressed, and
    /// teaches nothing about holding OK.
    #[test]
    fn a_track_menu_opens_on_its_checked_row_and_commits_the_choice() {
        use crate::registry::{MenuKind, TrackRow};
        use plx_data::local_progress::SubtitleChoice;
        plx_ui::hold_hint::reset_learned_for_test();
        let row = |label: &str, choice| TrackRow { label: label.into(), detail: String::new(), act: Action::ChooseSubtitle("/a.mkv".into(), choice) };
        let rows = vec![row("Off", SubtitleChoice::Off), row("English", SubtitleChoice::Track(0)), row("German", SubtitleChoice::File("a.de.srt".into()))];
        let mut screen = ItemMenuScreen::new(EntryId(7), ItemMenuArg { path: "/a.mkv".into(), kind: MenuKind::Tracks { rows, current: Some(2) }, host: EntryId(1), focus: None, anchor: [0; 4] });
        step_ev(&mut screen, &ScreenEvent::Mount);
        assert!(!plx_ui::hold_hint::learned(), "a press opened it, not a hold");
        let seat = with_cx(|cx| <ItemMenuScreen as Focusable<HostFixture>>::seat(&screen, GroupId(0), Placed { rect: Rect::FULL, rest_rect: Rect::FULL, clip: Rect::FULL, index: None }, cx));
        assert_eq!(seat.elem, ItemRow::Track(2).focus_key(), "opens on the checked row");
        let req = commit(&mut screen, ItemRow::Track(1).focus_key());
        assert_eq!(req.act, Action::ChooseSubtitle("/a.mkv".into(), SubtitleChoice::Track(0)));
    }

    /// **Opening the menu from ANY card screen retires Home's "Hold OK for options" hint**: the
    /// mount is the one door every presenter goes through, so the lesson is marked there.
    #[test]
    fn mounting_the_menu_retires_the_hold_hint() {
        plx_ui::hold_hint::reset_learned_for_test();
        let mut screen = ItemMenuScreen::new(EntryId(7), arg("/a.mkv", PosterMark::None));
        assert!(!plx_ui::hold_hint::learned());
        step_ev(&mut screen, &ScreenEvent::Mount);
        assert!(plx_ui::hold_hint::learned());
        plx_ui::hold_hint::reset_learned_for_test();
    }

    /// Committing a row reports its action, carrying the file, and dismisses the menu.
    #[test]
    fn a_commit_reports_the_file_the_menu_was_opened_on() {
        let mut screen = screen_with("/shows/s01e01.mkv", PosterMark::None);
        let elem = first_action(&screen, |a| matches!(a, Action::PlayFromStart(_)));
        let req = commit(&mut screen, elem);
        assert_eq!(req.act, Action::PlayFromStart("/shows/s01e01.mkv".into()));
    }
}
