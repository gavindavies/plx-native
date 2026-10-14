//! **The bridge between the loop and the dispatcher**: the application's `Host` ([`AppHost`]),
//! the rig the dispatcher borrows ([`Bridge`]: the real `TtfMeasure`, the stores, the requests an
//! owned screen makes of the loop), [`frame_with_tap`], which the loop calls once per iteration
//! with the inputs it collected, and the loop's navigations as container ops ([`nav_root`],
//! [`nav_push`], [`nav_pop`], [`nav_pop_to`]). The container is the one navigation authority.
//!
//! The contract with the loop, stated once:
//! - **Input.** The loop's ladders ask [`owns_input`] before their first arm. While a surface is
//!   up, or the top page is an owned one, every key, pointer move, click and wheel is an
//!   `InputEvent` for the dispatcher and the ladders see nothing.
//! - **Draw.** The loop draws the tree on ITS present gate (`Dispatcher::draw`). The dispatcher's
//!   own gate wanting a present becomes `idle::invalidate`.
//! - **The host under a surface.** The dispatcher's host fold is the loop's freeze/skip decision
//!   (`host_frozen`/`host_replaced`), and the surface's phases drive `popover`'s host-user
//!   counters ([`Bridge::sync_host`]).
//! - **Words.** The heartbeat's `overlay=` reads the topmost surface's `Screen::name`
//!   ([`overlay_word`]).

use std::ffi::CStr;

use plx_screens::player::HudPolicy;
use plx_screens::registry::{AppArg, AppFx, AppMounter, AppMsg, ContentReq, PageMemory};
use plx_data::stores::StoreCmd;
use plx_ui::containers::modal::{HostRender, Phase, Style};
use plx_ui::dispatch::{CxParts, Dispatcher, FrameReport, Rig, Split};
#[cfg(test)]
use plx_ui::dispatch::NoTap;
use plx_ui::frame::Budget;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Handled, Host,
    InputEvent, InputKind, InputOwner, Key, LogicalState, MachineId, Measure, NavOp,
    Source, Tick, TimerId,
};
use plx_machine::present::Present;
use plx_ui::screen::{At, DrawFrame, FocusSource, ReturnState};

// ---------------------------------------------------------------------------------------------
// the bundle
// ---------------------------------------------------------------------------------------------

pub(crate) struct AppHost;

#[derive(Clone, Copy)]
pub(crate) struct AppViews<'a> {
    /// The playing item's metadata (title, chapters, streams).
    pub(crate) metadata: plx_data::metadata::MetadataView<'a>,
    /// **The playback session, as this frame's publication.** The Player machine (`App.player`)
    /// owns the value; `Split` can only lend what the RIG owns, so the loop copies the decisions
    /// in once per frame ([`plx_media::playback::PlaybackSession::publication`]) and a screen reads
    /// them here. A screen that wants to CHANGE the playback emits an effect (`AppFx::Player`) —
    /// there is no `&mut` on this path by construction.
    pub(crate) session: &'a plx_media::playback::PlaybackSession,
}

impl Bridge {
    /// Every store's read handle for one context — the one spelling of the [`AppViews`] a screen
    /// reads through.
    pub(crate) fn views(&self) -> AppViews<'_> {
        AppViews {
            metadata: self.stores.metadata_view(),
            session: &self.playback,
        }
    }
}

#[derive(Default)]
pub(crate) struct BridgeInit;

impl LogicalState for BridgeInit {
    fn write(&self, _w: &mut Canon) {}
    fn probe(&self, out: &mut String) {
        out.push_str("bridge");
    }
}

impl Host for AppHost {
    type Arg = AppArg;
    type Fx = AppFx;
    type Msg = AppMsg;
    type Elem = u32;
    type Views<'a> = AppViews<'a>;
    type Init = BridgeInit;
    type Memory = PageMemory;

    fn shed_memory(memory: &mut PageMemory, focus: Option<u32>) {
        memory.shed_to_identity(focus);
    }

    fn app_fx_needs_return(fx: &AppFx) -> bool {
        !matches!(fx, AppFx::Store(..))
    }
}

impl plx_screens::registry::PlayerLike for AppHost {
    fn session<'a>(cx: &Cx<'a, Self>) -> &'a plx_media::playback::PlaybackSession { cx.views.session }
}

impl plx_screens::registry::MetadataLike for AppHost {
    fn metadata<'a>(cx: &Cx<'a, Self>) -> plx_data::metadata::MetadataView<'a> { cx.views.metadata }
}

// ---------------------------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------------------------

/// What the bridge lends the dispatcher, and what it collects for the loop.
pub(crate) struct Bridge {
    stores: plx_data::stores::Stores,
    mounter: AppMounter,
    /// This frame's publication of the playback session — see `AppViews::session`. Refreshed by
    /// [`Bridge::publish_playback`] from the loop, once per iteration.
    playback: plx_media::playback::PlaybackSession,
    /// Was the publication above refreshed on the last frame?
    playback_live: bool,
    /// **Is the video plane bound?** `Player::video_plane_bound`, published here by the loop on its
    /// edges — the rig's own copy of the one bit, for the call at draw entry
    /// (`clear_opaque_region`), which the `Rig` trait gives no argument.
    video_plane: bool,
    /// **Erased, so the host suite can supply one that does not need a font.** The app's own
    /// measure is `text::TtfMeasure`, which reads the fonts `init_text` opened at boot.
    measure: plx_ui::rec::Measurements,
    content_reqs: Vec<(MachineId, ContentReq, ReturnState<u32, PageMemory>)>,
    /// What the player's overlay surfaces asked of the loop this frame — drained by
    /// `playback::player_requests`, which holds the `MainThread` token they cannot.
    player_reqs: Vec<plx_screens::registry::PlayerReq>,
    /// …and what the item context menu asked, drained by `content::content_requests`.
    item_menu_reqs: Vec<plx_screens::registry::ItemMenuReq>,
    effect_return: ReturnState<u32, PageMemory>,
    /// The surfaces whose host counters this bridge holds: (entry, cached, closing).
    held: Vec<(EntryId, bool, bool)>,
    now_us: fn() -> u64,
    /// The top tab bar's publication and paint (`app::tabs`).
    tabs: super::tabs::TabChrome,
}

impl Bridge {
    pub(crate) fn new(now_us: fn() -> u64) -> Self {
        static TTF: plx_gfx::text::TtfMeasure = plx_gfx::text::TtfMeasure;
        Self::with_measure(&TTF, now_us)
    }

    /// The same bridge over a measure that needs no fonts — the ONLY constructor a host test may
    /// use: `TtfMeasure` dies on a `debug_assert!` with no font loaded.
    #[cfg(test)]
    pub(crate) fn for_test(now_us: fn() -> u64) -> Self {
        static FIXTURE: plx_ui::fixture::FixtureMeasure = plx_ui::fixture::FixtureMeasure;
        Self::with_measure(&FIXTURE, now_us)
    }

    fn with_measure(measure: &'static dyn Measure, now_us: fn() -> u64) -> Self {
        Self {
            stores: plx_data::stores::Stores::default(),
            mounter: AppMounter::default(),
            playback: plx_media::playback::PlaybackSession::IDLE,
            playback_live: false,
            video_plane: false,
            measure: plx_ui::rec::Measurements::Live(measure),
            content_reqs: Vec::new(),
            player_reqs: Vec::new(),
            item_menu_reqs: Vec::new(),
            effect_return: ReturnState::default(),
            held: Vec::new(),
            now_us,
            tabs: super::tabs::TabChrome::new(),
        }
    }

    /// Publish the tab bar for this frame (`app::tabs`), before the dispatcher runs.
    pub(crate) fn publish_tabs(&mut self, d: &mut Dispatcher<AppHost>, dt: f32) {
        let measure: &dyn Measure = &self.measure;
        self.tabs.publish(d, measure, dt);
    }

    pub(crate) fn take_content_reqs(&mut self) -> Vec<(MachineId, ContentReq, ReturnState<u32, PageMemory>)> {
        std::mem::take(&mut self.content_reqs)
    }

    pub(crate) fn take_player_reqs(&mut self) -> Vec<plx_screens::registry::PlayerReq> {
        std::mem::take(&mut self.player_reqs)
    }

    pub(crate) fn take_item_menu_reqs(&mut self) -> Vec<plx_screens::registry::ItemMenuReq> {
        std::mem::take(&mut self.item_menu_reqs)
    }

    /// Where the NEXT player instance returns to — see `AppMounter::player_origin`.
    pub(crate) fn seed_player_origin(&mut self, origin: plx_screens::player::Origin) {
        self.mounter.player_origin = Some(origin);
    }

    /// How long the NEXT player instance pins its transport for — see `AppMounter::player_hud_ms`.
    pub(crate) fn seed_player_hud(&mut self, ms: u32) {
        self.mounter.player_hud_ms = Some(ms);
    }

    /// **What a hold on the library page is a menu about**: the focused video, as the argument
    /// that presents the menu. `None` for a folder or nothing focused.
    pub(crate) fn content_menu_arg(&self, d: &Dispatcher<AppHost>, entry: EntryId, ret: &ReturnState<u32, PageMemory>)
        -> Option<plx_screens::registry::ItemMenuArg> {
        let e = d.nav.entry(entry)?;
        let real = &e.inst.as_ref()?.screen;
        let page = real.as_any()?.downcast_ref::<plx_screens::local::LocalScreen>()?;
        let (path, _size, mark) = page.focused_video(ret.focus)?;
        let parts = CxParts {
            tick: Tick { ms: 0, dt_us: 0 },
            press: plx_machine::machine::PressRead { scale: 1.0, is_long: false, held_ms: None, owner: None },
            focus: plx_machine::machine::FocusRead { current: ret.focus, ..Default::default() },
            owner: plx_machine::machine::InputOwner::Entry(entry),
        };
        let cx = parts.cx::<AppHost>(self.views(), &self.measure);
        let rect = real.focused_card_rect(&cx, ret.focus, At::Drawn);
        Some(plx_screens::registry::ItemMenuArg {
            path: path.to_string_lossy().into_owned(),
            kind: plx_screens::registry::MenuKind::Watch(mark),
            host: entry,
            focus: ret.focus,
            anchor: anchor_bits(rect),
        })
    }

    /// **A title page's track menu**, anchored beside the disc that asked for it. `None` while the
    /// file's tracks are still being read (the press then does nothing, and the next one opens it).
    pub(crate) fn track_menu_arg(
        &self,
        d: &Dispatcher<AppHost>,
        entry: EntryId,
        ret: &ReturnState<u32, PageMemory>,
        kind: plx_screens::registry::TrackKind,
    ) -> Option<plx_screens::registry::ItemMenuArg> {
        let e = d.nav.entry(entry)?;
        let real = &e.inst.as_ref()?.screen;
        let page = real.as_any()?.downcast_ref::<plx_screens::local_detail::LocalDetailScreen>()?;
        let (path, menu) = page.track_menu(kind)?;
        let parts = CxParts {
            tick: Tick { ms: 0, dt_us: 0 },
            press: plx_machine::machine::PressRead { scale: 1.0, is_long: false, held_ms: None, owner: None },
            focus: plx_machine::machine::FocusRead { current: ret.focus, ..Default::default() },
            owner: plx_machine::machine::InputOwner::Entry(entry),
        };
        let cx = parts.cx::<AppHost>(self.views(), &self.measure);
        let rect = real.focused_card_rect(&cx, ret.focus, At::Drawn);
        Some(plx_screens::registry::ItemMenuArg { path, kind: menu, host: entry, focus: ret.focus, anchor: anchor_bits(rect) })
    }

    /// **The opener LIFT: the focused tile repainted above the modal dim.** Render-only, and the
    /// pair it works from is the SURFACE's own argument (`ItemMenuScreen::opener`) — it was
    /// `Bridge::menu_opener`, a second copy of the same `(entry, focus)` kept on the side and
    /// written by four separate arms.
    ///
    /// It runs immediately after the container's page pass, which is where `ModalStack::draw_scrims`
    /// laid the dim down: the tile is the panel's whole subject, and the design's stated point is
    /// that the card stays visible behind it. The dim is the CONTAINER's now (`Screen::scrim`), so
    /// what is left here is the half only the page that drew the element can answer — a `fn()` lift
    /// has nothing to borrow a `&Dispatcher` through.
    ///
    /// Whether it runs at all is [`opener_lift`]'s answer, made once for every page: it stands down
    /// once the menu is dismissed and focus has moved off the opener element.
    pub(crate) fn redraw_opener(&self, d: &Dispatcher<AppHost>) {
        let Some((entry, focus)) = opener_lift(d) else { return };
        let Some(screen) = d.nav.entry(entry).and_then(|e| e.inst.as_ref()).map(|i| &i.screen) else { return };
        // The page pass may be submitting a cached host quad. Opener lifts are live paint
        // above that quad, like Popover::scrim_lifting's legacy callback scope.
        let _live = plx_ui::popover::host::live();
        let parts = opener_parts(d, entry, focus);
        let cx = parts.cx::<AppHost>(self.views(), &self.measure);
        let mut frame = DrawFrame::with_navigation(&cx, plx_ui::Painter::root(), self.navigation_presentation());
        screen.redraw_focused(&mut frame, focus);
    }

    /// Drive `popover`'s host-user counters from the surface phases (module doc).
    fn sync_host(&mut self, d: &Dispatcher<AppHost>) {
        let live: Vec<(EntryId, bool, bool)> = d
            .nav
            .modals
            .surfaces
            .iter()
            .filter(|s| s.phase != Phase::Hidden)
            .map(|s| {
                // DERIVED from the container's own policy table, never a second `matches!` over
                // `Style` — see `modal::style_caches_host`, which carries what the hand-written
                // list here cost `fps:library-switch` when `Style::Compact` was left off it.
                let cached = plx_ui::containers::modal::style_caches_host(s.style);
                (s.entry.id, cached, s.phase == Phase::Closing)
            })
            .collect();
        // released: held but no longer live
        let mut keep = Vec::new();
        for (id, cached, closing) in self.held.drain(..) {
            match live.iter().find(|(e, _, _)| *e == id) {
                None => plx_ui::popover::surface_released(cached, closing),
                Some((_, _, now_closing)) => {
                    if *now_closing && !closing {
                        plx_ui::popover::surface_closing(cached);
                    }
                    keep.push((id, cached, *now_closing));
                }
            }
        }
        for (id, cached, closing) in live {
            if !keep.iter().any(|(e, _, _)| *e == id) {
                plx_ui::popover::surface_held(cached);
                if closing {
                    plx_ui::popover::surface_closing(cached);
                }
                keep.push((id, cached, closing));
            }
        }
        self.held = keep;
    }
}

/// **Hand back every counter this bridge is still holding.** `held` is an OWNERSHIP list: each
/// entry means this bridge has called `popover::surface_held` and owes the matching
/// `surface_released`, and those counters are process-globals. In the app one `Bridge` lives as
/// long as the process; this is for the host suite, where every test builds its own.
impl Drop for Bridge {
    fn drop(&mut self) {
        for (_, cached, closing) in self.held.drain(..) {
            plx_ui::popover::surface_released(cached, closing);
        }
    }
}

impl Bridge {
    pub(crate) fn metadata_mut(&mut self) -> &mut plx_data::stores::metadata::MetadataStore {
        &mut self.stores.metadata
    }

    pub(crate) fn metadata_view(&self) -> plx_data::metadata::MetadataView<'_> {
        self.stores.metadata_view()
    }

    /// The video plane's EDGE, from `Player::set_video_plane_bound` and from nowhere else.
    pub(crate) fn publish_video_plane(&mut self, bound: bool) {
        self.video_plane = bound;
    }

    /// **Publish this frame's playback session** — see `AppViews::session`. Called once per
    /// iteration by the loop, BEFORE the dispatcher frame and the tree draw, so every screen in one
    /// frame reads one consistent picture of the playback. Off the player the publication is
    /// `PlaybackSession::IDLE` and stays there.
    pub(crate) fn publish_playback(&mut self, session: &plx_media::playback::PlaybackSession, live: bool) {
        if live {
            self.playback = session.publication();
            self.playback_live = true;
        } else if self.playback_live {
            self.playback = plx_media::playback::PlaybackSession::IDLE;
            self.playback_live = false;
        }
    }

    fn deliver_inner(&mut self, to: MachineId, msg: &AppMsg, _parts: &CxParts<u32>, _fx: &mut Effects<'_, AppHost>) -> Handled {
        let _ = (to, msg);
        Handled::No
    }

    fn store_cmd(&mut self, cmd: StoreCmd) {
        match cmd {
            StoreCmd::Metadata(command) => { self.stores.metadata_run(command); }
        }
    }

    fn app_effect(&mut self, from: MachineId, fx: AppFx) {
        match fx {
            AppFx::Store(_, cmd) => self.store_cmd(cmd),
            AppFx::Content(req) => self.content_reqs.push((from, req, self.effect_return.clone())),
            AppFx::Player(req) => self.player_reqs.push(req),
            AppFx::ItemMenu(req) => self.item_menu_reqs.push(req),
        }
    }
}

impl Rig<AppHost> for Bridge {
    fn draw_chrome(&mut self, _arg: &AppArg, _parts: &CxParts<u32>,
        nav: plx_ui::screen::NavPresentation,
        _glass: Option<&mut plx_ui::frame::glass::GlassPlan>) {
        self.tabs.draw(nav.chrome_alpha);
    }
    fn scrim_chrome_read(&self) -> Option<plx_ui::widgets::ChromeRead<'_>> {
        None
    }
    fn page_alpha(&self) -> f32 { plx_ui::nav::page_alpha() }
    fn navigation_presentation(&self) -> plx_ui::screen::NavPresentation {
        plx_ui::screen::NavPresentation {
            page_alpha: plx_ui::nav::page_alpha(),
            chrome_alpha: plx_ui::nav::chrome_alpha(),
            view_tab: u32::try_from(plx_ui::nav::view_tab(-1)).ok(),
            blur_amount: plx_ui::nav::blur_amount(),
        }
    }
    fn surface_scope(&mut self) -> Option<plx_ui::popover::host::Live> {
        Some(plx_ui::popover::host::live())
    }
    fn split(&mut self) -> Split<'_, AppHost> {
        Split {
            mounter: &mut self.mounter,
            views: AppViews {
                metadata: self.stores.metadata_view(),
                session: &self.playback,
            },
            measure: &self.measure,
        }
    }
    fn deliver(&mut self, to: MachineId, msg: &AppMsg, parts: &CxParts<u32>, fx: &mut Effects<'_, AppHost>) -> Handled {
        self.deliver_inner(to, msg, parts, fx)
    }
    fn timer(&mut self, _owner: MachineId, _id: TimerId, _parts: &CxParts<u32>, _fx: &mut Effects<'_, AppHost>) {}
    fn app_return(&mut self, _from: MachineId, ret: ReturnState<u32, PageMemory>) {
        self.effect_return = ret;
    }
    fn app_fx(&mut self, from: MachineId, fx: AppFx, _parts: &CxParts<u32>, _out: &mut Effects<'_, AppHost>) {
        self.app_effect(from, fx);
    }
    fn log(&mut self, line: &str) {
        plx_base::eventlog::log(line);
    }
    fn system_keyboard(&mut self, _up: bool) {}
    fn adopt_system_keyboard(&mut self) {}
    /// The render cache's upload step is the loop's (`adapters::poster::prepare`, in the window
    /// between its present decision and the draw), so this hook is empty.
    fn prepare(&mut self, _b: &mut Budget, _present: &mut Present) {}
    fn ls2_pump(&mut self) {}
    fn opaque_route(&mut self, video_plane_bound: bool) {
        super::run::rig_opaque_route(video_plane_bound);
    }
    /// At draw entry, and conditional on the video-plane bit: clearing the opaque region on a frame
    /// with no plane under it would silently retract the claim `opaque_route` had just made.
    fn clear_opaque_region(&mut self) {
        if self.video_plane {
            super::run::rig_clear_opaque_region();
        }
    }
    fn now_us(&self) -> u64 {
        (self.now_us)()
    }
    /// BACK at the library's root: the application has nowhere further back to go.
    fn back_at_root(&mut self) {}
}

// ---------------------------------------------------------------------------------------------
// the frame, and the loop's questions
// ---------------------------------------------------------------------------------------------

/// The dispatcher's frame, once per loop iteration: the pending navigation's own commit, the
/// stores' notices, then the ten steps WITHOUT the draw (the loop draws at its own slot, on its own
/// gate). Returns the top page's heartbeat word.
#[cfg(test)]
pub(crate) fn frame(
    d: &mut Dispatcher<AppHost>, rig: &mut Bridge,
    tick: Tick, inputs: Vec<InputEvent<u32>>,
) -> (&'static str, FrameReport) {
    frame_with_tap(d, rig, tick, inputs, &mut NoTap)
}

pub(crate) fn frame_with_tap(
    d: &mut Dispatcher<AppHost>,
    rig: &mut Bridge,
    tick: Tick,
    inputs: Vec<InputEvent<u32>>,
    tap: &mut dyn plx_ui::dispatch::Tap<AppHost>,
) -> (&'static str, FrameReport) {
    let _frame_scope = plx_base::task::FrameScope::enter();
    plx_base::storage_worker::pump_retained();
    for (id, gen) in rig.stores.take_notices() {
        d.store_changed(id.ord(), gen);
    }
    let surface = d.surface_up();
    let report = plx_base::diag::spans::span("dframe", || d.frame_with(rig, tick, inputs, Vec::new(), tap, false));
    d.prune(&report.unmounted);
    plx_base::diag::spans::span("dsync", || rig.sync_host(d));
    // A frame the video plane alone presented is not reported as damage: the loop's gate presents
    // it on its own video-plane term anyway, and the report would keep the settled-capture clock
    // (`plx_machine::idle::last_change_ms`) from ever seeing a paused player at rest.
    if report.presented && !report.video_only {
        // While a surface is up this bump is the PANEL's — `take_page_damage` subtracts the
        // panel's claims BY COUNT.
        let _own = surface.then(plx_machine::idle::OwnScope::open);
        plx_machine::idle::invalidate();
    }
    // **Publish the transition's presentation** for the readers that draw outside a page's own
    // `DrawFrame` (`ui::popover`'s panel and scrim).
    plx_ui::nav::publish(d.nav.tabs.stack.page_alpha(), d.nav.tabs.stack.chrome_alpha(), None);
    // **The heartbeat's `route=` word IS the top page's own name.**
    let word = d.top_screen().map_or("", |s| s.name());
    (word, report)
}

/// **Open one of the player's panels on the PLAYER PAGE's own `ModalStack`** (§6.2).
///
/// Idempotent per KIND while that kind is up, for `open_settings`'s reason: a second press on the
/// disc that opened it must not stack a second copy. Opening a DIFFERENT kind over an open one is
/// allowed and is what the transport's own tab row does — the container's `input_owner()` gives
/// the topmost surface the keys, so the stack is the answer to "which panel owns the frame".
///
/// The style is `PlayerPanel { survives_failure }`, whose host policy is `(Live, Live)`: what is
/// behind these panels is a hardware video plane GL cannot read back, so there is no host snapshot
/// to take and nothing to freeze (`ui/popover.rs`'s `HostPolicy::Live` says exactly this about the
/// player route). No player panel survives a failure: the read-out owns the frame.
pub(crate) fn open_player_overlay(
    meta: plx_data::metadata::MetadataView<'_>,
    d: &mut Dispatcher<AppHost>,
    kind: plx_screens::player::overlay::OverlayKind,
) {
    // Already up? Then this press is a re-ADDRESS of the entry that exists — the Audio disc
    // pressed while the Subtitles tab is showing — and never a second surface of the same kind.
    if player_overlay_kind(d).is_some_and(|up| up.slot() == kind.slot()) {
        if let Some(surface) = player_overlay_mut(d) {
            surface.retarget(meta, kind);
        }
        return;
    }
    d.nav.next_style = Style::PlayerPanel { survives_failure: false };
    d.request(
        MachineId::Nav,
        NavOp::Present(AppArg::PlayerOverlay(
            plx_screens::player::overlay::PlayerOverlayArg { kind },
        )),
    );
}

/// Which player panel owns input right now, if one does.
pub(crate) fn player_overlay_kind(
    d: &Dispatcher<AppHost>,
) -> Option<plx_screens::player::overlay::OverlayKind> {
    let InputOwner::Entry(id) = d.nav.input_owner()? else { return None };
    match &d.nav.entry(id)?.arg {
        AppArg::PlayerOverlay(arg) => Some(arg.kind),
        _ => None,
    }
}


/// **This frame's [`HudPolicy`] for the transport** (plan `subtitle-menu-capsule` §4): scans every
/// surface whose phase is not `Hidden` — Opening, Open, AND Closing. `input_owner()`
/// (`ModalStack::input_owner`) is deliberately NOT used here: it excludes `Closing`, and a
/// Tracks→Timing hand-off dismisses the Tracks surface (which starts Closing the same frame the
/// fresh Timing surface starts Opening) — reading only the input owner would see Timing alone and
/// miss that Tracks' own closing fade must not flash the transport either.
///
/// [`HudPolicy::Hidden`] if ANY such player-overlay surface's kind says so
/// (`OverlayKind::hud_policy` — today only `OverlayKind::Timing`); otherwise
/// [`HudPolicy::Lifted`] if ANY modal surface is up, a player panel or not (subtitles clear the
/// transport while something is being read over them — the answer the loop read from
/// `Dispatcher::surface_up` before the capsule existed); otherwise [`HudPolicy::Normal`]. So a
/// Tracks(Closing) + Timing(Opening) pair reads `Hidden`, never lifting the captions AND hiding
/// the transport in one frame.
pub(crate) fn player_hud_policy(d: &Dispatcher<AppHost>) -> HudPolicy {
    let hides = |arg: &AppArg| matches!(arg, AppArg::PlayerOverlay(p) if p.kind.hud_policy() == HudPolicy::Hidden);
    if surface_up(d, hides) {
        HudPolicy::Hidden
    } else if d.surface_up() {
        HudPolicy::Lifted
    } else {
        HudPolicy::Normal
    }
}


/// Dismiss every player panel — the exit ritual's half of `close_player_overlays`.
pub(crate) fn dismiss_player_overlays(d: &mut Dispatcher<AppHost>) {
    let ids: Vec<EntryId> = d
        .nav
        .modals
        .surfaces
        .iter()
        .filter(|s| matches!(s.entry.arg, AppArg::PlayerOverlay(_)))
        .map(|s| s.entry.id)
        .collect();
    for id in ids {
        d.request(MachineId::Nav, NavOp::Dismiss(id));
    }
}

/// A live panel's own state, for the dev triggers that drive one by hand
/// (`pitv-menupick`) and for the focus probe's `sel=`.
pub(crate) fn player_overlay_mut(
    d: &mut Dispatcher<AppHost>,
) -> Option<&mut plx_screens::player::overlay::PlayerOverlayScreen> {
    let InputOwner::Entry(id) = d.nav.input_owner()? else { return None };
    d.nav
        .entry_mut(id)?
        .inst
        .as_mut()?
        .screen
        .as_any_mut()?
        .downcast_mut::<plx_screens::player::overlay::PlayerOverlayScreen>()
}

/// A rect as the bit-preserving anchor an argument carries, so a canonical argument needs no float
/// equality. `None` (a host with nothing focused) resolves to the panel's own centred fallback
/// HERE, once, rather than every frame inside the screen.
fn anchor_bits(rect: Option<plx_ui::Rect>) -> [u32; 4] {
    let r = rect.unwrap_or_else(plx_screens::item_menu::fallback_anchor);
    [r.x.to_bits(), r.y.to_bits(), r.w.to_bits(), r.h.to_bits()]
}

/// **What the opener lift reads of the press: the dispatcher's own, live.** The lift is the page's
/// focused card drawn a second time above the dim. A card a card section left out of the page pass
/// (`DrawFrame::lifted`) is drawn by the lift ALONE, at this scale. An opener no card section draws
/// (the Detail episode strip, season tabs, hero buttons, the Home hero) is still drawn by the page
/// at the press scale (`DrawFrame.press`, from `Dispatcher::parts`), so there the lift must be
/// handed the SAME number or the two disagree on every frame the press is moving. They did, while
/// this was a constant `1.0`: a taken hold's card was drawn at its pop by the lift and at its dip
/// by the page and the host snapshot, and the owner saw a poster that popped in one frame with its
/// title doubled.
///
/// Only the scale is the lift's business; a hold in flight or the hint's elapsed time is not (the
/// lift draws one card, not the page's hold hint).
pub(crate) fn opener_press(d: &Dispatcher<AppHost>) -> plx_machine::machine::PressRead<u32> {
    plx_machine::machine::PressRead { scale: d.input.press.scale(), is_long: false, held_ms: None, owner: d.input.dip_owner }
}

/// **Which element, if any, the opener lift repaints this frame** — the ONE decision every page
/// inherits, so no screen's `redraw_focused` has to defend against a stale key.
///
/// The lift is the opener tile drawn again above the dim, *because it is the focused one*. That
/// stops being true the moment the menu is dismissed: `ModalStack::dismiss` hands input back to the
/// page on the same frame, but the surface stays in the stack until its fade settles, so a D-pad
/// press during the fade moves the page's focus to a neighbour while this lift would still paint
/// the OLD tile fully popped and captioned (two tiles look focused at once; the new one's pop
/// reads late). So while the surface is `Closing`, the lift is painted only if the engine's focus
/// on the host page is still the opener element (or there is none to compare); once focus has
/// moved, the page draws the tiles itself and the lift stands down. While the menu is open the
/// engine's focus is on the menu's own rows, so the lift always paints then.
pub(crate) fn opener_lift(d: &Dispatcher<AppHost>) -> Option<(EntryId, Option<FocusKey<u32>>)> {
    d.lifted_opener()
}

/// **The context the opener lift draws with**: the page's own pieces for `entry`'s focused element,
/// with the press taken from [`opener_press`] and nothing else standing in for it. `redraw_opener`
/// builds its whole `Cx` from this, so the scale the lift draws at is observable
/// (`parts.press.scale`) without a renderer.
pub(crate) fn opener_parts(
    d: &Dispatcher<AppHost>,
    entry: EntryId,
    focus: Option<plx_machine::machine::FocusKey<u32>>,
) -> CxParts<u32> {
    CxParts {
        tick: Tick { ms: 0, dt_us: 0 },
        press: opener_press(d),
        focus: plx_machine::machine::FocusRead { current: focus, ..Default::default() },
        owner: plx_machine::machine::InputOwner::Entry(entry),
    }
}

/// **Present the item context menu** over the page the hold happened on (idempotent while one is
/// up, for `open_settings`'s reason: a second hold must not stack a second panel).
///
/// **It does not touch the press.** A hold the screen answered was already abandoned onto its
/// release spring by the dispatcher (`InputMachine::cancel_press`), so the card springs from its dip
/// up to its pop as the menu opens, and the press is the ONE owner of that motion: the opener
/// lift ([`opener_press`]) draws it live above the frozen page (and, for an opener no card section
/// draws, the page draws it too). The page pass leaves
/// the lifted card out (`DrawFrame::lifted`), so the snapshot holds no copy of it to go stale and
/// the spring is not page damage (`Press::tick`); only an opener no card section draws keeps the
/// per-frame re-take. Ending the press at rest here instead (`Press::settle`, rc.3) made the
/// card jump to its pop in one frame, the lost animation the owner reported.
///
/// The style is `Compact`, whose host policy is `(Frozen, Cached)`: the page under the panel is
/// drawn once into the shared snapshot and served from it, and its focus springs do not advance
/// while the menu owns input. That was `Popover::caching_host()` plus the loop's own
/// `Route::ItemMenu` draw/update arms — one policy stated in three places — and it is the
/// container's single answer now.
pub(crate) fn open_item_menu(d: &mut Dispatcher<AppHost>, arg: plx_screens::registry::ItemMenuArg) {
    if surface_up(d, |a| matches!(a, AppArg::ItemMenu(_))) {
        return;
    }
    d.nav.next_style = Style::Compact;
    d.request(MachineId::Nav, NavOp::Present(AppArg::ItemMenu(arg)));
}



// ---------------------------------------------------------------------------------------------
// the loop's navigations, as container ops
// ---------------------------------------------------------------------------------------------

/// **Replace the WHOLE stack with `arg`** (`NavOp::Root`): every entry leaves for good and `arg` is
/// minted as the sole survivor.
pub(crate) fn nav_root(d: &mut Dispatcher<AppHost>, arg: AppArg) {
    d.request(MachineId::Nav, NavOp::Root(arg));
}


/// **Put `want` on top, reusing an entry that already holds it** — a test's way to stand on a
/// page without stacking a second copy of it.
#[cfg(test)]
pub(crate) fn show_page(d: &mut Dispatcher<AppHost>, want: AppArg) {
    use plx_ui::screen::ScreenArg;
    if d.nav.top_page().map(|e| e.arg.same_instance(&want)).unwrap_or(false) { return; }
    let existing = d.nav.tabs.stack.entries.iter().rev()
        .find(|e| e.arg.same_instance(&want)).map(|e| e.id);
    if let Some(id) = existing {
        nav_pop_to(d, id);
    } else if d.nav.top_page().is_none() {
        nav_root(d, want);
    } else {
        nav_push(d, want);
    }
}

/// **Stack a page** — a folder, the player.
pub(crate) fn nav_push(d: &mut Dispatcher<AppHost>, arg: AppArg) {
    d.request(MachineId::Nav, NavOp::Push(arg));
}

/// …with the outgoing page's `ReturnState` supplied rather than read off the engine, for a request
/// an owned screen froze on ITS press frame.
pub(crate) fn nav_push_with_return(
    d: &mut Dispatcher<AppHost>, arg: AppArg, ret: ReturnState<u32, PageMemory>,
) {
    d.request_with_return(MachineId::Nav, NavOp::Push(arg), ret);
}


pub(crate) fn nav_pop_with_return(d: &mut Dispatcher<AppHost>, ret: ReturnState<u32, PageMemory>) {
    d.request_with_return(MachineId::Nav, NavOp::Pop, ret);
}

/// **Back to a NAMED entry** — the player's exit (the origin is an `EntryId`). `PopTo` is a no-op
/// for an entry that is no longer on the stack, and a player whose origin has gone would then have
/// no way off the screen at all — so an absent origin falls back to the library's root.
pub(crate) fn nav_pop_to(d: &mut Dispatcher<AppHost>, entry: EntryId) {
    if d.nav.tabs.stack.entries.iter().any(|e| e.id == entry) {
        d.request(MachineId::Nav, NavOp::PopTo(entry));
    } else {
        d.request(MachineId::Nav, NavOp::SelectTab(AppArg::Home));
    }
}

/// **Withdraw a transition that has not committed**, iff the page that asked for it is still the
/// top. Returns whether there was one, so an input that cancelled NOTHING falls through to its
/// normal handling instead of being swallowed.
pub(crate) fn nav_cancel(d: &mut Dispatcher<AppHost>) -> bool {
    let Some(top) = d.nav.top_page().map(|e| e.id) else { return false };
    d.nav.tabs.stack.cancel(top)
}

/// **The OS took the screen**: the tree is PARKED, and the page stack does not move.
pub(crate) fn background(d: &mut Dispatcher<AppHost>) {
    d.suspend();
}

/// **…and gave it back.** Idempotent, which is why the loop calls it on both edges.
pub(crate) fn foreground(d: &mut Dispatcher<AppHost>) {
    d.resume();
}


/// Is a surface matching `which` on screen in ANY phase — a dismissal that is still fading
/// included? That is the conservative reading every caller wants: `open_*` must not present a
/// second surface over one that is on its way out.
fn surface_up(d: &Dispatcher<AppHost>, which: impl Fn(&AppArg) -> bool) -> bool {
    d.nav
        .modals
        .surfaces
        .iter()
        .any(|s| which(&s.entry.arg) && s.phase != Phase::Hidden)
}

/// **The player instance, if one is mounted.** `None` means there is no playback on screen. An
/// overlay presented on the player's page-owned `ModalStack` is a SURFACE, so the player stays the
/// top PAGE and this keeps answering with it while a panel is up.
pub(crate) fn player(d: &Dispatcher<AppHost>) -> Option<&plx_screens::player::PlayerScreen> {
    d.nav
        .top_page()?
        .inst
        .as_ref()?
        .screen
        .as_any()?
        .downcast_ref::<plx_screens::player::PlayerScreen>()
}

pub(crate) fn player_mut(
    d: &mut Dispatcher<AppHost>,
) -> Option<&mut plx_screens::player::PlayerScreen> {
    let entry = d.nav.top_page()?.id;
    d.nav
        .entry_mut(entry)?
        .inst
        .as_mut()?
        .screen
        .as_any_mut()?
        .downcast_mut::<plx_screens::player::PlayerScreen>()
}

/// **The top page answers its own keys** — the engine owns the input for it.
///
/// The `route: Route` parameter these three took is gone with the fold (D1): `owns_input` already
/// ignored its argument outright, and the other two used theirs only to re-assert what the
/// container is the authority for — that the top page is the committed route, which
/// `frame_with_results` asserts every frame anyway. Every one of them was already reading the top
/// entry.
pub(crate) fn page_owned(d: &Dispatcher<AppHost>) -> bool {
    d.top_screen().map_or(false, |s| s.focus_source() == FocusSource::Engine)
}

pub(crate) fn owns_input(d: &Dispatcher<AppHost>) -> bool {
    d.surface_up() || d.owns_input()
}


/// The host page is REPLACED: the loop skips drawing it.
pub(crate) fn host_replaced(d: &Dispatcher<AppHost>) -> bool {
    d.host_policy().1 == HostRender::Replaced
}

/// What the loop's PAGE half draws this frame, from the host fold and who owns the top page.
///
/// §8.3: a Replaced host "receives nothing" — and that includes its cached quad. Through phase 7
/// the loop's guard was `!host_replaced || page_owned`, which was right while the only owned
/// pages (first-run Favourites, Login, Profiles) could not host an opaque surface: `page_owned`
/// there meant "the dispatcher draws the page, not the closure", and `host_replaced` never held
/// on those routes. Phase 8 made Home an owned page AND the host of Settings, so both were true
/// at once and the page closure ran under the opaque ground on every frame — where
/// `popover::host::page_pass` served the frozen page snapshot as one full-screen `Class::Image`
/// quad, a second full-screen pass beneath the `RouteGround`'s own ambient wash. The draw-mask
/// census priced that quad at the whole regression: `settings-root` 40 fps unmasked, 60 with
/// either class masked (TV session 4, 2026-09-09). A Replaced host is drawn by nobody.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PagePlan {
    /// The closure draws the page and, through `Dispatcher::draw(.., true)`, the surfaces.
    Owned,
    /// The closure draws the legacy fallback; the loop draws the surfaces after it.
    LegacyThenSurfaces,
    /// The host is replaced: no page pass, no cached quad — the surfaces alone.
    SurfacesOnly,
}

pub(crate) fn page_plan(host_replaced: bool, page_owned: bool) -> PagePlan {
    match (host_replaced, page_owned) {
        (true, _) => PagePlan::SurfacesOnly,
        (false, true) => PagePlan::Owned,
        (false, false) => PagePlan::LegacyThenSurfaces,
    }
}

/// **The heartbeat's ` overlay=` word IS the topmost surface's own `Screen::name`.**
pub(crate) fn overlay_word(d: &Dispatcher<AppHost>) -> Option<&'static str> {
    d.top_surface_name()
}

/// …and the ` overlay=` word each one PRESENTS as, read back through [`overlay_word`] on a real
/// tree. The player's panels are presented over the Player page, the rest over the library.
#[cfg(test)]
pub(crate) fn every_surface_word() -> Vec<&'static str> {
    let mut out = Vec::new();
    for arg in plx_screens::registry::every_surface_arg() {
        let route = if matches!(arg, AppArg::PlayerOverlay(_)) {
            AppArg::Player
        } else {
            AppArg::Local(plx_screens::registry::LocalArg { path: String::new() })
        };
        let mut d = Dispatcher::<AppHost>::new();
        let mut rig = Bridge::for_test(|| 0);
        show_page(&mut d, route.clone());
        frame(&mut d, &mut rig, Tick { ms: 0, dt_us: 16_000 }, vec![]);
        d.nav.next_style = Style::Compact;
        d.request(MachineId::Nav, NavOp::Present(arg));
        frame(&mut d, &mut rig, Tick { ms: 16, dt_us: 16_000 }, vec![]);
        out.push(overlay_word(&d).expect("a presented surface names a word"));
    }
    out
}

/// A key for the dispatcher: the raw SDL fields classified once (`ui::consts::classify`), the
/// fork's packed state read as an edge.
pub(crate) fn key_input(sym: u32, wcode: u32, state: u32, now: Tick, source: Source) -> InputEvent<u32> {
    use plx_ui::consts::{classify, Key as K};
    let key = match classify(sym, wcode) {
        K::Up => Key::Up,
        K::Down => Key::Down,
        K::Left { .. } => Key::Left,
        K::Right { .. } => Key::Right,
        K::Ok => Key::Ok,
        K::Back => Key::Back,
        _ => Key::Other,
    };
    let edge = if (state & 0xff) != 1 {
        Edge::Up
    } else if state & 0x100 != 0 {
        Edge::Repeat
    } else {
        Edge::Down
    };
    InputEvent {
        at: now,
        source,
        kind: InputKind::Key {
            key,
            sym,
            wcode,
            edge,
            at_edge: false,
        },
    }
}

pub(crate) fn pointer_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Pointer { x, y, hit: None },
    }
}

pub(crate) fn click_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Click { x, y, hit: None },
    }
}

/// **Pointer motion with a button held down** (§7.5, restructure phase 12) — the third pointer
/// kind, and the one that drives a CONTROL rather than focus: `ui::hit` resolves the hit and then
/// deliberately raises neither hover nor activation for it, so the only thing a drag can do is
/// what the receiving screen makes of it. Its one consumer today is the player's scrub bar, whose
/// preview follows the pointer between the click that seated the gesture and the button-up that
/// commits it.
pub(crate) fn drag_input(x: f32, y: f32, now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Drag { x, y, hit: None },
    }
}

/// A wheel tick as the key it stands for on a vertical flow (the family's `on_updown`).
pub(crate) fn wheel_input(dy: i32, now: Tick) -> Vec<InputEvent<u32>> {
    let key = if dy < 0 { Key::Down } else { Key::Up };
    let (sym, wcode) = (0, 0);
    vec![
        InputEvent {
            at: now,
            source: Source::Sdl,
            kind: InputKind::Key { key, sym, wcode, edge: Edge::Down, at_edge: false },
        },
        InputEvent {
            at: now,
            source: Source::Sdl,
            kind: InputKind::Key { key, sym, wcode, edge: Edge::Up, at_edge: false },
        },
    ]
}

/// **A pointer button-UP as the press machine's release.** The dispatcher's `InputKind` has no
/// release of its own: `InputMachine::release` is reached from ONE place, an `Ok` key on the `Up`
/// edge (`dispatch`'s ingest), whatever armed the press. So a click that armed a control face is
/// released by handing the tree that edge — and it is inert everywhere else, because every screen
/// in the family matches `Edge::Down` (or `!= Edge::Up`) and the engine's own half skips `Up`
/// outright. Without it a mouse press would sit dipped until `press::MAX_HOLD_MS` (1 s) committed
/// it, which on the simulator reads as a UI that answers a click a second late.
pub(crate) fn release_input(now: Tick) -> InputEvent<u32> {
    InputEvent {
        at: now,
        source: Source::Sdl,
        kind: InputKind::Key {
            key: Key::Ok,
            sym: 0,
            wcode: 0,
            edge: Edge::Up,
            at_edge: false,
        },
    }
}



#[allow(dead_code)]
fn _measure_is_object_safe(m: &dyn Measure, s: &CStr) -> f32 {
    m.width(s, 1, false)
}
