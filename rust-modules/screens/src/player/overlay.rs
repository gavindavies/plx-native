//! **The player's overlays, as entries on the page's own `ModalStack`** (restructure spec §6.2
//! "Page-owned panels … Player: its four overlays", §9, phase 9).
//!
//! The track menu, the Chapters strip and the Timing capsule were
//! `Route::Player { overlay }` values driven by arms of `app/run.rs`'s key ladder, over
//! modules' worth of `static mut`. They are now `AppArg` variants presented on the player
//! page's stack, mounted by the one `Mounter`, styled `PlayerPanel { survives_failure }`, and each
//! owning its panel's state as a field. The container owns the PHASE and the appear spring; the
//! surface owns input while it is `Opening | Open`.
//!
//! **One screen type with an inner enum, not one type per panel**, because they differ only in which
//! panel they hold and share every rule that matters here: what a transport key does, how a held
//! direction is paced, when a panel dismisses itself, and how a decision reaches the loop. A copy
//! of those rules per panel is exactly the drift `overlay_swallows_key` was written to end.
//!
//! **The transport-key rule is the one behaviour a reader must not lose.** A viewer holding the
//! track menu or the Chapters strip open still expects PAUSE/PLAY to work, and the
//! panel to stay up (issue 28). While the ladder owned these keys that was expressed by
//! `overlay_swallows_key` answering `false` for exactly those three keys so the press FELL THROUGH
//! to the ordinary transport arms. A surface cannot fall through — the dispatcher hands it the key
//! and the ladder never sees it — so it FORWARDS instead: `PlayerReq::Transport`, which the loop
//! spends on the same toggle, leaving this panel untouched.
//!
//! **The `Focusable` half is real now (restructure phase 12, D2).** Each panel's own
//! `*Part` wrapper (`appkit::track_menu::TrackMenuPart`, `appkit::chapters_panel::ChaptersPart`)
//! answers the Engine's query
//! protocol (§7.1) over that panel's real row geometry, and [`PlayerOverlayScreen::step`] no
//! longer moves focus BY HAND: a direction falls through (`Handled::No`) to the engine's own
//! `neighbour`/`EdgeRule` unless this panel's own cadence gate is still waiting
//! ([`PANEL_REPEAT_MS`]) or the engine has just reached this panel's group EDGE and re-delivered
//! the key (`edge_key`, the one place a panel still decides something outside its own scope — a
//! tab switch, or dropping focus onto the HUD tabs below). OK is answered the same way: the
//! engine's own `Activate`/press machinery (§7.4) fires `ScreenEvent::Activate`/`PressCommit`,
//! which [`PlayerOverlayScreen::activate`] spends. A click resolves against the real per-row hit
//! map [`PlayerOverlayScreen::draw`] registers, not a hand-rolled pixel scan.

use std::borrow::Cow;
use std::os::raw::c_int;

use crate::registry::{AppFx, AppLike, PlayerReq};
use plx_appkit::chapters_panel::{chapter_count, ChaptersPart};
use plx_ui::consts;
use plx_ui::frame::Budget;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, LogicalState,
    Machine, NavOp,
};
use plx_ui::screen::{
    At, Dir, DrawFrame, FocusSource, Focusable, GroupSpec, HitSource, Part, Placed, RenderStrategy,
    Screen, ScreenEvent, Step,
};
use plx_appkit::timing_capsule::{CapsuleOut, TimingCapsule};
use plx_appkit::track_menu::{TrackMenuPart, TrackOk};

use super::HudPolicy;
use plx_ui::Rect;

use super::input::{HUD_LINGER_MS, HUD_MENU_MS};
use crate::registry::{RepeatGate, PANEL_REPEAT_MS};

/// The fields [`PlayerOverlayScreen::write`] canonicalises, for the recorder's shape pin (§5.4).
/// The selected ROW is in it deliberately: these panels' UP/DOWN changes nothing else in the app,
/// so without it a replay grades a panel opening and closing and nothing between.
///
/// `sel` is the highlighted row's index for every panel but the track menu, whose own canon
/// ([`plx_appkit::track_menu::TrackMenuState::canon`]) replaces it: its `tab`, the page-path depth,
/// each pushed page's `{page,return_key}` and the selected row's KEY.
pub const SHAPE: &str =
    "PlayerOverlayScreen{kind:str,sel:u32|Tracks:{tab:u32,depth:u32,pages:[{page:u32,return_key:u32}],key:u32}}";

/// Which panel a [`PlayerOverlayArg`] names, and — since the argument is what the container holds
/// for the whole life of the entry — the identity `ScreenArg::same_instance` compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverlayKind {
    /// The audio/subtitle picker (`appkit/track_menu.rs`). `tab` is the panel it opens on: the two
    /// on-screen discs open it directly on theirs.
    Tracks { tab: c_int },
    /// The chapter strip (`appkit/chapters_panel.rs`).
    Chapters,
    /// The on-video Subtitle Timing capsule (`appkit/timing_capsule.rs`, plan `subtitle-menu-capsule`
    /// §4) — the Subtitles panel's Timing row hands off here
    /// (`appkit::track_menu::TrackOk::OpenTiming`) rather than stepping the offset itself. The only
    /// overlay whose [`Self::hud_policy`] is [`HudPolicy::Hidden`] — the capsule sits where the
    /// transport's caption band and scrubber would, so the two must never be up together.
    Timing,
}

impl OverlayKind {
    /// **One of every kind**, parameters at their plain-open value — the list every "each panel"
    /// census iterates (the heartbeat alphabet, `registry::every_surface_arg`, the surface
    /// navigation tests), so a new kind is added here once rather than to each of them.
    /// `every_kind_is_listed_once` fails to compile when a variant is added and not listed.
    #[cfg(any(test, feature = "test-support"))]
    pub const ALL: [OverlayKind; 3] = [OverlayKind::Tracks { tab: 0 }, OverlayKind::Chapters, OverlayKind::Timing];

    /// The heartbeat's `overlay=` word. These spellings are the ones `tests/manifest.json`'s
    /// fps scenes select by; changing one silently disarms a scene rather than failing anything
    /// visible (§15.3). Since phase 10 item 4 they reach the heartbeat DIRECTLY — `app::overlay_word`
    /// is the topmost surface's own `Screen::name`, so there is no mapping table between this
    /// function and the printed line, and `app::heartbeat_word_tests` derives its alphabet by
    /// presenting every surface argument and reading the word back.
    /// **WHICH panel this is, with the parameter thrown away** — the identity
    /// `ScreenArg::same_instance` compares, and the reason it is not the whole `OverlayKind`.
    ///
    /// The two on-screen discs open the SAME track menu on their own tab. So `tab` is
    /// a boot ADDRESS, not an identity — exactly as `AppArg::Settings`'s root page is — and a
    /// container that compared them would stack a second track menu on top of the first when the
    /// viewer moved from Subtitles to Audio.
    pub fn slot(self) -> u8 {
        match self {
            OverlayKind::Tracks { .. } => 0,
            OverlayKind::Chapters => 1,
            OverlayKind::Timing => 2,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            OverlayKind::Tracks { .. } => "menu",
            OverlayKind::Chapters => "chapters",
            OverlayKind::Timing => "timing",
        }
    }

    /// **What this panel does to the transport HUD while it is up** (any phase but `Hidden` —
    /// `app::bridge::player_hud_policy`, plan `subtitle-menu-capsule` §4). Only the Timing capsule
    /// HIDES it: it sits at the caption's own band, over the scrubber, so the transport and the
    /// capsule must never both be drawn. The other panels leave it up and lift the captions clear
    /// of it — the same distinction `extends_hud` answers for
    /// a different question.
    pub fn hud_policy(self) -> HudPolicy {
        match self {
            OverlayKind::Timing => HudPolicy::Hidden,
            OverlayKind::Tracks { .. } | OverlayKind::Chapters => HudPolicy::Lifted,
        }
    }

    /// **Does this panel keep the transport alive while a viewer reads it** (the per-frame
    /// `ExtendHud` of `PlayerOverlayScreen`'s `Tick`, and the ordinary linger a hand-off INTO it
    /// leaves behind)? Every panel that shows the HUD; never one that hides it, since extending a
    /// HUD it is hiding would fight [`HudPolicy::Hidden`].
    pub fn extends_hud(self) -> bool {
        self.hud_policy() != HudPolicy::Hidden
    }

}

/// What the container is asked to present. The `host` is the player instance the panel reports back
/// to; it rides on the argument rather than being looked up, for `LibraryMenuArg`'s reason — the
/// entry outlives any one frame's idea of which page is on top.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlayerOverlayArg {
    pub kind: OverlayKind,
}

impl LogicalState for PlayerOverlayArg {
    fn write(&self, c: &mut Canon) {
        c.u32(self.kind.slot() as u32);
        match self.kind {
            OverlayKind::Tracks { tab } => {
                c.u32(tab as u32);
            }
            OverlayKind::Chapters | OverlayKind::Timing => {}
        }
    }
    fn probe(&self, out: &mut String) {
        out.push_str(self.kind.word());
    }
}

/// The panel itself — the state its modules kept in `static mut`s until phase 9.
pub enum Panel {
    Tracks(plx_appkit::track_menu::TrackMenuState),
    Chapters(plx_appkit::chapters_panel::ChaptersState),
    Timing(TimingCapsule),
}

pub struct PlayerOverlayScreen {
    entry: EntryId,
    kind: OverlayKind,
    panel: Panel,
    /// The cadence a HELD direction walks this panel's list at. The loop's own client-side repeat
    /// timer used to do this at 110 ms for exactly these panels; the dispatcher delivers the
    /// hardware's ~50 ms `Edge::Repeat` instead, so the surface applies the cadence itself
    /// ([`PANEL_REPEAT_MS`]).
    repeat: RepeatGate,
    /// The panel draws nothing this frame (a FAILED playback hides the content panels with the
    /// transport), so it must ask for no dim either. Resolved in `prepare`, which runs before the
    /// page pass the container paints dims in; `draw` is too late for that.
    suppressed: bool,
}

/// **"Your languages"**, in preference order, for the Subtitles menu's grouping
/// (`metadata::sub_layout::sub_sections`): the subtitle-language preference this play resolved under
/// (`playback::cur_sub_pref_lang`), then the playing audio's language, then the current subtitle's
/// own — each only if it names one.
fn subtitle_yours_langs(ps: &plx_media::playback::PlaybackSession, meta: plx_data::metadata::MetadataView<'_>) -> Vec<String> {
    let mut yours = Vec::new();
    if let Some(pref) = plx_media::playback::cur_sub_pref_lang(ps) {
        if !pref.trim().is_empty() {
            yours.push(pref.to_string());
        }
    }
    if let Some(item) = meta.playing() {
        let asid = plx_media::playback::cur_audio_sid(ps);
        if let Some(a) = item.audio.iter().find(|s| s.id == asid) {
            if !a.lang_code.trim().is_empty() {
                yours.push(a.lang_code.clone());
            }
        }
        let ssid = plx_media::playback::cur_sub_sid(ps);
        if let Some(s) = item.subs.iter().find(|s| s.id == ssid) {
            if !s.lang_code.trim().is_empty() {
                yours.push(s.lang_code.clone());
            }
        }
    }
    yours
}

impl PlayerOverlayScreen {
    /// Every panel here answers on exactly one focus group — see each `*Part`'s own `groups()`
    /// (restructure phase 12); this screen never holds more than one panel at a time, so there is
    /// no second id to reserve.
    const GROUP: GroupId = GroupId(0);

    pub fn new(ps: &plx_media::playback::PlaybackSession, meta: plx_data::metadata::MetadataView<'_>, entry: EntryId, kind: OverlayKind) -> Self {
        let panel = match kind {
            OverlayKind::Tracks { tab } => Panel::Tracks(plx_base::diag::spans::span("tmnew", || {
                plx_appkit::track_menu::TrackMenuState::new(ps, meta, tab, subtitle_yours_langs(ps, meta))
            })),
            OverlayKind::Chapters => {
                Panel::Chapters(plx_appkit::chapters_panel::ChaptersState::new(meta))
            }
            OverlayKind::Timing => {
                let (lo, hi) = plx_media::player::subtitle_offset_range_ms();
                Panel::Timing(TimingCapsule::new(plx_media::player::subtitle_offset_ms(), lo, hi))
            }
        };
        Self {
            entry,
            kind,
            panel,
            repeat: RepeatGate::IDLE,
            suppressed: false,
        }
    }

    pub fn kind(&self) -> OverlayKind {
        self.kind
    }

    /// **Re-address a panel that is already up.** The two disc icons open the one track menu on
    /// their own tab, so the second press has to move the tab of the entry that exists rather than
    /// present a second one — `same_instance` says they ARE the same instance, and this is the
    /// other half of that: what "the same instance, at a different address" does.
    pub fn retarget(&mut self, meta: plx_data::metadata::MetadataView<'_>, kind: OverlayKind) {
        if kind.slot() != self.kind.slot() {
            return;
        }
        self.kind = kind;
        match (&mut self.panel, kind) {
            (Panel::Tracks(p), OverlayKind::Tracks { tab }) => p.focus_tab(meta, tab),
            _ => {}
        }
    }

    pub fn panel(&self) -> &Panel {
        &self.panel
    }

    /// The highlighted row, for the focus probe — a READ of the cursor a key press moves, and the
    /// reason it exists: these panels' UP/DOWN changes nothing else, so without it the fingerprint
    /// records a panel opening and closing and nothing between.
    pub fn sel(&self) -> i32 {
        match &self.panel {
            Panel::Tracks(p) => p.sel(),
            Panel::Chapters(p) => p.sel(),
            // Timing exposes no rows and no cursor — the capsule is driven entirely by its own
            // `key()`, never by the engine's focus movement.
            Panel::Timing(_) => 0,
        }
    }

    /// Resolve `/tmp/pitv-menupick`'s second field to an absolute row: a plain row number.
    /// `None` for anything else. The Subtitles tab's `"track:N"` is not a row at all (the track may sit
    /// on a page that is not showing): see [`Self::resolve_menupick_track`].
    pub fn resolve_menupick_row(&self, target: &str) -> Option<c_int> {
        target.parse::<c_int>().ok()
    }

    /// Resolve a `"track:N"` target of `/tmp/pitv-menupick` to the subtitle's index in the
    /// item's list ([`plx_appkit::track_menu::TrackMenuState::sub_track_for_target`]) — root tracks
    /// first, then the ones behind Other languages. `None` for any other target.
    pub fn resolve_menupick_track(&self, target: &str) -> Option<usize> {
        match &self.panel {
            Panel::Tracks(p) => p.sub_track_for_target(target),
            _ => None,
        }
    }

    /// `submenuosc`'s read of the Tracks panel: `(tab, page depth, has Other languages)`; `None`
    /// when the panel on screen is not Tracks.
    pub fn tracks_probe(&self) -> Option<(c_int, usize, bool)> {
        match &self.panel {
            Panel::Tracks(p) => Some(p.osc_probe()),
            _ => None,
        }
    }

    /// `submenuosc`'s cursor seat: put the Tracks cursor on `row` so the next real RIGHT key
    /// enters it. `false` when this page has no such row.
    pub fn seat_track_row(&mut self, row: plx_appkit::track_menu::TrackRow) -> bool {
        match &mut self.panel {
            Panel::Tracks(p) => p.focus_id(row),
            _ => false,
        }
    }

    /// **The headless pick of a named subtitle track**: commit the track at `i` by its own index
    /// ([`plx_appkit::track_menu::TrackMenuState::commit_sub_track`]), whatever page its row is on.
    /// Like [`Self::pick_track_row`] it leaves the panel on screen.
    pub fn pick_sub_track(
        &mut self,
        meta: plx_data::metadata::MetadataView<'_>,
        i: usize,
    ) -> Option<plx_appkit::track_menu::TrackCommit> {
        match &mut self.panel {
            Panel::Tracks(p) => match p.commit_sub_track(i, meta) {
                TrackOk::Commit { commit, .. } => Some(commit),
                TrackOk::Dismiss | TrackOk::OpenTiming | TrackOk::Inert | TrackOk::Navigated => None,
            },
            _ => None,
        }
    }

    /// **The headless track pick** (`/tmp/pitv-menupick=<tab>,<row>`): seat the cursor on
    /// `row` and confirm it, exactly as a viewer's DOWN…DOWN…OK would. It is a method rather than
    /// two calls at the trigger's site because the panel's state is an INSTANCE now — the trigger
    /// presents the surface and the body is mounted at nav commit, a frame later — so the loop
    /// reaches the cursor through the container or not at all.
    ///
    /// Dismissing afterwards is the caller's business, and deliberately is not done here: the
    /// trigger exists to leave the chosen track's panel on screen for a capture.
    pub fn pick_track_row(
        &mut self,
        meta: plx_data::metadata::MetadataView<'_>,
        row: c_int,
    ) -> Option<plx_appkit::track_menu::TrackCommit> {
        if let Panel::Tracks(p) = &mut self.panel {
            p.focus_row(row);
            return match p.on_ok(meta) {
                TrackOk::Commit { commit, .. } => Some(commit),
                TrackOk::Dismiss | TrackOk::OpenTiming | TrackOk::Inert | TrackOk::Navigated => None,
            };
        }
        None
    }

    fn ask<H: AppLike>(fx: &mut Effects<'_, H>, req: PlayerReq) {
        fx.push(Fx::App(AppFx::Player(req)));
    }

    fn dismiss<H: AppLike>(&self, fx: &mut Effects<'_, H>) {
        fx.push(Fx::Nav(NavOp::Dismiss(self.entry)));
    }

    /// A direction moved this panel's cursor: the transport stays up for a MENU's read time, not
    /// the plain linger.
    fn moved<H: AppLike>(&self, fx: &mut Effects<'_, H>) {
        Self::ask(fx, PlayerReq::ExtendHud(HUD_MENU_MS));
    }

    /// This panel is going away by a viewer's own press: hand the transport the ordinary linger.
    fn closing<H: AppLike>(&self, fx: &mut Effects<'_, H>) {
        Self::ask(fx, PlayerReq::ExtendHud(HUD_LINGER_MS));
    }

    /// **Apply whatever the focused element decided, at once** (§7.4). Reached from a `Bare` row's
    /// (`Tracks`) key-down/pointer-click `Activate`, and from `Chapters`'s (`Card`) `PressCommit` on
    /// release. The panel's own cursor is already correct — every `FocusMoved` this screen sees
    /// writes it back — so this reads the panel's OWN `on_ok`.
    fn activate<H: AppLike + crate::registry::MetadataLike>(
        &mut self,
        cx: &Cx<'_, H>,
        fx: &mut Effects<'_, H>,
    ) {
        match &mut self.panel {
            Panel::Tracks(p) => match p.on_ok(H::metadata(cx)) {
                TrackOk::Commit { commit, keep_open } => {
                    fx.push(Fx::App(AppFx::Player(PlayerReq::CommitTrack(commit))));
                    if keep_open {
                        // a Style pick (or an audio toggle): the viewer is watching the change, so
                        // the transport keeps a menu's read time rather than starting to close
                        self.moved(fx);
                    } else {
                        self.dismiss(fx);
                        self.closing(fx);
                    }
                }
                TrackOk::Dismiss => {
                    self.dismiss(fx);
                    self.closing(fx);
                }
                // a dim row (Timing while subtitles are Off, a locked Style row): nothing happens and
                // the panel stays. A Nav row opened a page: the panel stays with its rows replaced.
                TrackOk::Inert | TrackOk::Navigated => self.moved(fx),
                TrackOk::OpenTiming => {
                    // The Tracks→Timing hand-off (plan §4): dismiss THIS entry and ask for a fresh
                    // `Timing` overlay. `open_player_overlay` sees a different slot
                    // (`OverlayKind::slot`) than the one being dismissed, so it presents a new
                    // entry rather than re-addressing this one. The ordinary linger is left only
                    // if the panel taking over keeps the HUD at all — the capsule hides it.
                    let next = OverlayKind::Timing;
                    self.dismiss(fx);
                    Self::ask(fx, PlayerReq::OpenOverlay(next));
                    if next.extends_hud() {
                        self.closing(fx);
                    }
                }
            },
            Panel::Chapters(p) => {
                let ns = p.on_ok(H::metadata(cx));
                self.dismiss(fx);
                if ns >= 0 {
                    Self::ask(fx, PlayerReq::SeekTo(ns));
                }
                self.closing(fx);
            }
            // Unreachable in practice: Timing publishes no focus group and no `ElemKind::Bare`
            // row (`Focusable::groups` answers empty, `key()` returns `Handled::Yes` for every
            // key before `Machine::step` ever reaches its `Activate`/`PressCommit` arms), so
            // nothing ever calls `activate()` while this variant is up. Exhaustiveness only.
            Panel::Timing(_) => {}
        }
    }

    /// **The engine reached this panel's group EDGE and re-delivered the direction**
    /// (`EdgeRule::Screen`, §7.3 step 3) — the one thing left that a panel decides outside its own
    /// scope, because it moves focus OFF this screen (Chapters' DOWN) or re-addresses the panel
    /// entirely (Tracks' LEFT/RIGHT: LEFT pops a sub-page, else switches tab
    /// (`TrackMenuState::focus_tab`); RIGHT enters a Nav row, else switches tab).
    fn edge_key<H: AppLike + crate::registry::MetadataLike>(
        &mut self,
        cx: &Cx<'_, H>,
        key: consts::Key,
        fx: &mut Effects<'_, H>,
    ) -> Handled {
        use consts::Key;
        match (&mut self.panel, key) {
            // LEFT pops a sub-page, else switches tab as before; RIGHT on a Nav row enters it (inert
            // when that row is disabled), else switches tab as before
            (Panel::Tracks(p), Key::Left { .. }) => {
                p.on_left(H::metadata(cx));
                self.moved(fx);
            }
            (Panel::Tracks(p), Key::Right { .. }) => {
                p.on_right(H::metadata(cx));
                self.moved(fx);
            }
            (Panel::Chapters(_), Key::Down) => {
                self.dismiss(fx);
                Self::ask(fx, PlayerReq::FocusTabs);
                self.closing(fx);
            }
            _ => {} // no other panel declares any other edge escape
        }
        Handled::Yes
    }

    /// The one key ladder these panels share, for everything the ENGINE does not already resolve
    /// (§7.3 step 1). Transport keys always fall through here first (module doc) — the only keys
    /// this ladder still fully owns. A direction is paced by [`PANEL_REPEAT_MS`] and then handed
    /// to the engine's own `neighbour`/`EdgeRule` (`Handled::No`) unless the engine has already
    /// reached this panel's group edge and re-delivered it (`edge_key`, above). OK is likewise
    /// left to the engine's own `Activate`/press machinery (§7.4; see [`Self::activate`]). BACK
    /// dismisses — except on a Tracks sub-page, where it pops one page first (the root, and the
    /// other panels, dismiss). Tracks closes silently; Chapters also hands the transport the
    /// ordinary linger.
    fn key<H: AppLike + crate::registry::MetadataLike>(
        &mut self,
        cx: &Cx<'_, H>,
        key: consts::Key,
        edge: Edge,
        at_edge: bool,
        now: u32,
        fx: &mut Effects<'_, H>,
    ) -> Handled {
        use consts::Key;
        // Transport first, above every panel's own arms: this is the rule that outranks modality.
        if matches!(key, Key::Pause | Key::Play | Key::PlayPause) {
            if edge == Edge::Down {
                Self::ask(
                    fx,
                    PlayerReq::Transport(match key {
                        Key::Play => Some(true),
                        Key::Pause => Some(false),
                        _ => None,
                    }),
                );
            }
            return Handled::Yes;
        }
        // The Timing capsule owns every key itself — it has no focus group for the engine to
        // walk, so it never falls through to the directional block below and OK/BACK must not
        // reach `Machine::step`'s `Activate` arm either (the capsule has nothing for that
        // machinery to activate). The cadence is the SAME `RepeatGate` every other panel here
        // paces a held direction with — one admission rule, reused rather than a second copy of
        // it living in `appkit::timing_capsule`.
        if let Panel::Timing(cap) = &mut self.panel {
            match edge {
                Edge::Down => self.repeat.rearm(now),
                Edge::Repeat if !self.repeat.ready_every(now, PANEL_REPEAT_MS) => {
                    return Handled::Yes
                }
                Edge::Up => return Handled::Yes,
                _ => {}
            }
            match cap.key(key, edge, now) {
                Some(CapsuleOut::Step(v)) => {
                    Self::ask(
                        fx,
                        PlayerReq::CommitTrack(plx_appkit::track_menu::TrackCommit::SubtitleOffset(v)),
                    );
                }
                // The transport stays down after the capsule leaves — not asked for here, since a
                // pointer miss closes it without reaching this ladder; `PlayerScreen::set_hud_policy`
                // turns ANY close into a dismissed HUD on the frame the surface is gone.
                Some(CapsuleOut::Close) => self.dismiss(fx),
                Some(CapsuleOut::Bump) | None => {}
            }
            return Handled::Yes;
        }
        // A held direction is paced; a FRESH press is never swallowed by the cadence before it.
        let directional = matches!(key, Key::Up | Key::Down | Key::Left { .. } | Key::Right { .. });
        if directional {
            match edge {
                Edge::Down => self.repeat.rearm(now),
                Edge::Repeat if !self.repeat.ready_every(now, PANEL_REPEAT_MS) => {
                    return Handled::Yes
                }
                Edge::Up => return Handled::Yes,
                _ => {}
            }
            if at_edge {
                return self.edge_key(cx, key, fx);
            }
            // an interior move: let the engine's own `neighbour`/`EdgeRule` answer it (§7.3 steps
            // 2-3) rather than moving the panel's cursor by hand.
            return Handled::No;
        }
        if edge != Edge::Down {
            return Handled::Yes;
        }
        match key {
            Key::Back => {
                // BACK on a Subtitles sub-page goes up one page; on a root it dismisses
                if let Panel::Tracks(p) = &mut self.panel {
                    if p.pop(H::metadata(cx)) {
                        self.moved(fx);
                        return Handled::Yes;
                    }
                }
                self.dismiss(fx);
                if matches!(self.panel, Panel::Chapters(_)) {
                    self.closing(fx);
                }
                Handled::Yes
            }
            // Not consumed here: the engine's own `Activate`/`PressArm` machinery answers OK by
            // the focused element's `ElemKind` (§7.4), and `Machine::step`'s `Activate`/
            // `PressCommit` arms below spend what it decided (`Self::activate`).
            Key::Ok => Handled::No,
            _ => Handled::Yes,
        }
    }
}

impl<H: crate::registry::PlayerLike + crate::registry::MetadataLike> Machine<H> for PlayerOverlayScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        // The frame's publication of the playback session (spec §2.3): a screen reads the Player
        // machine's decisions here and asks for a change through an effect, never by writing them.
        let ps = H::session(cx);
        match ev {
            ScreenEvent::Input(input) => {
                // The dispatcher's `machine::Key` is the four directions plus OK/BACK; the
                // transport alphabet this ladder turns on lives in `consts::Key`. Preserve the
                // canonical navigation key and consult raw fields only for `Other`.
                if let InputKind::Key {
                    key, sym, wcode, edge, at_edge,
                } = input.kind
                {
                    return self.key(cx, consts::classify_input(key, sym, wcode), edge, at_edge, input.at.ms, fx);
                }
                // **An open panel owns the click and closes on it.** The `modal_of` arms of
                // the loop's pointer path said this — "the transport is partly hidden while a
                // panel is up, so its rects must not be consulted" — and it is one answer here.
                //
                // The dispatcher hands a `Click`/`Pointer` to the owner whatever its
                // `hit_source()` is. (This comment said `HitSource::Legacy`; this surface has
                // answered `HitSource::Engine` since phase 12's contract freeze, and the property
                // it relies on was never the Legacy answer but the delivery.)
                if let InputKind::Click { .. } | InputKind::Pointer { .. } = input.kind {
                    // The Engine's own hit map now resolves every row (`Focusable::place` below,
                    // `DrawFrame::stop` registered in `draw`): a hit delivers `Activate` directly
                    // (§7.5-7.6, spent by `Self::activate` the same as a key OK), a hover parks
                    // focus through the engine, and a miss reaches `Style::PlayerPanel`'s own
                    // `OnMiss::Dismiss` (`ui/containers/modal.rs`) — this arm no longer scans
                    // pixels or a panel's own cursor by hand.
                    return Handled::No;
                }
                Handled::No
            }
            ScreenEvent::FocusMoved { to, .. } => {
                // §7.3 step 5: the owner's `step` is the only place that mutates in response to a
                // move the engine made — write the new cursor back into whichever panel is open,
                // and extend the transport's read time exactly as a hand-moved cursor used to.
                // Tracks' element is a row KEY (`appkit::track_menu::TrackRow`), the rest's an index.
                let i = to.elem as i32;
                match &mut self.panel {
                    Panel::Tracks(p) => p.focus_key(to.elem),
                    Panel::Chapters(p) => p.set_sel(i),
                    // Timing has no cursor the engine could have moved — its own `key()` owns
                    // every press and always returns `Handled::Yes`, so this arm is unreached for
                    // it in practice.
                    Panel::Timing(_) => {}
                }
                self.moved(fx);
                Handled::No
            }
            // A pointer click on ANY row (`Activate::Immediate`/`Direct`, every `*Part::draw`
            // above) always applies at once. A keyboard OK on a `Card` row (`Chapters`) arms an
            // engine press first and only reaches here as `PressCommit`, on release.
            ScreenEvent::Activate(elem) => {
                // the event names the row that was hit (its key for Tracks): seat the panel on
                // THAT row before `activate` reads the panel's own cursor, so a click never acts
                // on a neighbour the cursor happened to still hold
                if let Panel::Tracks(p) = &mut self.panel {
                    if plx_ui::page_stack::is_title_key(*elem) {
                        // the "< TITLE" band: a click goes back one page, and acts on no row
                        p.pop(H::metadata(cx));
                        self.moved(fx);
                        return Handled::No;
                    }
                    p.focus_key(*elem);
                }
                self.activate(cx, fx);
                Handled::No
            }
            ScreenEvent::PressCommit(_) => {
                self.activate(cx, fx);
                Handled::No
            }
            ScreenEvent::Tick(tick) => {
                let dt = tick.dt();
                match &mut self.panel {
                    Panel::Tracks(p) => p.update(dt, cx.measure, ps, H::metadata(cx)),
                    Panel::Chapters(p) => p.update(dt, H::metadata(cx)),
                    Panel::Timing(p) => p.update(dt),
                }
                // The transport must not auto-hide out from under a panel a viewer is reading —
                // the rule `app/run.rs` kept as "keep the HUD alive while the track menu /
                // Chapters strip is open", stated once here by the surface that IS open.
                // A panel that HIDES the HUD (the Timing capsule) does not share its read time.
                if self.kind.extends_hud() {
                    Self::ask(fx, PlayerReq::ExtendHud(HUD_LINGER_MS));
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

impl PlayerOverlayScreen {
    /// Every visible row registers its own stop (§7.6) — the same per-row geometry
    /// `Focusable::place` answers for focus movement, so the hit map and the focus engine agree on
    /// every rect. Each panel's own `*Part::draw` does the registration (it needs the
    /// module-private row geometry this screen cannot reach directly); the paint happens first, on
    /// the owned, mutable `Panel`. Paint-free, so issue #162's pointer census runs it on the host.
    pub fn record_stops<H>(&self, f: &mut DrawFrame<'_, '_, H>)
    where
        H: crate::registry::PlayerLike + crate::registry::MetadataLike,
    {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.draw(f, Rect::FULL),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(f.cx)) }.draw(f, Rect::FULL),
            // Timing owns its own key ladder and never asks the engine for focus, so it has no
            // per-row geometry to register — `draw`, below, paints the capsule itself.
            Panel::Timing(_) => {}
        }
    }
}

/// **Real per-row groups, delegated to whichever panel is ACTIVE** (restructure phase 12, D2 —
/// see `screens/player/mod.rs`'s own `Focusable` impl for the sibling case). Each panel exposes
/// its own row geometry through a `*Part` wrapper (`appkit::track_menu::TrackMenuPart`,
/// `appkit::chapters_panel::ChaptersPart`), built fresh per query over a SHARED reference to the panel's
/// own state — every method here is `&self`, and so is every method on this screen's own
/// `Focusable` impl (§7.1: "the engine never mutates a screen"), which is why each wrapper's
/// `state` field is `&'a StateType` rather than `&'a mut` (see each wrapper's own doc). This
/// screen holds exactly one panel at a time, so there is no `Composed`/`layout()` here — that
/// trait concatenates SEVERAL simultaneous parts, and one instance's panels never coexist.
impl<H: crate::registry::PlayerLike + crate::registry::MetadataLike> Focusable<H> for PlayerOverlayScreen {
    fn groups(&self, cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.groups(cx, out),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.groups(cx, out),
            // Timing declares no group at all — the capsule's own `key()` swallows every press
            // (`Handled::Yes`) before the engine's focus machinery ever sees one.
            Panel::Timing(_) => {}
        }
    }
    fn group_of(&self, key: &u32, cx: &Cx<'_, H>) -> Option<GroupId> {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.group_of(key, cx),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.group_of(key, cx),
            Panel::Timing(_) => None,
        }
    }
    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, cx: &Cx<'_, H>) -> Step<u32> {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.neighbour(key, dir, cx),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.neighbour(key, dir, cx),
            Panel::Timing(_) => Step::Edge,
        }
    }
    fn place(&self, key: &u32, cx: &Cx<'_, H>, at: At) -> Option<Placed> {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.place(key, cx, at),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.place(key, cx, at),
            Panel::Timing(_) => None,
        }
    }
    fn reconcile(&self, want: FocusKey<u32>, cx: &Cx<'_, H>) -> FocusKey<u32> {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.reconcile(want, cx),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.reconcile(want, cx),
            Panel::Timing(_) => want,
        }
    }
    fn seat(&self, g: GroupId, from: Placed, cx: &Cx<'_, H>) -> FocusKey<u32> {
        match &self.panel {
            Panel::Tracks(p) => TrackMenuPart { state: p, entry: self.entry, group: Self::GROUP }.seat(g, from, cx),
            Panel::Chapters(p) => ChaptersPart { state: p, entry: self.entry, group: Self::GROUP, count: chapter_count(H::metadata(cx)) }.seat(g, from, cx),
            Panel::Timing(_) => FocusKey { entry: self.entry, elem: 0 },
        }
    }
}

impl LogicalState for PlayerOverlayScreen {
    fn write(&self, c: &mut Canon) {
        c.str(self.kind.word());
        match &self.panel {
            // the tab, the page path with each opener, and the selected row's KEY
            Panel::Tracks(p) => p.canon(c),
            _ => {
                c.u32(self.sel() as u32);
            }
        }
    }
    fn probe(&self, out: &mut String) {
        out.push_str(self.kind.word());
    }
}

impl<H: crate::registry::PlayerLike + crate::registry::MetadataLike> Screen<H> for PlayerOverlayScreen {
    fn name(&self) -> &'static str {
        self.kind.word()
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _b: &mut Budget, cx: &Cx<'_, H>) {
        self.suppressed = plx_appkit::player_hud::transport_hidden(H::session(cx));
        // The first `update` is a frame behind the mount, so the root page's strings are queued
        // here for the open frame's drain instead (`TrackMenuState::warm_open`).
        if !self.suppressed {
            match &self.panel {
                Panel::Tracks(p) => p.warm_open(cx.measure),
                Panel::Chapters(_) | Panel::Timing(_) => {}
            }
        }
    }
    /// **The panel's dim, through the container like every other surface's.** It used to be
    /// hand-drawn at the top of each panel's `draw`; the container paints it now, at the end of the
    /// player's page pass — the same z-order (over the transport and subtitles, under the panel).
    /// The Chapters strip deliberately asks for none: it sits in the transport's own band and dims
    /// nothing.
    fn scrim(&self) -> plx_ui::screen::Scrim {
        use plx_ui::theme::underlay::DIM_PLAYER;
        if self.suppressed {
            return plx_ui::screen::Scrim::NONE;
        }
        match self.panel {
            Panel::Tracks(_) => plx_ui::screen::Scrim::over_video(DIM_PLAYER, None),
            // The capsule sits over the video like the Chapters strip — no dim.
            Panel::Chapters(_) | Panel::Timing(_) => plx_ui::screen::Scrim::NONE,
        }
    }
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let ps = H::session(f.cx);
        // Stale content panels are gone with the transport when a playback has FAILED.
        if plx_appkit::player_hud::transport_hidden(ps) {
            return;
        }
        // The container owns the appear spring; `DrawFrame::page_alpha` IS `Surface::motion.appear`
        // for a surface, which is what the panels' own `Popover` used to hold.
        let appear = f.page_alpha;
        let measure = f.measure;
        match &mut self.panel {
            Panel::Tracks(p) => p.draw(appear, measure),
            Panel::Chapters(p) => p.draw(appear, measure, H::metadata(f.cx)),
            // Fixed y, never the live caption's own baseline — see `player_hud::CAPSULE_BOTTOM_Y`.
            Panel::Timing(p) => p.draw(plx_appkit::player_hud::CAPSULE_BOTTOM_Y, appear),
        }
        self.record_stops(f);
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::VideoPlane
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
    /// **The pointer is held while the panel's card resizes or a page slides** (`ui::panel_motion`):
    /// the dispatcher swallows it before hit resolution, so a click on a row in motion is neither
    /// a hit on whatever sits under it nor a miss that would dismiss the surface.
    fn pointer_held(&self) -> bool {
        match &self.panel {
            Panel::Tracks(p) => p.transitioning(),
            Panel::Chapters(_) | Panel::Timing(_) => false,
        }
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
