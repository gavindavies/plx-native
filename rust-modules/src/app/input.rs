//! The key ladder's arm bodies and the pointer state: what a fresh press does before any screen
//! sees it, OK and BACK on the player page, and the pointer machine. Every owned page and surface
//! takes its keys as `InputEvent`s the loop hands the dispatcher before this ladder is consulted.

use super::*;


/// The Magic Remote POINTER, as one value: which input mode the remote is in, what the
/// cursor is doing, and the two gestures that outlive a single event (a scrub drag and the
/// wheel's own debounce).
///
/// `dpad_mode`/`cur_hidden`/`mot_accum` are one rule between them and are why this is a
/// type: the first D-pad press hides the cursor and switches modes, and motion only switches
/// back once it has accumulated past the gate (see `remote_synth_ptr`, which has to defeat
/// that gate to click at all).
pub(crate) struct Pointer {
    pub(crate) dpad_mode: bool,  // D-pad input owns focus; pointer motion below the gate is ignored
    pub(crate) cur_hidden: bool, // the LG cursor is hidden right now
    pub(crate) mot_accum: f32,   // motion accumulated since D-pad mode was entered, in logical px
    pub(crate) prev_mx: f32,     // last motion's position, for that accumulation (-1 = none yet)
    pub(crate) prev_my: f32,
    pub(crate) last_motion: u32, // last motion tick — playback hides an idle cursor off this
    /// **A pointer button is DOWN right now** — so this motion is a DRAG (§7.5), and the idle
    /// cursor is not hidden out from under a hand that is holding something.
    ///
    /// It was `drag`, "a click is dragging the HUD scrub band", and it meant the player's scrub
    /// gesture: `app/run.rs`'s own click block set it, its motion arm moved the preview and its
    /// button-up committed the seek. That gesture belongs to `PlayerScreen` (restructure phase 12,
    /// PX-PLAYER), so the flag went with it (`screens::player::input::Scrub::drag`) and what is
    /// left here is the pointer-machine FACT the loop still has to know: the button's state, which
    /// decides whether a motion event is dispatched as `InputKind::Pointer` or `InputKind::Drag`.
    pub(crate) button_down: bool,
    pub(crate) last_wheel: u32,  // last wheel tick, for the wheel's own debounce
}
impl Pointer {
    /// Pointer mode, cursor shown, nothing held or dragging — where the loop starts.
    pub(crate) const IDLE: Pointer = Pointer {
        dpad_mode: false,
        cur_hidden: false,
        mot_accum: 0.0,
        prev_mx: -1.0,
        prev_my: -1.0,
        last_motion: 0,
        button_down: false,
        last_wheel: 0,
    };
}

//
// The run loop's key handler is a LADDER: a key-up, a hardware auto-repeat and the preamble every
// fresh press runs; then the DISPATCHER's arm and the route-scoped ones after it, each
// `continue`ing; then one chained `else if` on key identity. Each arm's BODY is a function here,
// in the order the ladder tries them — bar the ones with no body to name (the dispatcher's arm is
// one `app.inputs.push`, the pointer-hidden arm is empty). No count is given, deliberately: three
// arms left this ladder in phase 5b alone and every remaining player arm left it in phase 12, and
// a number here rots without anything failing.
//
// Every guard, every `continue` and the order itself stay at the CALL SITE, because the order is
// part of the behaviour: an earlier guard subsumes later ones it overlaps with, and that is only
// legible while the tests sit in one list, in order, in one place. (The clearest example used to
// be `key_player_failed`, whose guard deliberately subsumed every player arm below it; the whole
// group is `PlayerScreen`'s since phase 12, where the same precedence is the FIRST test in
// `handle_key` rather than the height of an arm in a chain.)
//
// No host test executes any of this: it runs inside the SDL event loop. The gate over it is
// `tools/keytable.py`, which drives the simulator through (screen x key) and diffs the focus
// fingerprint each press produces against a recorded table.

/// A key-up: the reliable release (this remote sends exactly one per press). Retires this sym from
/// the physically-down slot and springs a deferred grid-card press back.
///
/// It cleared a second slot — `HeldKey::sym`, the client-side hold-repeat's own — until phase 10
/// deleted that timer with its last consumer (the item menu). What is left is `down_sym`, which is
/// a fact about the PHYSICAL key rather than about whoever read it, and the press machine's
/// release, which is a fact about the LOOP's own deferred press. **Both are why this still runs
/// unconditionally, above the tree-ownership question**: a key the ladders never saw go down would
/// otherwise look held for as long as a surface was up.
///
/// **The scrub half is gone** (restructure phase 12, PX-PLAYER). It ran on
/// `PlayerScreen::scrub` — a field of a screen that has owned the whole gesture since it began
/// answering `HitSource::Engine`, and which arms its own `TAP_COMMIT_MS` debounce on
/// `Edge::Up` — so the two together issued TWO seeks for one tap. The commit, the debounce and
/// the reveal-cancel are `screens::player::PlayerScreen::key_scrub_release`'s now, and
/// `repause_at` reaches `commit_seek` through `PlayerReq::CommitSeek` instead of through here.
pub(crate) fn on_key_up(
    sym: c_uint,
    ok_armed: bool,
    down_sym: &mut u32,
    press: &mut plx_ui::press::Press,
) {
    if sym == *down_sym {
        *down_sym = 0;
    }
    if is_ok(sym) && ok_armed {
        // OK released over a grid card: start the spring-back; the deferred
        // activation commits from the per-frame loop once the bounce has shown.
        press.release(clock::now());
    }
}

/// A hardware AUTO-REPEAT (held key). **All that is left of it is the deferred press's liveness
/// beat** — the dropped-key-up net in `ui::press`, which is the loop's own machine and belongs to
/// no screen.
///
/// The client-side hold-repeat timer went in phase 10: every discrete focus list in the app — the
/// home grid, detail, the Settings family, the player's four panels and, last of them, the item
/// context menu — is an owned screen or a surface on the dispatcher, so a repeat reaching one is
/// an `InputEvent` carrying `Edge::Repeat` handed over before this function is reached, and the
/// cadence is applied THERE (`RepeatGate`, to the DIRECTIONS only).
///
/// The player's continuous scrub was the last thing this drove directly, and it went the same way
/// in phase 12 (PX-PLAYER): `PlayerScreen::key_scrub_repeat` engages the ramp off its own
/// `Edge::Repeat`, which is the same event one layer down, and the per-frame advance is that
/// screen's `Tick`. Nothing about the gesture is read or written from here any more, which is what
/// makes `PlayerScreen::scrub` a field with ONE owner.
pub(crate) fn on_auto_repeat(sym: c_uint, ok_armed: bool, press: &mut plx_ui::press::Press) {
    if ok_armed && is_ok(sym) {
        press.note_alive(clock::now()); // OK held: keep the dropped-key-up net honest
    }
}

/// What EVERY fresh press does before the ladder sees it: remember the sym as physically down,
/// un-dismiss the HUD, abort an armed click that a non-OK key slid off, and — the LG pointer
/// convention, global to every screen including the onboarding ones the ladder dispatches first —
/// let the first D-pad press dismiss the Magic-Remote cursor and put input in D-pad mode. Pointer
/// motion brings it back.
///
/// The cursor gate takes the plain syms only (`alt: false`), which is exactly the set the four
/// spelled-out `sym ==` comparisons here took. Whether the alternate D-pad codes BELONG in it is an
/// open behavioural question — the Chapters strip accepts them and does not hide the cursor — and
/// naming the identity did not settle it.
///
/// # The unsupported-key invariant (LG checklist item 40)
///
/// **This function runs BEFORE the ladder has decided whether anything takes the press**, and two
/// of the things it does are GLOBAL rather than local to an arm: un-dismissing the player HUD, and
/// aborting a tvOS click in flight. So until 2026-08-23 an unsupported key — a colour button,
/// GUIDE, INFO, a universal remote's extra half — raised the transport over playback and cancelled
/// a press the user was in the middle of, and neither is a thing "the app ignored that key" is
/// allowed to mean. Both are now gated on [`is_bound`], whose doc carries the whole map and the one
/// place it deliberately over-approximates.
///
/// **The invariant is about CONSUMPTION, not about [`Key::Other`].** `Other` is a legitimate
/// identity for real, handled keys — the Library pager (a separate `page_dir` predicate) and
/// Search's Backspace/Clear both classify as `Other` and are then taken by hand — so making the
/// variant inert would break all of them. `is_bound` is the superset that answers the actual
/// question.
///
/// Three things stay UNCONDITIONAL and each for its own reason. `down_sym` is bookkeeping
/// about the physical key, not a side effect: without it a held unsupported key's auto-repeats
/// would each arrive as a fresh press (`state & 0x100 != 0 && sym == app.down_sym` in the
/// caller). The D-pad cursor gate is already narrower than `is_bound` — it takes the four plain
/// direction syms and nothing else — so it needs no second guard. And the caller's `last_input`
/// stamp is a local read only by arms that run in the same iteration, so an unbound press cannot
/// carry it anywhere.
pub(crate) unsafe fn begin_fresh_press(
    ps: &plx_media::playback::PlaybackSession,
    key: Key,
    sym: c_uint,
    wcode: c_uint,
    now: u32,
    down_sym: &mut u32,
    hud: Option<&mut HudState>,
    ptr: &mut Pointer,
    ok_armed: &mut bool,
    press: &mut plx_ui::press::Press,
) {
    *down_sym = sym;
    note_global_press(ps, sym, wcode, now, hud, ok_armed, press);
    if matches!(
        key,
        Key::Up | Key::Down | Key::Left { alt: false } | Key::Right { alt: false }
    ) {
        ptr.dpad_mode = true;
        ptr.cur_hidden = true;
        ptr.mot_accum = 0.0;
    }
}

/// **The two GLOBAL effects of a fresh press, and the guard that decides whether they happen** —
/// the half of [`begin_fresh_press`] that LG checklist item 40 is about, and its only caller.
///
/// Split out for one blunt reason: `begin_fresh_press` calls `hide_cursor`, which names a
/// webOS-only SDL symbol, so a host test that reaches it fails at `ld` rather than at an assertion
/// (the boundary the testing section of `docs/agent-reference.md` describes — the crate links today only
/// because nothing reachable from a test calls it and the linker dead-strips it). This half touches
/// no SDL at all, so the invariant is gradeable by `make check` instead of only by a television.
pub(crate) fn note_global_press(
    ps: &plx_media::playback::PlaybackSession,
    sym: c_uint,
    wcode: c_uint,
    now: u32,
    hud: Option<&mut HudState>,
    ok_armed: &mut bool,
    press: &mut plx_ui::press::Press,
) {
    if !is_bound(sym, wcode) {
        return; // an unsupported key is not input the app acted on — see `begin_fresh_press`
    }
    // What the user could SEE, and only then the un-dismiss — one operation, because the order is
    // load-bearing (`HudState::note_fresh_press`). Taken for every BOUND key on every screen: it is
    // one cheap predicate, and the alternative is each player arm remembering to ask first, which
    // is exactly the ordering the pointer path had to be fixed for once already.
    if let Some(hud) = hud {
        hud.note_fresh_press(ps, now, paused());
    }
    // a fresh non-OK key (navigation / BACK) while a click is armed aborts the press — spring the
    // card back to rest WITHOUT activating (you "slid off" the control). A key the app does not
    // bind is not sliding off anything: nothing moved, so nothing is abandoned.
    if *ok_armed && !is_ok(sym) {
        press.cancel();
        *ok_armed = false;
    }
}

/// **The unsupported-key invariant, graded.** `tools/keytable.py` grades the other half — that an
/// unbound press moves no focus on any screen — and cannot see either of these two, because neither
/// appears in a focus fingerprint.
#[cfg(test)]
mod unsupported_key_tests {
    use super::*;

    /// Drive one fresh press through [`note_global_press`] and report what it left behind:
    /// `(a click is still armed, the HUD is still dismissed)`. `press::*`, `hud_until()` and
    /// `hud_until()` and `paused()` are crate globals, so every caller holds `testlock::serial()`;
    /// the press is the test's own `Press`.
    fn press(ps: &plx_media::playback::PlaybackSession, sym: c_uint, wcode: c_uint) -> (bool, bool) {
        let mut hud = HudState::IDLE;
        let mut ok_armed = true; // a click is in flight, as if OK were still down on a card
        hud.dismissed = true; // …and the transport was hidden by hand (UP from the control row)
        let mut p = plx_ui::press::Press::new();
        p.begin(1_000);
        note_global_press(ps, sym, wcode, 1_000, Some(&mut hud), &mut ok_armed, &mut p);
        let out = (p.is_active() && ok_armed, hud.dismissed);
        p.cancel();
        out
    }

    /// A key the app binds behaves exactly as it always has: it un-dismisses the HUD and aborts the
    /// click it slid off. BACK is the case to use — it is not OK, so it takes the abort branch.
    #[test]
    fn a_bound_key_still_wakes_the_hud_and_aborts_the_click() {
        let ps = plx_media::playback::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        let (armed, dismissed) = press(&ps, SDLK_ESCAPE, 0);
        assert!(
            !armed,
            "BACK slides off the control — the press is cancelled"
        );
        assert!(!dismissed, "…and any key un-dismisses the transport");
    }

    /// **The regression.** An unsupported key must do NEITHER — it is not input the app acted on,
    /// so it may not raise the transport over playback and it may not abandon a press in flight.
    /// 269 is HOME (`SDL_SCANCODE_AC_HOME`, evdev 172 `KEY_HOMEPAGE`); every other unbound
    /// scancode takes the same branch.
    #[test]
    fn an_unsupported_key_wakes_nothing_and_abandons_nothing() {
        let ps = plx_media::playback::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        for (sym, wcode, what) in [
            (0, 269, "HOME"),
            (0, 270, "AC_BACK"),
            (b'a' as c_uint, 4, "a letter"),
        ] {
            let (armed, dismissed) = press(&ps, sym, wcode);
            assert!(armed, "{what} must not cancel the armed click");
            assert!(dismissed, "{what} must not un-dismiss the HUD");
        }
    }

    /// The digits are the one place [`is_bound`] deliberately over-approximates (its doc argues
    /// it): they count as bound everywhere. Pinned so the trade-off stays a decision on record
    /// rather than something a reader finds.
    #[test]
    fn a_number_key_counts_as_bound() {
        let ps = plx_media::playback::PlaybackSession::IDLE;
        let _g = plx_base::testlock::serial();
        let (armed, dismissed) = press(&ps, b'5' as c_uint, 34);
        assert!(!armed);
        assert!(!dismissed);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn key_ok(
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    now: u32,
    _ptr: &mut Pointer,
    ok_armed: &mut bool,
    press: &mut plx_ui::press::Press,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    if super::bridge::player(pages).is_some() {
        // The cursor and the pre-press visibility are the mounted screen's, read ONCE into copies
        // so the arms below are free to present a panel on the same container (`open_player_overlay`
        // takes it mutably). The pre-press sample is the one the other player arms take too:
        // `begin_fresh_press` has already cleared `dismissed`, so re-asking calls a hand-hidden
        // transport visible and this arm would open a panel from behind it
        // (`HudState::visible_at_press`).
        let Some((vis, focus)) = super::bridge::player(pages)
            .map(|player| (player.hud.visible_at_press, player.hud.nav.focus))
        else {
            return;
        };
        // Row 1 is the transport's TOP MENU — Info, Chapters, Subtitles, Audio. Every item is a
        // control FACE with a pop of its own (`player_hud::RowPop`), so OK takes the tvOS press:
        // dip now, act on the spring-back, in `activate_player_row` from the per-frame loop. Every
        // arm opens a panel OVER this HUD rather than leaving the route, which makes this the one
        // control row in the app where the whole dip → ring is on screen either side of it.
        if vis && focus == 1 {
            press.begin_ctl(now);
            *ok_armed = true;
        } else {
            let np = !super::lifecycle::viewer_paused();
            if np {
                set_transport_paused(pa, true);
            } else {
                set_transport_paused(pa, false);
            }
        }
        if let Some(player) = super::bridge::player_mut(pages) {
            player.hud.extend(now, HUD_LINGER_MS);
            player.publish();
        }
    }
}

/// BACK on the player: leave it. Every other page answers its own BACK (an owned screen), so this
/// is reached only while the player is on top or a transition is still withdrawable.
///
/// A BACK inside the page fade's window WITHDRAWS the transition rather than acting on a screen
/// that is already half gone: the request is at most four frames old and nothing has changed yet,
/// so it can still be un-asked. `nav_cancel` refuses once the swap has happened, and then this is
/// an ordinary BACK on the NEW screen — the press is never dropped, only ever spent on exactly one
/// of the two.
pub(crate) fn key_back(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    if super::bridge::nav_cancel(pages) {
    } else if super::bridge::player(pages).is_some() {
        exit_player(ps, pa, pages);
    }
}
