//! **The application bundle's screen-side half**: the effects a screen may ask for (`AppFx`), the
//! messages a machine receives (`AppMsg`), the requests an owned screen makes of the loop, the
//! screen argument ([`AppArg`]) and the one `mount` match ([`AppMounter`]).
//!
//! A new screen touches its own `screens/<name>.rs` and this file: the variant, the id, the
//! `mount` arm and the recorded shape ([`SCREEN_SHAPES`]) are all here.

use plx_machine::machine::{
    Canon, Chrome, Cx, Effects, EntryId, Host, InstanceId, LogicalState, ScreenId,
};
use plx_ui::screen::{Mounter, ReturnState, Screen};

/// The application's effects.
pub enum AppFx {
    /// One live player setting's pick (`plx_media::route::Choice`): the app writes it through the
    /// media door, and the sender learns whether the write was durable.
    Preferences(plx_media::route::Choice, std::sync::mpsc::Sender<bool>),
    /// Library-page requests, executed by the navigation bridge.
    Content(ContentReq),
    /// Player-route requests — what an overlay panel on the player's own `ModalStack` asks of the
    /// loop. Executed by `app/playback.rs`'s drain.
    Player(PlayerReq),
    /// The item context menu's committed row — see [`ItemMenuReq`].
    ItemMenu(ItemMenuReq),
    /// A store command, executed as a `Deliver` to the store machine in the same drain.
    Store(plx_data::stores::StoreId, plx_data::stores::StoreCmd),
    /// Poll only the store work this visible route owns, after its read-only step returns.
    StoreWork(plx_data::stores::StoreWork),
}

/// **What the item context menu asks of the loop**, once its own `step` has resolved the pressed
/// row to an [`crate::item_menu::Action`]. The panel owns its rows, cursor, anchor and dismissal;
/// the loop performs what an action does (play, mark watched), which a screen may not.
pub struct ItemMenuReq {
    pub act: crate::item_menu::Action,
}

/// **What a player overlay asks for**, once its own `step` has decided.
///
/// Seeking, pausing and leaving the player need the `MainThread` token and the container ops
/// `app::bridge` owns, neither of which a screen may name. So the panel decides and the loop
/// performs.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerReq {
    /// Accepted only after the owned player confirmation. Never emitted by boot or Play.
    RepairSandbox,
    /// The failure read-out's fix for Force Direct Play: set the Direct Play preference to Auto
    /// and resolve the failed item again under it.
    PlayAutomatically,
    /// The failure read-out's *Try again*: resolve the failed item again, unchanged, at the
    /// position the viewer was at (`app::playback::retry_failed_playback`).
    RetryPlayback,
    /// A transport key FELL THROUGH the panel (§ `overlay_transport_key_tests`): a viewer holding
    /// the track menu, the Info card or the Chapters strip open still expects PAUSE/PLAY to work,
    /// and the panel stays up. `true` = the key was PLAY, `false` = PAUSE; a PLAYPAUSE toggle is
    /// neither and is carried as `None`.
    Transport(Option<bool>),
    /// Seek to this position in ns and resume if paused — the Chapters strip's OK.
    SeekTo(i64),
    /// **Commit a SCRUB to this position, and leave a paused film paused** (restructure phase 12,
    /// PX-PLAYER) — `app::playback::commit_seek`, the other half of the pair above.
    ///
    /// The difference is the reason there are two. [`SeekTo`](PlayerReq::SeekTo) RESUMES, because
    /// a viewer who picked a chapter asked to watch it. A scrub does not: the transport's bar is
    /// how a PAUSED film is moved, and starting it there is a behaviour nobody asked for. Holding
    /// the pause across the seek needs three pieces of state that are the loop's and no screen's —
    /// `App::repause_at`, `TX.resume_pend` and the bounded seek-preroll feed override that lets
    /// the pipeline decode the landed frame without publishing a viewer Resume — which is why this
    /// is a request rather than something `PlayerScreen` performs.
    CommitSeek(i64),
    /// Apply the `…` popover's chosen row.
    More(plx_appkit::more_menu::Action),
    /// Apply the Info card's focused action.
    Info(plx_appkit::info_panel::InfoAction),
    /// The panel took a DOWN past its own bottom: drop the HUD's ring onto the tabs row.
    FocusTabs,
    /// Keep the transport alive while a panel is being read (`HUD_MENU_MS`), or hand it the
    /// ordinary linger as a panel closes (`HUD_LINGER_MS`).
    ExtendHud(u32),
    /// The Info card's OK landed on a control FACE, which has a press dip of its own: arm the
    /// tvOS press and commit on the spring-back rather than acting now.
    ArmInfoPress,
    /// The track menu picked a row. `route::commit_audio_selection`/`commit_subtitle_selection`
    /// take the playback session's `&mut`, which a screen never has (§2.2) — so the panel decides
    /// and the loop performs, exactly as every other request in this enum.
    CommitTrack(plx_appkit::track_menu::TrackCommit),
    /// **Present one of the player's overlays directly** (restructure phase 12) — the tabs row's OK
    /// (`OverlayKind::Info`/`::Chapters`, the old `key_ok`'s `focus == 2` arm) and the failure
    /// read-out's own recovery escape (`OverlayKind::More { quality: true }`, the old
    /// `key_player_failed`'s `ChooseQuality` arm). Both used to reach
    /// `super::bridge::open_player_overlay` straight from the loop's key ladder; `PlayerScreen`
    /// may not name that function itself (§2.2), so it asks instead.
    OpenOverlay(crate::player::overlay::OverlayKind),
    /// **OK landed on the transport's own control row** — whichever disc, the Skip pill or the Up
    /// Next tile currently occupies it (restructure phase 12, the old `key_ok`'s `focus == 1`
    /// arm). Arms the same tvOS press dip [`ArmInfoPress`](PlayerReq::ArmInfoPress) does; the
    /// loop's existing commit-frame dispatch (`app/run.rs`'s `Route::Player =>
    /// activate_player_row(...)`) is unchanged and performs whatever the row decides once the
    /// spring-back has played.
    ArmControlRow,
    /// **Leave the player** — the one ritual the STOP key, a BACK with nothing else open, and the
    /// failure read-out's own BACK escape all perform (`exit_player`; restructure phase 12,
    /// replacing `app/run.rs`'s direct calls to it from the player's own key arms).
    Exit,
}

/// **What the item context menu is about**, captured at the moment it is presented and never
/// re-resolved (`screens::item_menu`).
#[derive(Clone, PartialEq, Eq)]
pub struct ItemMenuArg {
    /// The file the menu is about.
    pub path: String,
    /// Its watch state, as the library page drew it.
    pub mark: plx_ui::widgets::PosterMark,
    /// The PAGE entry the menu hangs off: its focused element is the anchor the panel sits beside
    /// and the tile lifted back out of the modal dim (`app::bridge::redraw_opener`).
    pub host: plx_machine::machine::EntryId,
    /// …and which element that is, as the host page's own focus at the press frame.
    pub focus: Option<plx_machine::machine::FocusKey<u32>>,
    /// The focused tile's drawn rect, bit-preserving so a canonical argument needs no float
    /// equality. The presenter resolves the centred fallback (`item_menu::fallback_anchor`) before
    /// storing it, so this is always a real rect.
    pub anchor: [u32; 4],
}

impl plx_machine::machine::LogicalState for ItemMenuArg {
    fn write(&self, c: &mut plx_machine::machine::Canon) {
        c.str(&self.path).u32(self.mark as u32).u32(self.host.0);
        c.option(self.focus, |c, key| { c.u32(key.entry.0).u32(key.elem); });
        for value in self.anchor { c.u32(value); }
    }
    fn probe(&self, out: &mut String) { out.push_str("item_menu_arg"); }
}

/// What a page leaves behind on its return state. The library page keeps nothing beyond the
/// engine's own focus memory.
#[derive(Clone, Debug, Default)]
pub enum PageMemory {
    #[default]
    None,
}

impl PageMemory {
    pub fn shed_to_identity(&mut self, _focus: Option<u32>) {}
}

impl plx_machine::machine::LogicalState for PageMemory {
    fn write(&self, c: &mut plx_machine::machine::Canon) {
        match self {
            Self::None => { c.u32(0); }
        }
    }
    fn probe(&self, out: &mut String) { out.push_str("page_memory"); }
}

pub const PAGE_MEMORY_SHAPE: &str = "PageMemory{None}";

/// Effects cross the screen/loop boundary; screens do not poll one another's pending latches.
pub enum ContentReq {
    /// Open a folder as a page of its own, so BACK returns here.
    LocalOpen(LocalArg),
    /// Play a file on this machine's disk. The loop probes it and starts the engine, which needs
    /// the playback session a screen may not name.
    LocalPlay(String),
    Back,
    ItemMenu,
}

pub trait ContentLike: AppLike<Memory = PageMemory> {}
impl<H: AppLike<Memory = PageMemory>> ContentLike for H {}

/// A host publishing the metadata read surface for this frame (the playing item's title, chapters
/// and streams).
pub trait MetadataLike: AppLike + Sized {
    fn metadata<'a>(cx: &Cx<'a, Self>) -> plx_data::metadata::MetadataView<'a>;
    /// The subtitle search the player's Search page reads. PROVIDED, defaulting to `None` — a host
    /// that offers no search shows no Search row.
    fn subtitle_search<'a>(_cx: &Cx<'a, Self>) -> Option<plx_data::subsearch::SubSearchView<'a>> {
        None
    }
}

/// A host that publishes this frame's playback session. The player's owned screens read the
/// Player machine's decisions through it; a change is asked for as an effect (`AppFx::Player`),
/// because the publication is a copy and there is no `&mut` on this path by construction.
pub trait PlayerLike: AppLike<Memory = PageMemory> + Sized {
    fn session<'a>(cx: &Cx<'a, Self>) -> &'a plx_media::route::PlaybackSession;
}

/// The application's messages.
pub enum AppMsg {}

/// Any host that carries this bundle.
pub trait AppLike: Host<Elem = u32, Fx = AppFx, Msg = AppMsg> {}
impl<H: Host<Elem = u32, Fx = AppFx, Msg = AppMsg>> AppLike for H {}

/// The heartbeat words an owned screen can name: the top screen's own `Screen::name`.
pub mod word {
    /// The item context menu (`screens::item_menu`).
    pub const ITEM_MENU: &str = "itemmenu";
    /// The local library's folder page (`screens::local`).
    pub const LOCAL: &str = "local";
}

/// An element key for a route-family screen: table rows are their index; the action band's
/// controls sit above [`BAND`], so one `u32` namespace serves both groups of a screen.
///
/// **This number is repeated, not shared, and the repeat is `ui::table_screen::BAND_BASE`.**
/// `table_screen.rs` is a LIBRARY module and cannot name `screens::registry` (the layer rule:
/// `ui/` never names `screens/`), so the one place that actually MINTS a band element
/// (`BandPart::key`) carries its own copy of this literal with a comment pointing back here. The
/// assertion below is what keeps that a documented duplication rather than a silent one: if a
/// future edit moves this constant without moving its twin, `band_index`/`alert_index` would
/// misresolve every control in the family's action row (Privacy & data's Share/Don't Share, every
/// screen's Done/Try again) the next time anyone TYPED the mismatch, rather than the next time
/// anyone ran the app on a television.
pub const BAND: u32 = 0x4000_0000;
const _: () = assert!(BAND >= plx_ui::geom::NAMESPACE_FLOOR);
/// The decision alert's two answers (Cancel, Delete), above the band.
pub const ALERT: u32 = 0x4000_0100;

const _: () = assert!(
    BAND == plx_ui::table_screen::BAND_BASE,
    "screens::registry::BAND and ui::table_screen::BAND_BASE are the same address in two crates \
     that cannot import from each other; keep them numerically identical"
);

/// The inverse of [`band_index`]: where a family screen's Nth band control lives in the shared
/// `u32` namespace. Currently unused by any screen — every band element in the tree today is
/// minted by `table_screen::BandPart::key` (which carries its own copy of the same arithmetic,
/// for the layer reason on [`BAND`]'s doc) — kept here as the one place that STATES the forward
/// direction, and pinned by a round-trip test against `band_index` so the two cannot drift apart
/// silently if a future screen starts calling it directly instead of `BandPart`.
///
/// **`#[allow(dead_code)]` because its only callers are test modules** — this one's round-trip
/// test and `screens::onboard`'s, which mints the band key it presses through here rather than
/// writing `BAND + 0` out by hand. A `cfg(test)`-only caller is invisible to `dead_code` in the
/// build that ships, so `-D warnings` fails `--no-default-features` on it; the same reason
/// `screens::onboard::probe_fields` carries one. Deleting the function instead would delete the
/// only STATEMENT of the forward direction in this crate and leave `table_screen::BandPart::key`'s
/// copy of the arithmetic unpaired, which is the drift the const assert above exists to prevent.
#[allow(dead_code)]
pub fn band_elem(i: usize) -> u32 {
    BAND + i as u32
}
/// **`then`, not `then_some`, and the difference is a panic.** `bool::then_some` takes its value
/// by VALUE, so the subtraction is evaluated whatever the condition says — and every ordinary
/// table row is an `elem` far BELOW `BAND`, so `elem - BAND` underflows. The dev profile has
/// overflow checks on, so that is an outright panic on the commonest input this function has
/// ("attempt to subtract with overflow"), reached from any screen in the family that asks whether
/// the focused row is a band control. `then` takes a closure and so runs the arithmetic only on
/// the branch that already proved it cannot underflow.
///
/// A release build would not have panicked, which is what makes this worth a comment rather than
/// a silent edit: overflow wraps there, the guard is still `false`, and the function still answers
/// `None`. So the bug was invisible on the television and fatal in `make check` — the reverse of
/// the usual direction, and not something to re-derive from the diff.
pub fn band_index(elem: u32) -> Option<usize> {
    (elem >= BAND && elem < ALERT).then(|| (elem - BAND) as usize)
}
pub fn alert_index(elem: u32) -> Option<usize> {
    (elem >= ALERT && elem < ALERT + 2).then(|| (elem - ALERT) as usize)
}

// ── The modal REPEAT CADENCE ─────────────────────────────────────────────────────────────────
//
// **Here, and not in `screens/player/input.rs`, because it is not the player's** (phase 10 merge).
// It lived there while the player's four overlay panels were the only screens that paced their own
// `Edge::Repeat`; the item context menu became a surface in the same phase and needs the identical
// cadence, and a screen may not reach into a sibling family for a shared word (§2.1, and
// `ci/check-deps.sh`'s `sibling` gate). The `app/` side — `App::modal_repeat`, seeded in
// `app/boot.rs` — has always used it too, so "the player's input module" was never an honest home:
// the callers are one surface family, one standalone surface and the loop.

/// Rate-limits a REPEAT-DRIVEN discrete step — a forwarded hardware auto-repeat, or one tick of a
/// scroll-wheel gesture — to a couch-comfortable cadence, independent of the SOURCE's own cadence.
/// A held hardware key repeats roughly every 50ms; a wheel gesture can deliver several ticks in one
/// pass. Settings, Consent and Legal move a whole table row — or, inside a document, a full page of
/// reading text — per step, so letting either source drive `on_updown` at its own rate reads as a
/// blur rather than a scroll: item 13's whole ask.
///
/// Pure and host-testable — no `SDL_GetTicks` inside; `now` is threaded in by the caller, the same
/// shape `HeldKey`'s own `wrapping_sub` timing takes, so it survives the tick wrap the same way.
pub struct RepeatGate {
    /// The tick of the last step this gate admitted; `None` before the first one.
    pub last: Option<u32>,
}
impl RepeatGate {
    /// Minimum time between two repeat-driven steps this gate allows. Slower than the discrete
    /// focus-list repeat (110ms, [`PANEL_REPEAT_MS`]) on purpose — a home-grid card is a glance, a
    /// settings row or a line of reading text is not.
    pub const STEP_MS: u32 = 160;
    pub const IDLE: RepeatGate = RepeatGate { last: None };
    /// True at most once per [`Self::STEP_MS`]; always true the first call, or after a gap at
    /// least that long (which is also what makes a long-idle gate behave like a fresh one).
    pub fn ready(&mut self, now: u32) -> bool {
        self.ready_every(now, Self::STEP_MS)
    }
    /// The same gate at a caller-chosen cadence. The player's overlay surfaces take
    /// [`PANEL_REPEAT_MS`] through this, which is what preserves the exact hold-to-move feel the
    /// loop's own client-side repeat timer gave them before phase 9 moved their input onto the
    /// dispatcher: the remote streams `Edge::Repeat` at roughly 50 ms, and a list walked at that
    /// rate reads as a blur (see [`PANEL_REPEAT_MS`]).
    pub fn ready_every(&mut self, now: u32, step_ms: u32) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => now.wrapping_sub(last) >= step_ms,
        };
        if due {
            self.last = Some(now);
        }
        due
    }
    /// **A fresh press restarts the cadence from itself.** The press is acted on unconditionally
    /// by its own arm — a fresh press is never swallowed by the cadence of the press before it —
    /// and this records it as the step it is, so the FIRST hardware repeat of the new hold waits a
    /// full [`PANEL_REPEAT_MS`] rather than landing on top of it.
    ///
    /// It cleared `last` instead until the surface tests were written, which was wrong in exactly
    /// the direction that is invisible on a fresh gate: the remote streams `Edge::Repeat` at ~50 ms
    /// and the first of them arrives with `last: None`, so a held key stepped TWICE before the
    /// cadence engaged, once for the press and once for the repeat behind it.
    pub fn rearm(&mut self, now: u32) {
        self.last = Some(now);
    }
}

/// **The cadence a held direction walks a player panel's list at**, in ms.
///
/// 110 ms, which is the number `app/run.rs`'s client-side repeat timer used for exactly these four
/// panels (the track menu, the `…` popover, the Info card and the Chapters strip) while their keys
/// went through the loop's own ladder. That timer existed because the Magic Remote streams
/// hardware auto-repeat at ~50 ms and a list walked at that rate is unusable; phase 9 put the
/// panels' input on the dispatcher, where `Edge::Repeat` arrives at the hardware's rate, so the
/// cadence has to be applied by the surface that receives it. Same number, same feel, one owner.
pub const PANEL_REPEAT_MS: u32 = 110;

#[cfg(test)]
mod repeat_gate_tests {
    use super::{RepeatGate, PANEL_REPEAT_MS};

    #[test]
    fn a_gate_admits_the_first_step_then_holds_the_cadence() {
        let mut gate = RepeatGate::IDLE;
        assert!(gate.ready(1_000), "nothing has fired yet");
        assert!(!gate.ready(1_050), "too soon");
        assert!(!gate.ready(1_159), "still short of the step");
        assert!(gate.ready(1_160), "exactly one step later");
        assert!(!gate.ready(1_161));
    }

    /// SDL ticks wrap at 2^32ms; the same arithmetic `HeldKey`'s lost-keyup net and client-side
    /// repeat already rely on, so this gate must survive it the same way.
    #[test]
    fn the_gate_survives_the_tick_wrap() {
        let mut gate = RepeatGate::IDLE;
        let at = u32::MAX - 50;
        assert!(gate.ready(at));
        assert!(!gate.ready(at.wrapping_add(100)));
        assert!(gate.ready(at.wrapping_add(160)));
    }

    /// The player panels' own cadence, and the reason `ready_every` exists: the remote's ~50 ms
    /// hardware repeat is admitted at 110 ms, not at 160 (a settings row) and not at 50.
    #[test]
    fn a_player_panel_walks_a_held_direction_at_its_own_cadence() {
        let mut gate = RepeatGate::IDLE;
        assert!(gate.ready_every(1_000, PANEL_REPEAT_MS));
        assert!(!gate.ready_every(1_050, PANEL_REPEAT_MS), "the hardware's own rate is too fast");
        assert!(gate.ready_every(1_110, PANEL_REPEAT_MS));
        // …and a FRESH press restarts the cadence FROM ITSELF. The press is acted on by its own
        // arm without consulting the gate, so it is never swallowed; what this pins is the other
        // half, which was wrong until the surface tests caught it — the first hardware repeat of
        // the new hold must wait a full cadence rather than landing on top of the press.
        gate.rearm(1_115);
        assert!(!gate.ready_every(1_165, PANEL_REPEAT_MS), "the repeat behind a fresh press waits");
        assert!(gate.ready_every(1_225, PANEL_REPEAT_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A row index stays itself under `band_elem`/`band_index` for the whole practical range of
    /// an action row (never more than two controls in this family today, but the round trip is
    /// asserted well past that so a widened band does not silently wrap into the alert's space).
    #[test]
    fn band_elem_and_band_index_round_trip() {
        for i in 0..64usize {
            let e = band_elem(i);
            assert!(e >= BAND && e < ALERT, "band_elem({i}) = {e:#x} left the band's own range");
            assert_eq!(band_index(e), Some(i));
        }
    }

    /// A raw table-row index (always far below [`BAND`]) is never mistaken for a band or alert
    /// control — the three ranges the family's `u32` namespace is carved into must not overlap.
    #[test]
    fn a_table_row_index_is_neither_a_band_nor_an_alert_element() {
        for row in [0u32, 1, 2, 41, 4095] {
            assert_eq!(band_index(row), None, "row {row} must not resolve as a band control");
            assert_eq!(alert_index(row), None, "row {row} must not resolve as an alert answer");
        }
    }

    /// The alert's two answers (Cancel, Delete) are the only two elements in its range, and the
    /// band's own top control does not spill into it.
    #[test]
    fn the_alert_range_holds_exactly_two_answers_just_above_the_band() {
        assert_eq!(alert_index(ALERT), Some(0));
        assert_eq!(alert_index(ALERT + 1), Some(1));
        assert_eq!(alert_index(ALERT + 2), None, "the alert's range is exactly two elements wide");
        assert_eq!(band_index(ALERT - 1), Some((ALERT - 1 - BAND) as usize), "the band's range runs right up to the alert's");
        assert_eq!(band_index(ALERT), None, "…and stops there — the two ranges must not overlap");
    }
}

// ---------------------------------------------------------------------------------------------
// the page alphabet, the screen argument and the one mount match
// ---------------------------------------------------------------------------------------------

/// **A screen argument: everything the container needs to mount one screen, and nothing else.**
#[derive(Clone, PartialEq, Eq)]
pub enum AppArg {
    /// The item context menu (`screens::item_menu`), presented on the shared `ModalStack` over the
    /// library page whose card was held.
    ItemMenu(ItemMenuArg),
    /// One of the player's panels, on the player page's own `ModalStack`.
    PlayerOverlay(crate::player::overlay::PlayerOverlayArg),
    /// Playback. Its panels are entries on the player page's own `ModalStack`
    /// ([`Self::PlayerOverlay`]), and the container owns which one is up.
    Player,
    /// One folder of the local library. The path is the folder's, empty for the library's top
    /// level.
    Local(LocalArg),
}

/// The argument of [`AppArg::Local`]: which folder the page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalArg {
    pub path: String,
}

pub const ARG_SHAPE: &str = "AppArg{ItemMenu{path:str,mark:u32,host:u32,focus:Option<{entry:u32,elem:u32}>,anchor:[u32;4]},\
     PlayerOverlay{Tracks(tab:i32),Info,Chapters,More(quality:bool)},Player,Local{path:str}}";

impl LogicalState for AppArg {
    fn write(&self, c: &mut Canon) {
        match self {
            Self::PlayerOverlay(arg) => { c.u32(5); arg.write(c); }
            Self::ItemMenu(arg) => { c.u32(9); arg.write(c); }
            Self::Player => { c.u32(0).u32(10); }
            Self::Local(arg) => { c.u32(13).str(&arg.path); }
        }
    }
    fn probe(&self, out: &mut String) { out.push_str("app_arg"); }
}

impl plx_ui::screen::ScreenArg for AppArg {
    fn chrome(&self) -> Chrome {
        Chrome::None
    }
    fn id(&self) -> ScreenId {
        ScreenId(match self {
            // One id for every player panel: they are kinds of ONE screen, and `same_instance`
            // must never let the container think it is holding two of them.
            AppArg::PlayerOverlay(_) => 16,
            AppArg::ItemMenu(_) => 20,
            AppArg::Player => 11,
            // Every folder page is one SCREEN; which folder is its argument.
            AppArg::Local(_) => 25,
        })
    }
    fn title(&self) -> Option<&str> {
        None
    }
    fn same_instance(&self, other: &Self) -> bool {
        if let (Self::Local(a), Self::Local(b)) = (self, other) {
            return a == b;
        }
        // **A player overlay's identity is its KIND, never the playback under it**: the panel is
        // a surface on the player's own stack, so a BACK out of the track menu dismisses a surface
        // instead of remounting the page.
        if let (Self::PlayerOverlay(a), Self::PlayerOverlay(b)) = (self, other) {
            return a.kind.slot() == b.kind.slot();
        }
        <Self as plx_ui::screen::ScreenArg>::id(self) == <Self as plx_ui::screen::ScreenArg>::id(other)
    }
}

#[derive(Default)]
pub struct AppMounter {
    /// **Where the NEXT player instance returns to** — the entry that was on top when the push was
    /// asked for, stamped at the press and consumed by the mount.
    pub player_origin: Option<crate::player::Origin>,
    /// How long the NEXT player instance pins its transport for, in ms. A seed because the
    /// deadline must be stamped from the instant the page MOUNTS, not from the keypress that asked
    /// for the playback: a slow open would otherwise expire the HUD before it was ever drawn.
    pub player_hud_ms: Option<u32>,
}

/// **The one `mount` match** — generic over the host, so the mounter names no application type.
impl<H> Mounter<H> for AppMounter
where
    H: plx_machine::machine::Host<Arg = AppArg> + PlayerLike + MetadataLike,
{
    fn mount(
        &mut self,
        id: InstanceId,
        arg: &AppArg,
        _ret: &ReturnState<u32, PageMemory>,
        cx: &Cx<'_, H>,
        _fx: &mut Effects<'_, H>,
    ) -> Box<dyn Screen<H>> {
        let entry = match cx.owner {
            plx_machine::machine::InputOwner::Entry(e) => e,
            _ => EntryId(0),
        };
        match arg {
            AppArg::ItemMenu(arg) => Box::new(crate::item_menu::ItemMenuScreen::new(entry, arg.clone())),
            AppArg::PlayerOverlay(arg) => Box::new(
                crate::player::overlay::PlayerOverlayScreen::new_with(
                    H::session(cx), H::metadata(cx), entry, arg.kind,
                    // The search asks the playing item's SERVER; a local file has none.
                    H::subtitle_search(cx).is_some() && !plx_media::localfile::is_file_url(&plx_media::route::url(H::session(cx)))),
            ),
            AppArg::Player => {
                let mut page = crate::player::PlayerScreen::new(entry);
                // **Where BACK, Stop and EOS land.** Captured at the MOUNT, from the page that was
                // on top when the push was asked for — never read live, because an overlay
                // opening on this page's own stack changes the input owner and must not be
                // mistaken for a new origin.
                page.origin = self.player_origin.take();
                page.hud.extend(cx.tick.ms, self.player_hud_ms.take().unwrap_or(crate::player::input::HUD_LINGER_MS));
                page.publish();
                Box::new(page)
            }
            AppArg::Local(arg) => Box::new(crate::local::LocalScreen::new(entry, id, arg)),
        }
    }
}

/// **Every `AppArg` a SURFACE can be presented with, one per variant** — the domain the heartbeat's
/// ` overlay=` alphabet is derived over. Exhaustive by the compiler: the `match` names every
/// variant, so adding one and forgetting it here does not compile.
#[cfg(any(test, feature = "test-support"))]
pub fn every_surface_arg() -> Vec<AppArg> {
    use crate::player::overlay::{OverlayKind, PlayerOverlayArg};
    let mut args = vec![AppArg::ItemMenu(ItemMenuArg {
        path: "/media/a.mkv".into(),
        mark: plx_ui::widgets::PosterMark::None,
        host: EntryId(0),
        focus: None,
        anchor: [0; 4],
    })];
    // The player's panels are ONE screen with several kinds, and each answers a different
    // `Screen::name` — so every kind is listed, not one representative.
    args.extend(OverlayKind::ALL.map(|kind| AppArg::PlayerOverlay(PlayerOverlayArg { kind })));
    for a in &args {
        match a {
            AppArg::ItemMenu(_) | AppArg::PlayerOverlay(_) => {}
            AppArg::Player | AppArg::Local(_) => panic!("a page argument is not a surface: its word is `route=`"),
        }
    }
    args
}

/// **Every SCREEN-side shape the recorder's `state_fp` hashes**, as one array in the module a new
/// screen is added to. Order is part of the hash, so an entry is APPENDED.
pub const SCREEN_SHAPES: &[&str] = &[
    ARG_SHAPE,
    PAGE_MEMORY_SHAPE,
    crate::item_menu::SHAPE,
    crate::player::SHAPE,
    crate::player::overlay::SHAPE,
    crate::local::LocalScreen::SHAPE,
];

#[cfg(test)]
mod arg_tests {
    use super::*;

    /// **One `ScreenId` per surface VARIANT.** `same_instance` is derived from `id`, so two
    /// different variants sharing one would let the container hold a mounted instance of the
    /// wrong screen and believe it was reusing the right one.
    #[test]
    fn two_different_surface_variants_never_share_a_screen_id() {
        use plx_ui::screen::ScreenArg;
        let args = every_surface_arg();
        for (i, a) in args.iter().enumerate() {
            for b in args.iter().skip(i + 1) {
                if a.id() == b.id() {
                    assert_eq!(std::mem::discriminant(a), std::mem::discriminant(b),
                        "two different surfaces share ScreenId({})", a.id().0);
                }
            }
        }
    }
}
